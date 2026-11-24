//! What `sql!` expands to: a query checked against the offline cache, with
//! its bind values and the dialects it was verified on.

use std::fmt;
use std::marker::PhantomData;

use hyve_core::{HyveResult, Id, Timestamp, Ulid};

use crate::db::{Exec, Target};
use crate::table::Table;
use crate::value::{DbValue, SqlType};

/// Parameter type markers (`sql!` binds each argument as the type the cache
/// recorded).
pub mod types {
    /// `bool`
    pub struct Bool;
    /// `int`
    pub struct Int;
    /// `float`
    pub struct Float;
    /// `text`
    pub struct Text;
    /// `bytes`
    pub struct Bytes;
    /// `timestamp`
    pub struct Timestamp;
}

/// A parameter type marker.
pub trait Marker {
    /// The type it stands for.
    const TYPE: SqlType;
}

macro_rules! markers {
    ($($m:ident),*) => {$(
        impl Marker for types::$m {
            const TYPE: SqlType = SqlType::$m;
        }
    )*};
}

markers!(Bool, Int, Float, Text, Bytes, Timestamp);

/// A Rust value that can be bound to a parameter of type `M`. Each Rust type
/// binds as exactly one type, so `sql!` can infer it where the engine did not
/// type the parameter.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be bound to this `sql!` parameter",
    label = "the offline cache records a different type for this parameter",
    note = "text: `String`, `&str`, `Ulid`, `Id<T>`; int: integers up to `i64`/`u32`; float: `f32`, `f64`; bool; bytes: `Vec<u8>`, `&[u8]`; timestamp: `Timestamp`; `Option` of any binds NULL"
)]
pub trait Encode<M> {
    /// The value to bind.
    fn encode(&self) -> DbValue;
}

impl<M, T: Encode<M> + ?Sized> Encode<M> for &T {
    fn encode(&self) -> DbValue {
        (**self).encode()
    }
}

impl<M, T: Encode<M>> Encode<M> for Option<T> {
    fn encode(&self) -> DbValue {
        self.as_ref().map_or(DbValue::Null, Encode::encode)
    }
}

impl Encode<types::Text> for str {
    fn encode(&self) -> DbValue {
        DbValue::Text(self.to_owned())
    }
}

impl Encode<types::Text> for String {
    fn encode(&self) -> DbValue {
        DbValue::Text(self.clone())
    }
}

impl Encode<types::Text> for Ulid {
    fn encode(&self) -> DbValue {
        DbValue::Text(self.to_string())
    }
}

impl<T> Encode<types::Text> for Id<T> {
    fn encode(&self) -> DbValue {
        DbValue::Text(self.to_string())
    }
}

macro_rules! encode_ints {
    ($($t:ty),*) => {$(
        impl Encode<types::Int> for $t {
            fn encode(&self) -> DbValue {
                DbValue::Int(i64::from(*self))
            }
        }
    )*};
}

encode_ints!(i8, i16, i32, i64, u8, u16, u32);

impl Encode<types::Float> for f64 {
    fn encode(&self) -> DbValue {
        DbValue::Float(*self)
    }
}

impl Encode<types::Float> for f32 {
    fn encode(&self) -> DbValue {
        DbValue::Float(f64::from(*self))
    }
}

impl Encode<types::Bool> for bool {
    fn encode(&self) -> DbValue {
        DbValue::Bool(*self)
    }
}

impl Encode<types::Bytes> for [u8] {
    fn encode(&self) -> DbValue {
        DbValue::Bytes(self.to_vec())
    }
}

impl Encode<types::Bytes> for Vec<u8> {
    fn encode(&self) -> DbValue {
        DbValue::Bytes(self.clone())
    }
}

impl Encode<types::Timestamp> for Timestamp {
    fn encode(&self) -> DbValue {
        DbValue::Timestamp(*self)
    }
}

/// A bound parameter: the value and the type it binds as (`NULL` needs one).
#[derive(Debug, Clone, PartialEq)]
pub struct Bind {
    /// The value.
    pub value: DbValue,
    /// The parameter type.
    pub ty: SqlType,
}

/// Binds `value` as a parameter of type `M`.
pub fn bind<M: Marker, V: Encode<M> + ?Sized>(value: &V) -> Bind {
    Bind {
        value: value.encode(),
        ty: M::TYPE,
    }
}

