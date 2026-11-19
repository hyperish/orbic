//! `orbic::db` — Dual-track database layer (`#[table]` typed chaining + offline-verified `sql!` macro)
//! with closure-based transactions and cancellation-triggered rollback.

use hyve::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Scalar parameter value stored inside `orbic::db` rows and SQL parameters.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    /// UTF-8 string value (including ULIDs and typed IDs).
    Text(String),
    /// Signed 64-bit integer value.
    I64(i64),
    /// 64-bit floating-point value.
    F64(f64),
    /// Boolean value.
    Bool(bool),
    /// SQL `NULL`.
    Null,
}

impl SqlParam {
    /// Formats the parameter cleanly for primary key indexing and cursor comparison.
    #[must_use]
    pub fn as_key_string(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::I64(i) => i.to_string(),
            Self::F64(f) => f.to_string(),
            Self::Bool(b) => b.to_string(),
            Self::Null => String::new(),
        }
    }
}

/// Converts typed Rust values into [`SqlParam`].
pub trait IntoSqlParam {
    /// Converts `self` into a [`SqlParam`].
    fn into_sql_param(self) -> SqlParam;
}

/// Extracts typed Rust values from a SQL row column map.
pub trait FromSqlParam: Sized {
    /// Decodes a named column from `row`.
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self>;
}

fn find_col<'a>(row: &'a [(&'static str, SqlParam)], col: &str) -> Option<&'a SqlParam> {
    row.iter().find(|(k, _)| *k == col).map(|(_, v)| v)
}

impl IntoSqlParam for String {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::Text(self)
    }
}
impl IntoSqlParam for &str {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::Text(self.to_owned())
    }
}
impl FromSqlParam for String {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        match find_col(row, col) {
            Some(SqlParam::Text(s)) => Ok(s.clone()),
            _ => Err(HyveError::internal(format!(
                "missing or invalid text column `{col}`"
            ))),
        }
    }
}

impl IntoSqlParam for bool {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::Bool(self)
    }
}
impl FromSqlParam for bool {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        match find_col(row, col) {
            Some(SqlParam::Bool(b)) => Ok(*b),
            _ => Err(HyveError::internal(format!(
                "missing or invalid bool column `{col}`"
            ))),
        }
    }
}

impl IntoSqlParam for i64 {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::I64(self)
    }
}
impl FromSqlParam for i64 {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        match find_col(row, col) {
            Some(SqlParam::I64(v)) => Ok(*v),
            _ => Err(HyveError::internal(format!(
                "missing or invalid i64 column `{col}`"
            ))),
        }
    }
}

impl IntoSqlParam for i32 {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::I64(i64::from(self))
    }
}
impl FromSqlParam for i32 {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        let v = i64::from_sql_row(row, col)?;
        Self::try_from(v).map_err(|_| HyveError::internal("i32 out of range"))
    }
}

impl IntoSqlParam for u32 {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::I64(i64::from(self))
    }
}
impl FromSqlParam for u32 {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        let v = i64::from_sql_row(row, col)?;
        Self::try_from(v).map_err(|_| HyveError::internal("u32 out of range"))
    }
}

impl IntoSqlParam for u64 {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::I64(self as i64)
    }
}
impl FromSqlParam for u64 {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        let v = i64::from_sql_row(row, col)?;
        Ok(v as u64)
    }
}

impl IntoSqlParam for f64 {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::F64(self)
    }
}
impl FromSqlParam for f64 {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        match find_col(row, col) {
            Some(SqlParam::F64(f)) => Ok(*f),
            _ => Err(HyveError::internal(format!(
                "missing or invalid f64 column `{col}`"
            ))),
        }
    }
}

impl<T: ?Sized> IntoSqlParam for Id<T> {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::Text(self.as_str().to_owned())
    }
}
impl<T: ?Sized> FromSqlParam for Id<T> {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        let s = String::from_sql_row(row, col)?;
        Ok(Self::from_raw(s))
    }
}

impl IntoSqlParam for Timestamp {
    fn into_sql_param(self) -> SqlParam {
        SqlParam::I64(self.as_millis())
    }
}
impl FromSqlParam for Timestamp {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        Ok(Self::from_millis(i64::from_sql_row(row, col)?))
    }
}

