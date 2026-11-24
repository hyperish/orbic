//! Chaining queries over `#[table]` models (`docs/01-dx-and-dsl.md` §1):
//! `db.query::<Project>().where_eq(Project::OWNER_ID, user.id).cursor_after(after, 50).fetch_page()`.
//! Identifiers come from `#[table]` (checked `snake_case`) and are quoted;
//! values are always bound.

use std::fmt::Write as _;
use std::marker::PhantomData;

use hyve_api::Page;
use hyve_core::{HyveError, HyveResult};

use crate::db::{Db, Exec, Target, Tx};
use crate::driver::{Text, Want};
use crate::sql::Bind;
use crate::table::{Column, Table};
use crate::value::{DbValue, SqlType, SqlValue};

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

#[derive(Default)]
struct Filters {
    sql: Vec<String>,
    binds: Vec<Bind>,
}

impl Filters {
    fn push<T: SqlValue>(&mut self, column: &str, op: &str, value: &T) {
        self.binds.push(Bind {
            value: value.to_db(),
            ty: T::TYPE,
        });
        self.sql
            .push(format!("\"{column}\" {op} ${}", self.binds.len()));
    }

    fn raw(&mut self, sql: String) {
        self.sql.push(sql);
    }

    fn write(&self, out: &mut String) {
        if !self.sql.is_empty() {
            out.push_str(" WHERE ");
            out.push_str(&self.sql.join(" AND "));
        }
    }
}

macro_rules! filters {
    () => {
        /// `column = value`.
        pub fn where_eq<T: SqlValue>(mut self, column: Column<M, T>, value: T) -> Self {
            self.filters.push(column.name(), "=", &value);
            self
        }

        /// `column <> value`.
        pub fn where_ne<T: SqlValue>(mut self, column: Column<M, T>, value: T) -> Self {
            self.filters.push(column.name(), "<>", &value);
            self
        }

        /// `column < value`.
        pub fn where_lt<T: SqlValue>(mut self, column: Column<M, T>, value: T) -> Self {
            self.filters.push(column.name(), "<", &value);
            self
        }

        /// `column <= value`.
        pub fn where_le<T: SqlValue>(mut self, column: Column<M, T>, value: T) -> Self {
            self.filters.push(column.name(), "<=", &value);
            self
        }

        /// `column > value`.
        pub fn where_gt<T: SqlValue>(mut self, column: Column<M, T>, value: T) -> Self {
            self.filters.push(column.name(), ">", &value);
            self
        }

        /// `column >= value`.
        pub fn where_ge<T: SqlValue>(mut self, column: Column<M, T>, value: T) -> Self {
            self.filters.push(column.name(), ">=", &value);
            self
        }

        /// `column IS NULL`.
        pub fn where_null<T: SqlValue>(mut self, column: Column<M, Option<T>>) -> Self {
            self.filters.raw(format!("\"{}\" IS NULL", column.name()));
            self
        }

        /// `column IS NOT NULL`.
        pub fn where_not_null<T: SqlValue>(mut self, column: Column<M, Option<T>>) -> Self {
            self.filters
                .raw(format!("\"{}\" IS NOT NULL", column.name()));
            self
        }
    };
}

/// A `SELECT` over `M`.
#[must_use = "a query does nothing until fetched"]
pub struct Select<M> {
    exec: Exec,
    filters: Filters,
    order: Option<(&'static str, Order)>,
    limit: Option<u32>,
    cursor: Option<(Option<String>, u32)>,
    _model: PhantomData<fn() -> M>,
}

impl<M: Table> Select<M> {
    filters!();

    /// Orders by `column` (not with [`cursor_after`](Self::cursor_after),
    /// which orders by the primary key).
    pub fn order_by<T>(mut self, column: Column<M, T>, order: Order) -> Self {
        self.order = Some((column.name(), order));
        self
    }

    /// At most `n` rows.
    pub fn limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }

    /// The page of at most `limit` rows after the cursor `after` (none: the
    /// first page), in primary-key order; for [`fetch_page`](Self::fetch_page).
    pub fn cursor_after(mut self, after: Option<String>, limit: u32) -> Self {
        self.cursor = Some((after, limit));
        self
    }