/// A query from `sql!`. `M` is the model rows decode into (`()` for
/// statements without rows).
pub struct Sql<M = ()> {
    sql: &'static str,
    dialects: &'static [&'static str],
    columns: &'static [&'static str],
    binds: Vec<Bind>,
    _model: PhantomData<fn() -> M>,
}

impl<M> Sql<M> {
    /// A checked query (built by `sql!`).
    pub fn new(
        sql: &'static str,
        dialects: &'static [&'static str],
        columns: &'static [&'static str],
        binds: Vec<Bind>,
    ) -> Self {
        Sql {
            sql,
            dialects,
            columns,
            binds,
            _model: PhantomData,
        }
    }

    /// The query text.
    pub fn sql(&self) -> &'static str {
        self.sql
    }

    /// The dialects the query was verified on (`postgres`, `sqlite`).
    pub fn dialects(&self) -> &'static [&'static str] {
        self.dialects
    }

    /// The result columns in query order.
    pub fn columns(&self) -> &'static [&'static str] {
        self.columns
    }

    /// The bound parameters in order.
    pub fn binds(&self) -> &[Bind] {
        &self.binds
    }

    /// Runs a statement without rows; returns the number of rows affected.
    pub async fn execute(self, target: impl Target) -> HyveResult<u64> {
        let exec = self.checked(target)?;
        exec.execute(self.sql.into(), &self.binds).await
    }

    /// Refuses a database whose dialect the query was not verified on,
    /// without sending it.
    fn checked(&self, target: impl Target) -> HyveResult<Exec> {
        let exec = target.exec();
        let dialect = exec.dialect().name();
        if self.dialects.contains(&dialect) {
            Ok(exec)
        } else {
            Err(hyve_core::HyveError::internal(format!(
                "this `sql!` query was verified on {} only, not on the {dialect} database it was sent to \
                 (it was not sent); make it portable and run `hyve codegen`, or test it on the dialect it targets: {}",
                if self.dialects.is_empty() {
                    "no dialect".to_owned()
                } else {
                    self.dialects.join(", ")
                },
                self.sql
            )))
        }
    }
}

impl<M: Table> Sql<M> {
    /// The single row; `<table>_not_found` without one.
    pub async fn fetch_one(self, target: impl Target) -> HyveResult<M> {
        self.fetch_optional(target)
            .await?
            .ok_or_else(|| hyve_core::HyveError::not_found(M::NAME))
    }

    /// The first row, if any.
    pub async fn fetch_optional(self, target: impl Target) -> HyveResult<Option<M>> {
        Ok(self.fetch_all(target).await?.into_iter().next())
    }

    /// Every row.
    pub async fn fetch_all(self, target: impl Target) -> HyveResult<Vec<M>> {
        let exec = self.checked(target)?;
        exec.fetch::<M>(self.sql.into(), &self.binds).await
    }
}

impl<M> fmt::Debug for Sql<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sql")
            .field("sql", &self.sql)
            .field("dialects", &self.dialects)
            .finish_non_exhaustive()
    }
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Compile-time check of one result column against `M` (emitted by `sql!`).
/// Unknown types and nullability are left to decoding.
pub const fn assert_column<M: Table>(
    name: &str,
    ty: Option<SqlType>,
    nullable: Option<bool>,
    missing: &str,
    wrong_type: &str,
    not_optional: &str,
) {
    let mut i = 0;
    while i < M::COLUMNS.len() {
        if str_eq(M::COLUMNS[i], name) {
            let (field_ty, field_nullable) = M::TYPES[i];
            if let Some(ty) = ty
                && field_ty as u8 != ty as u8
            {
                panic!("{}", wrong_type);
            }
            if let Some(true) = nullable
                && !field_nullable
            {
                panic!("{}", not_optional);
            }
            return;
        }
        i += 1;
    }
    panic!("{}", missing);
}

/// Compile-time check that the query returns every column of `M`.
pub const fn assert_all_columns<M: Table>(names: &[&str], message: &str) {
    let mut i = 0;
    while i < M::COLUMNS.len() {
        let mut found = false;
        let mut j = 0;
        while j < names.len() {
            if str_eq(names[j], M::COLUMNS[i]) {
                found = true;
            }
            j += 1;
        }
        if !found {
            panic!("{}", message);
        }
        i += 1;
    }
}