impl<T: IntoSqlParam> IntoSqlParam for Option<T> {
    fn into_sql_param(self) -> SqlParam {
        match self {
            Some(v) => v.into_sql_param(),
            None => SqlParam::Null,
        }
    }
}
impl<T: FromSqlParam> FromSqlParam for Option<T> {
    fn from_sql_row(row: &[(&'static str, SqlParam)], col: &'static str) -> HyveResult<Self> {
        match find_col(row, col) {
            None | Some(SqlParam::Null) => Ok(None),
            Some(_) => T::from_sql_row(row, col).map(Some),
        }
    }
}

/// Compile-time typed column descriptor emitted on `#[table]` structs (`Project::OWNER_ID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column<M, V> {
    name: &'static str,
    _marker: PhantomData<fn() -> (M, V)>,
}

impl<M, V> Column<M, V> {
    /// Constructs a static typed column constant.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _marker: PhantomData,
        }
    }

    /// Returns the underlying SQL column name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

/// Type-checks a `sql!` parameter against its exact [`Column`] type `V`.
pub fn bind_column<M, V: IntoSqlParam>(_col: &Column<M, V>, value: V) -> SqlParam {
    value.into_sql_param()
}

/// Trait binding a `#[model]` + `#[table("...")]` struct to relational table rows.
pub trait TableModel: Sized + Clone + HasSchema {
    /// Underlying SQL table name.
    fn table_name() -> &'static str;
    /// Primary key column name.
    fn pk_field() -> &'static str;
    /// Stringified primary key value of `self`.
    fn pk_value(&self) -> String;
    /// Returns current optimistic-locking `version` if present.
    fn version_of(&self) -> Option<i64> {
        None
    }
    /// Updates optimistic-locking `version` if present.
    fn set_version(&mut self, _v: i64) {}
    /// Converts `self` into column-value pairs.
    fn to_sql_row(&self) -> Vec<(&'static str, SqlParam)>;
    /// Reconstructs `Self` from stored column-value pairs.
    fn from_sql_row(row: &[(&'static str, SqlParam)]) -> HyveResult<Self>;
}

/// Cursor pagination metadata following `docs/03-api-protocol.md` §4-§5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMeta {
    /// Opaque cursor pointing to the last returned item, if any.
    pub next_cursor: Option<String>,
    /// True if more items exist beyond this page.
    pub has_more: bool,
}

impl ToJson for PageMeta {
    fn to_json(&self) -> JsonValue {
        JsonValue::Object(vec![
            ("nextCursor".to_owned(), self.next_cursor.to_json()),
            ("hasMore".to_owned(), JsonValue::Bool(self.has_more)),
        ])
    }
}
impl FromJson for PageMeta {
    fn from_json(val: &JsonValue) -> HyveResult<Self> {
        Ok(Self {
            next_cursor: match val.get("nextCursor") {
                Some(v) => Option::<String>::from_json(v)?,
                None => None,
            },
            has_more: val
                .get("hasMore")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Cursor-paginated collection wrapper returning `{ "data": [...], "page": { ... } }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    /// Returned page items.
    pub data: Vec<T>,
    /// Cursor pagination metadata.
    pub page: PageMeta,
}

impl<T: BinaryEncode> BinaryEncode for Page<T> {
    fn binary_encode(&self, out: &mut Vec<u8>) {
        self.data.binary_encode(out);
        self.page.next_cursor.binary_encode(out);
        self.page.has_more.binary_encode(out);
    }
}
impl<T: BinaryDecode> BinaryDecode for Page<T> {
    fn binary_decode(reader: &mut hyve::serde::BinaryReader<'_>) -> HyveResult<Self> {
        let data = Vec::<T>::binary_decode(reader)?;
        let next_cursor = Option::<String>::binary_decode(reader)?;
        let has_more = bool::binary_decode(reader)?;
        Ok(Self {
            data,
            page: PageMeta {
                next_cursor,
                has_more,
            },
        })
    }
}
impl<T: ToJson> ToJson for Page<T> {
    fn to_json(&self) -> JsonValue {
        JsonValue::Object(vec![
            ("data".to_owned(), self.data.to_json()),
            ("page".to_owned(), self.page.to_json()),
        ])
    }
}
impl<T: FromJson> FromJson for Page<T> {
    fn from_json(val: &JsonValue) -> HyveResult<Self> {
        let data = match val.get("data") {
            Some(v) => Vec::<T>::from_json(v)?,
            None => Vec::new(),
        };
        let page = match val.get("page") {
            Some(v) => PageMeta::from_json(v)?,
            None => PageMeta {
                next_cursor: None,
                has_more: false,
            },
        };
        Ok(Self { data, page })
    }
}
impl<T: HasSchema> HasSchema for Page<T> {
    fn schema() -> ModelSchema {
        T::schema()
    }
}

type StoredRow = Vec<(String, SqlParam)>;

#[derive(Debug, Clone, Default)]
struct DbState {
    tables: HashMap<String, BTreeMap<String, StoredRow>>,
}

/// Transactional in-memory database backing Track 1 chaining and Track 2 `sql!`.
#[derive(Debug, Clone)]
pub struct Db {
    state: Arc<Mutex<DbState>>,
}

/// Active database transaction handle passed into `db.tx(|tx| ...)` closures.
#[derive(Debug, Clone)]
pub struct Tx {
    state: Arc<Mutex<DbState>>,
}

impl Db {
    /// Creates an isolated in-memory database suitable for tests and prototyping.
    pub async fn in_memory() -> Self {
        Self {
            state: Arc::new(Mutex::new(DbState::default())),
        }
    }

    /// Executes `f` inside a transaction that commits on `Ok` and rolls back on `Err`.
    pub async fn tx<T, F, Fut>(&self, f: F) -> HyveResult<T>
    where
        F: FnOnce(Tx) -> Fut,
        Fut: Future<Output = HyveResult<T>>,
    {
        self.tx_with_cancel(CancellationToken::new(), f).await
    }

    /// Executes `f` inside a transaction bound to `token`, rolling back automatically on cancel or error.
    pub async fn tx_with_cancel<T, F, Fut>(&self, token: CancellationToken, f: F) -> HyveResult<T>
    where
        F: FnOnce(Tx) -> Fut,
        Fut: Future<Output = HyveResult<T>>,
    {
        token.check()?;
        let snapshot = self
            .state
            .lock()
            .map_err(|_| HyveError::internal("db mutex poisoned"))?
            .clone();

        let tx_handle = Tx {
            state: Arc::clone(&self.state),
        };
        let result = f(tx_handle).await;
        if result.is_err() || token.is_cancelled() {
            if let Ok(mut guard) = self.state.lock() {
                *guard = snapshot;
            }
            token.check()?;
        }
        result
    }

    /// Inserts a model row cleanly into its `#[table]` store.
    pub async fn insert<M: TableModel>(&self, model: M) -> HyveResult<M> {
        insert_row(&self.state, model)
    }

    /// Fetches a model row by primary key.
    pub async fn get<M: TableModel>(&self, id: &Id<M>) -> HyveResult<Option<M>> {
        let guard = self
            .state
            .lock()
            .map_err(|_| HyveError::internal("db mutex poisoned"))?;
        let Some(tbl) = guard.tables.get(M::table_name()) else {
            return Ok(None);
        };
        let Some(row) = tbl.get(id.as_str()) else {
            return Ok(None);
        };
        let schema = M::schema();
        let pairs: Vec<(&'static str, SqlParam)> = row
            .iter()
            .filter_map(|(k, v)| {
                schema
                    .fields
                    .iter()
                    .find(|f| f.name == k)
                    .map(|f| (f.name, v.clone()))
            })
            .collect();
        M::from_sql_row(&pairs).map(Some)
    }

    /// Updates a versioned model with optimistic locking (`409 version_conflict` on mismatch).
    pub async fn update_optimistic<M: TableModel>(
        &self,
        id: &Id<M>,
        expected_version: i64,
        updater: impl FnOnce(&mut M),
    ) -> HyveResult<M> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| HyveError::internal("db mutex poisoned"))?;
        let tbl = guard.tables.entry(M::table_name().to_owned()).or_default();
        let Some(raw) = tbl.get(id.as_str()).cloned() else {
            return Err(HyveError::not_found(M::table_name(), "record not found"));
        };
        let schema = M::schema();
        let pairs: Vec<(&'static str, SqlParam)> = raw
            .iter()
            .filter_map(|(k, v)| {
                schema
                    .fields
                    .iter()
                    .find(|f| f.name == k)
                    .map(|f| (f.name, v.clone()))
            })
            .collect();
        let mut model = M::from_sql_row(&pairs)?;
        let current_ver = model.version_of().unwrap_or(0);
        if current_ver != expected_version {
            return Err(HyveError::conflict(
                "version_conflict",
                format!("expected version {expected_version}, found {current_ver}"),
            ));
        }
        updater(&mut model);
        model.set_version(current_ver + 1);
        let new_row: StoredRow = model
            .to_sql_row()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        tbl.insert(id.as_str().to_owned(), new_row);
        Ok(model)
    }

    /// Starts a Track 1 typed chaining query against `M`'s table.
    #[must_use]
    pub fn query<M: TableModel>(&self) -> QueryBuilder<M> {
        QueryBuilder {
            state: Arc::clone(&self.state),
            filters: Vec::new(),
            after: None,
            limit: 50,
            _marker: PhantomData,
        }
    }
}

fn insert_row<M: TableModel>(state: &Arc<Mutex<DbState>>, model: M) -> HyveResult<M> {
    let pk = model.pk_value();
    let row: StoredRow = model
        .to_sql_row()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    let mut guard = state
        .lock()
        .map_err(|_| HyveError::internal("db mutex poisoned"))?;
    let tbl = guard.tables.entry(M::table_name().to_owned()).or_default();
    if tbl.contains_key(&pk) {
        return Err(HyveError::conflict(
            "conflict",
            format!("duplicate key `{pk}` in table `{}`", M::table_name()),
        ));
    }
    tbl.insert(pk, row);
    Ok(model)
}

/// Track 1 fluent query builder (`db.query::<Project>().where_eq(...).cursor_after(...).fetch_page()`).
pub struct QueryBuilder<M: TableModel> {
    state: Arc<Mutex<DbState>>,
    filters: Vec<(&'static str, SqlParam)>,
    after: Option<String>,
    limit: usize,
    _marker: PhantomData<fn() -> M>,
}

impl<M: TableModel> QueryBuilder<M> {
    /// Adds an equality filter verified against the exact column value type `V`.
    #[must_use]
    pub fn where_eq<V: IntoSqlParam>(mut self, col: Column<M, V>, value: V) -> Self {
        self.filters.push((col.name(), value.into_sql_param()));
        self
    }

    /// Configures cursor-based pagination starting strictly after `after`.
    #[must_use]
    pub fn cursor_after(mut self, after: Option<String>, limit: usize) -> Self {
        self.after = after.filter(|s| !s.is_empty());
        self.limit = limit.max(1);
        self
    }

    /// Executes the query and returns a cursor-paginated [`Page<M>`].
    pub async fn fetch_page(self) -> HyveResult<Page<M>> {
        let guard = self
            .state
            .lock()
            .map_err(|_| HyveError::internal("db mutex poisoned"))?;
        let schema = M::schema();
        let Some(tbl) = guard.tables.get(M::table_name()) else {
            return Ok(Page {
                data: Vec::new(),
                page: PageMeta {
                    next_cursor: None,
                    has_more: false,
                },
            });
        };

        let mut matched = Vec::new();
        for (pk, row) in tbl {
            if let Some(ref cursor) = self.after
                && pk <= cursor
            {
                continue;
            }
            let passes = self.filters.iter().all(|(col, expected)| {
                row.iter()
                    .find(|(k, _)| k == col)
                    .is_some_and(|(_, v)| v == expected)
            });
            if passes {
                let pairs: Vec<(&'static str, SqlParam)> = row
                    .iter()
                    .filter_map(|(k, v)| {
                        schema
                            .fields
                            .iter()
                            .find(|f| f.name == k)
                            .map(|f| (f.name, v.clone()))
                    })
                    .collect();
                matched.push(M::from_sql_row(&pairs)?);
            }
        }

        let has_more = matched.len() > self.limit;
        if has_more {
            matched.truncate(self.limit);
        }
        let next_cursor = matched.last().map(TableModel::pk_value);
        Ok(Page {
            data: matched,
            page: PageMeta {
                next_cursor,
                has_more,
            },
        })
    }
}

/// Trait abstracting execution targets for `sql!(...).fetch_one(exec)` (`Tx`, `&Tx`, `Db`, `&Db`, `Ctx<Db>`).
pub trait SqlExecutor {
    /// Executes a SQL query expecting one returned model row.
    fn execute_one<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<M>;

    /// Executes a SQL query returning zero or more model rows.
    fn execute_all<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<Vec<M>>;
}

impl SqlExecutor for Tx {
    fn execute_one<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<M> {
        run_sql_one(&self.state, sql, params)
    }
    fn execute_all<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<Vec<M>> {
        run_sql_all(&self.state, sql, params)
    }
}

impl SqlExecutor for &Tx {
    fn execute_one<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<M> {
        (*self).execute_one(sql, params)
    }
    fn execute_all<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<Vec<M>> {
        (*self).execute_all(sql, params)
    }
}

impl SqlExecutor for Db {
    fn execute_one<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<M> {
        run_sql_one(&self.state, sql, params)
    }
    fn execute_all<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<Vec<M>> {
        run_sql_all(&self.state, sql, params)
    }
}

impl SqlExecutor for &Db {
    fn execute_one<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<M> {
        (*self).execute_one(sql, params)
    }
    fn execute_all<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<Vec<M>> {
        (*self).execute_all(sql, params)
    }
}

impl SqlExecutor for Ctx<Db> {
    fn execute_one<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<M> {
        run_sql_one(&self.state, sql, params)
    }
    fn execute_all<M: TableModel>(&self, sql: &str, params: &[SqlParam]) -> HyveResult<Vec<M>> {
        run_sql_all(&self.state, sql, params)
    }
}

fn parse_sql_literal(tok: &str, params: &[SqlParam]) -> HyveResult<SqlParam> {
    let t = tok.trim();
    if let Some(num) = t.strip_prefix('$') {
        let idx: usize = num
            .parse::<usize>()
            .map_err(|_| HyveError::domain("invalid_sql", format!("invalid placeholder {t}")))?;
        return params
            .get(idx - 1)
            .cloned()
            .ok_or_else(|| HyveError::domain("invalid_sql", format!("missing SQL param ${idx}")));
    }
    if t.eq_ignore_ascii_case("true") {
        return Ok(SqlParam::Bool(true));
    }
    if t.eq_ignore_ascii_case("false") {
        return Ok(SqlParam::Bool(false));
    }
    if t.eq_ignore_ascii_case("null") {
        return Ok(SqlParam::Null);
    }
    if t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2 {
        return Ok(SqlParam::Text(t[1..t.len() - 1].to_owned()));
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(SqlParam::I64(i));
    }
    if let Ok(f) = t.parse::<f64>() {
        return Ok(SqlParam::F64(f));
    }
    Err(HyveError::domain(
        "invalid_sql",
        format!("unsupported SQL literal `{t}`"),
    ))
}

fn run_sql_one<M: TableModel>(
    state: &Arc<Mutex<DbState>>,
    sql: &str,
    params: &[SqlParam],
) -> HyveResult<M> {
    let rows = run_sql_all::<M>(state, sql, params)?;
    rows.into_iter()
        .next()
        .ok_or_else(|| HyveError::not_found(M::table_name(), "query returned zero rows"))
}

fn run_sql_all<M: TableModel>(
    state: &Arc<Mutex<DbState>>,
    sql: &str,
    params: &[SqlParam],
) -> HyveResult<Vec<M>> {
    let upper = sql.trim_start().to_ascii_uppercase();
    let schema = M::schema();
    if upper.starts_with("INSERT INTO") {
        let lp1 = sql
            .find('(')
            .ok_or_else(|| HyveError::domain("invalid_sql", "missing INSERT column list"))?;
        let rp1 = sql[lp1 + 1..]
            .find(')')
            .map(|x| lp1 + 1 + x)
            .ok_or_else(|| HyveError::domain("invalid_sql", "unclosed INSERT column list"))?;
        let table_seg = sql["INSERT INTO".len()..lp1].trim();
        if table_seg != M::table_name() {
            return Err(HyveError::domain(
                "invalid_sql",
                format!(
                    "table mismatch: SQL targets `{table_seg}`, model `{}` binds `{}`",
                    schema.name,
                    M::table_name()
                ),
            ));
        }
        let cols: Vec<&str> = sql[lp1 + 1..rp1].split(',').map(str::trim).collect();
        let rest = &sql[rp1 + 1..];
        let lp2 = rest
            .find('(')
            .ok_or_else(|| HyveError::domain("invalid_sql", "missing INSERT VALUES list"))?;
        let rp2 = rest[lp2 + 1..]
            .find(')')
            .map(|x| lp2 + 1 + x)
            .ok_or_else(|| HyveError::domain("invalid_sql", "unclosed INSERT VALUES list"))?;
        let vals: Vec<&str> = rest[lp2 + 1..rp2].split(',').map(str::trim).collect();
        if cols.len() != vals.len() {
            return Err(HyveError::domain(
                "invalid_sql",
                "INSERT column and value counts differ",
            ));
        }
        let mut row_pairs: Vec<(&'static str, SqlParam)> = Vec::with_capacity(cols.len());
        for (col, val_tok) in cols.into_iter().zip(vals) {
            let static_name = schema
                .fields
                .iter()
                .find(|f| f.name == col)
                .map(|f| f.name)
                .ok_or_else(|| {
                    HyveError::domain(
                        "invalid_sql",
                        format!("unknown column `{col}` on `{}`", schema.name),
                    )
                })?;
            let param = parse_sql_literal(val_tok, params)?;
            row_pairs.push((static_name, param));
        }
        let model = M::from_sql_row(&row_pairs)?;
        let inserted = insert_row(state, model)?;
        return Ok(vec![inserted]);
    }

    if upper.starts_with("SELECT ") {
        let guard = state
            .lock()
            .map_err(|_| HyveError::internal("db mutex poisoned"))?;
        let Some(tbl) = guard.tables.get(M::table_name()) else {
            return Ok(Vec::new());
        };
        let where_filter: Option<(&str, SqlParam)> = if let Some(wpos) = upper.find("WHERE ") {
            let clause = sql[wpos + 6..].trim();
            if let Some((lhs, rhs)) = clause.split_once('=') {
                let col = lhs.trim();
                let val_tok = rhs.split_whitespace().next().unwrap_or("");
                Some((col, parse_sql_literal(val_tok, params)?))
            } else {
                None
            }
        } else {
            None
        };

        let mut out = Vec::new();
        for row in tbl.values() {
            if let Some((col, ref expected)) = where_filter
                && !row
                    .iter()
                    .find(|(k, _)| k == col)
                    .is_some_and(|(_, v)| v == expected)
            {
                continue;
            }
            let pairs: Vec<(&'static str, SqlParam)> = row
                .iter()
                .filter_map(|(k, v)| {
                    schema
                        .fields
                        .iter()
                        .find(|f| f.name == k)
                        .map(|f| (f.name, v.clone()))
                })
                .collect();
            out.push(M::from_sql_row(&pairs)?);
        }
        return Ok(out);
    }

    Err(HyveError::domain(
        "invalid_sql",
        "unsupported SQL statement in Phase 1 in-memory engine",
    ))
}

/// Prepared compile-time checked SQL query created by `sql!(Model, "...")`.
pub struct SqlQuery<M> {
    sql: &'static str,
    params: Vec<SqlParam>,
    _marker: PhantomData<fn() -> M>,
}

impl<M: TableModel> SqlQuery<M> {
    /// Constructs a validated `SqlQuery<M>`.
    #[must_use]
    pub fn new(sql: &'static str, params: Vec<SqlParam>) -> Self {
        Self {
            sql,
            params,
            _marker: PhantomData,
        }
    }

    /// Executes the query expecting exactly one result row (`RETURNING *` or single SELECT).
    pub fn fetch_one<'a, E: SqlExecutor + 'a>(
        self,
        exec: E,
    ) -> Pin<Box<dyn Future<Output = HyveResult<M>> + 'a>> {
        Box::pin(async move { exec.execute_one::<M>(self.sql, &self.params) })
    }

    /// Executes the query returning all matching rows.
    pub fn fetch_all<'a, E: SqlExecutor + 'a>(
        self,
        exec: E,
    ) -> Pin<Box<dyn Future<Output = HyveResult<Vec<M>>> + 'a>> {
        Box::pin(async move { exec.execute_all::<M>(self.sql, &self.params) })
    }
}