    fn build(&mut self, extra: u32) -> HyveResult<String> {
        let pk = M::PK;
        if let Some((after, _)) = &self.cursor {
            if self.order.is_some() {
                return Err(HyveError::internal(
                    "`cursor_after` orders by the primary key; drop `order_by`",
                ));
            }
            if let Some(after) = after {
                let i = pk_index::<M>();
                let (ty, _) = M::TYPES[i];
                let value = match ty {
                    SqlType::Int => after.parse().map(DbValue::Int).map_err(|_| {
                        HyveError::invalid_request(format!("invalid cursor `{after}`"))
                            .with_detail("field", "after")
                    })?,
                    _ => DbValue::Text(after.clone()),
                };
                self.filters.binds.push(Bind { value, ty });
                let n = self.filters.binds.len();
                self.filters.raw(format!("\"{pk}\" > ${n}"));
            }
        }
        let mut sql = String::from("SELECT ");
        sql.push_str(
            &M::COLUMNS
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let _ = write!(sql, " FROM \"{}\"", M::NAME);
        self.filters.write(&mut sql);
        match (self.cursor.as_ref(), self.order) {
            (Some(_), _) => {
                let _ = write!(sql, " ORDER BY \"{pk}\" ASC");
            }
            (None, Some((c, o))) => {
                let _ = write!(
                    sql,
                    " ORDER BY \"{c}\" {}",
                    if o == Order::Asc { "ASC" } else { "DESC" }
                );
            }
            (None, None) => {}
        }
        let limit = self.cursor.as_ref().map(|(_, l)| *l).or(self.limit);
        if let Some(l) = limit {
            if l == 0 {
                return Err(HyveError::internal("a query limit must be at least 1"));
            }
            let _ = write!(sql, " LIMIT {}", u64::from(l) + u64::from(extra));
        }
        Ok(sql)
    }

    /// Every matching row.
    pub async fn fetch_all(mut self) -> HyveResult<Vec<M>> {
        let sql = self.build(0)?;
        self.exec
            .fetch::<M>(Text::Built(sql), &self.filters.binds)
            .await
    }

    /// The first matching row, if any.
    pub async fn fetch_optional(self) -> HyveResult<Option<M>> {
        Ok(self.limit(1).fetch_all().await?.into_iter().next())
    }

    /// The first matching row; `<table>_not_found` without one.
    pub async fn fetch_one(self) -> HyveResult<M> {
        self.fetch_optional()
            .await?
            .ok_or_else(|| HyveError::not_found(M::NAME))
    }

    /// The page selected by [`cursor_after`](Self::cursor_after); the next
    /// cursor is the last row's primary key when more rows follow.
    pub async fn fetch_page(mut self) -> HyveResult<Page<M>> {
        let Some((_, limit)) = self.cursor.as_ref().map(|(a, l)| (a.clone(), *l)) else {
            return Err(HyveError::internal(
                "`fetch_page` needs `cursor_after(after, limit)`",
            ));
        };
        let sql = self.build(1)?;
        let mut rows = self
            .exec
            .fetch::<M>(Text::Built(sql), &self.filters.binds)
            .await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = if more {
            rows.last().map(pk_text::<M>)
        } else {
            None
        };
        Ok(Page::new(rows, next))
    }

    /// The number of matching rows.
    pub async fn count(mut self) -> HyveResult<u64> {
        let mut sql = format!("SELECT COUNT(*) AS \"n\" FROM \"{}\"", M::NAME);
        self.filters.write(&mut sql);
        let binds = std::mem::take(&mut self.filters.binds);
        let rows = self
            .exec
            .rows(
                Text::Built(sql),
                &binds,
                &[Want {
                    name: "n",
                    ty: SqlType::Int,
                }],
            )
            .await?;
        match rows.first().and_then(|r| r.first()) {
            Some(DbValue::Int(n)) => Ok(u64::try_from(*n).unwrap_or(0)),
            _ => Err(HyveError::internal("COUNT(*) returned no row")),
        }
    }
}

fn pk_index<M: Table>() -> usize {
    M::COLUMNS
        .iter()
        .position(|c| *c == M::PK)
        .expect("#[table] checks the primary key")
}

fn pk_text<M: Table>(row: &M) -> String {
    match row.values().swap_remove(pk_index::<M>()) {
        DbValue::Text(s) => s,
        DbValue::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

/// An `UPDATE` of `M`.
#[must_use = "an update does nothing until executed"]
pub struct Update<M> {
    exec: Exec,
    sets: Vec<(&'static str, Bind)>,
    filters: Filters,
    _model: PhantomData<fn() -> M>,
}

impl<M: Table> Update<M> {
    filters!();

    /// `SET column = value`.
    pub fn set<T: SqlValue>(mut self, column: Column<M, T>, value: T) -> Self {
        self.sets.push((
            column.name(),
            Bind {
                value: value.to_db(),
                ty: T::TYPE,
            },
        ));
        self
    }

    /// Runs it; returns the number of rows updated (0 is how optimistic
    /// locking notices a stale `expectedVersion`).
    pub async fn execute(self) -> HyveResult<u64> {
        if self.sets.is_empty() {
            return Err(HyveError::internal("an update needs at least one `set`"));
        }
        let mut binds: Vec<Bind> = Vec::new();
        let sets: Vec<String> = self
            .sets
            .into_iter()
            .map(|(c, b)| {
                binds.push(b);
                format!("\"{c}\" = ${}", binds.len())
            })
            .collect();
        // Renumber the filters after the SET values.
        let offset = binds.len();
        let mut sql = format!("UPDATE \"{}\" SET {}", M::NAME, sets.join(", "));
        if !self.filters.sql.is_empty() {
            let renumbered: Vec<String> =
                self.filters.sql.iter().map(|f| shift(f, offset)).collect();
            sql.push_str(" WHERE ");
            sql.push_str(&renumbered.join(" AND "));
        }
        binds.extend(self.filters.binds);
        self.exec.execute(Text::Built(sql), &binds).await
    }
}

/// `$n` -> `$(n + by)` in a filter written by this module.
fn shift(filter: &str, by: usize) -> String {
    match filter.rsplit_once('$') {
        Some((head, n)) => match n.parse::<usize>() {
            Ok(n) => format!("{head}${}", n + by),
            Err(_) => filter.to_owned(),
        },
        None => filter.to_owned(),
    }
}

/// A `DELETE` from `M`.
#[must_use = "a delete does nothing until executed"]
pub struct Delete<M> {
    exec: Exec,
    filters: Filters,
    _model: PhantomData<fn() -> M>,
}

impl<M: Table> Delete<M> {
    filters!();

    /// Runs it; returns the number of rows deleted.
    pub async fn execute(self) -> HyveResult<u64> {
        let mut sql = format!("DELETE FROM \"{}\"", M::NAME);
        self.filters.write(&mut sql);
        self.exec
            .execute(Text::Built(sql), &self.filters.binds)
            .await
    }
}

async fn insert<M: Table>(exec: Exec, row: &M) -> HyveResult<()> {
    let cols: Vec<String> = M::COLUMNS.iter().map(|c| format!("\"{c}\"")).collect();
    let params: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "INSERT INTO \"{}\" ({}) VALUES ({})",
        M::NAME,
        cols.join(", "),
        params.join(", ")
    );
    let binds: Vec<Bind> = row
        .values()
        .into_iter()
        .zip(M::TYPES)
        .map(|(value, (ty, _))| Bind { value, ty: *ty })
        .collect();
    exec.execute(Text::Built(sql), &binds).await.map(drop)
}

macro_rules! entry_points {
    ($t:ty) => {
        impl $t {
            /// A `SELECT` over `M`.
            pub fn query<M: Table>(&self) -> Select<M> {
                Select {
                    exec: self.clone().exec(),
                    filters: Filters::default(),
                    order: None,
                    limit: None,
                    cursor: None,
                    _model: PhantomData,
                }
            }

            /// Inserts `row`.
            pub async fn insert<M: Table>(&self, row: &M) -> HyveResult<()> {
                insert(self.clone().exec(), row).await
            }

            /// An `UPDATE` of `M`.
            pub fn update<M: Table>(&self) -> Update<M> {
                Update {
                    exec: self.clone().exec(),
                    sets: Vec::new(),
                    filters: Filters::default(),
                    _model: PhantomData,
                }
            }

            /// A `DELETE` from `M`.
            pub fn delete<M: Table>(&self) -> Delete<M> {
                Delete {
                    exec: self.clone().exec(),
                    filters: Filters::default(),
                    _model: PhantomData,
                }
            }
        }
    };
}

entry_points!(Db);
entry_points!(Tx);

impl Tx {
    /// Runs raw SQL without binds inside the transaction.
    pub async fn execute_script(&self, sql: &str) -> HyveResult<()> {
        self.clone().exec().script(sql).await
    }
}

pub(crate) async fn applied_migrations(db: &Db) -> HyveResult<Vec<(String, String)>> {
    let want = [
        Want {
            name: "name",
            ty: SqlType::Text,
        },
        Want {
            name: "checksum",
            ty: SqlType::Text,
        },
    ];
    let rows = db
        .clone()
        .exec()
        .rows(
            Text::Static("SELECT name, checksum FROM _orbic_migrations ORDER BY name"),
            &[],
            &want,
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| match <[DbValue; 2]>::try_from(r) {
            Ok([DbValue::Text(n), DbValue::Text(c)]) => Some((n, c)),
            _ => None,
        })
        .collect())
}

pub(crate) async fn record_migration(tx: &Tx, name: &str, checksum: &str) -> HyveResult<()> {
    let binds = [
        Bind {
            value: DbValue::Text(name.to_owned()),
            ty: SqlType::Text,
        },
        Bind {
            value: DbValue::Text(checksum.to_owned()),
            ty: SqlType::Text,
        },
    ];
    tx.clone()
        .exec()
        .execute(
            Text::Static("INSERT INTO _orbic_migrations (name, checksum) VALUES ($1, $2)"),
            &binds,
        )
        .await
        .map(drop)
}
