//! Binding and decoding per dialect, on top of the pinned `sqlx` drivers.

use hyve_core::{HyveError, HyveResult, Timestamp};
use sqlx::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueFormat, PgValueRef, Postgres};
use sqlx::sqlite::Sqlite;
use sqlx::{Column as _, Row as _, TypeInfo as _, ValueRef as _};

use crate::sql::Bind;
use crate::value::{DbValue, SqlType};

/// A column to read: its name in the result and the type to decode.
pub(crate) struct Want<'a> {
    pub(crate) name: &'a str,
    pub(crate) ty: SqlType,
}

/// The query text: a literal from `sql!`, or text the query builder made
/// from checked identifiers (values are always bound).
pub(crate) enum Text {
    Static(&'static str),
    Built(String),
}

impl From<&'static str> for Text {
    fn from(s: &'static str) -> Self {
        Text::Static(s)
    }
}

impl Text {
    fn safe(&self) -> sqlx::SqlStr {
        use sqlx::SqlSafeStr;
        match self {
            Text::Static(s) => s.into_sql_str(),
            Text::Built(s) => sqlx::AssertSqlSafe(s.clone()).into_sql_str(),
        }
    }
}

/// Maps driver errors: unreachable databases are retryable `Infra` errors,
/// unique and other constraint violations are `Domain` errors, the rest is
/// `Internal` (its message stays server-side).
pub(crate) fn error(e: sqlx::Error) -> HyveError {
    match &e {
        sqlx::Error::Io(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => {
            HyveError::infra("service_unavailable", format!("database unavailable: {e}"))
                .retryable(true)
        }
        sqlx::Error::Database(d) if d.is_unique_violation() => HyveError::domain(
            "already_exists",
            "a row with the same unique value already exists",
        ),
        sqlx::Error::Database(d) if d.is_foreign_key_violation() || d.is_check_violation() => {
            HyveError::domain(
                "constraint_violated",
                format!("the change violates a constraint: {}", d.message()),
            )
        }
        _ => HyveError::internal(format!("database error: {e}")),
    }
}

fn decode_error(column: &str, why: impl std::fmt::Display) -> HyveError {
    HyveError::internal(format!("column `{column}`: {why}"))
}

/// Microseconds between the Unix epoch and PostgreSQL's (2000-01-01).
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;
const TIMESTAMPTZ: u32 = 1184;
const TIMESTAMP: u32 = 1114;

/// `Timestamp` on PostgreSQL: `timestamptz` (also reads `timestamp`).
pub(crate) struct PgTs(pub(crate) Timestamp);

impl sqlx::Type<Postgres> for PgTs {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(sqlx::postgres::types::Oid(TIMESTAMPTZ))
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        matches!(
            ty.oid(),
            Some(sqlx::postgres::types::Oid(TIMESTAMPTZ | TIMESTAMP))
        )
    }
}

impl sqlx::Encode<'_, Postgres> for PgTs {
    fn encode_by_ref(
        &self,
        buf: &mut PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        buf.extend_from_slice(&(self.0.as_unix_micros() - PG_EPOCH_MICROS).to_be_bytes());
        Ok(sqlx::encode::IsNull::No)
    }
}

impl sqlx::Decode<'_, Postgres> for PgTs {
    fn decode(value: PgValueRef<'_>) -> Result<Self, sqlx::error::BoxDynError> {
        if value.format() != PgValueFormat::Binary {
            return Err("timestamps are read in binary format".into());
        }
        let bytes: [u8; 8] = value
            .as_bytes()?
            .try_into()
            .map_err(|_| "a timestamp is 8 bytes")?;
        let micros = i64::from_be_bytes(bytes)
            .checked_add(PG_EPOCH_MICROS)
            .ok_or("timestamp out of range")?;
        Ok(PgTs(
            Timestamp::from_unix_micros(micros).ok_or("timestamp outside years 0000-9999")?,
        ))
    }
}

pub(crate) mod sqlite {
    use super::*;

    pub(crate) fn query(
        sql: &Text,
        binds: &[Bind],
    ) -> sqlx::query::Query<'static, Sqlite, sqlx::sqlite::SqliteArguments> {
        let mut q = sqlx::query::<Sqlite>(sql.safe());
        for b in binds {
            q = match (&b.value, b.ty) {
                (DbValue::Null, SqlType::Bool) => q.bind(None::<bool>),
                (DbValue::Null, SqlType::Int | SqlType::Timestamp) => q.bind(None::<i64>),
                (DbValue::Null, SqlType::Float) => q.bind(None::<f64>),
                (DbValue::Null, SqlType::Text) => q.bind(None::<String>),
                (DbValue::Null, SqlType::Bytes) => q.bind(None::<Vec<u8>>),
                (DbValue::Bool(v), _) => q.bind(*v),
                (DbValue::Int(v), _) => q.bind(*v),
                (DbValue::Float(v), _) => q.bind(*v),
                (DbValue::Text(v), _) => q.bind(v.clone()),
                (DbValue::Bytes(v), _) => q.bind(v.clone()),
                // Integer microseconds sort and compare correctly.
                (DbValue::Timestamp(t), _) => q.bind(t.as_unix_micros()),
            };
        }
        q
    }

    pub(crate) fn field(row: &sqlx::sqlite::SqliteRow, i: usize, w: &Want) -> HyveResult<DbValue> {
        let raw = row.try_get_raw(i).map_err(|e| decode_error(w.name, e))?;
        if raw.is_null() {
            return Ok(DbValue::Null);
        }
        let r = match w.ty {
            SqlType::Bool => crate::sqlx_row::checked::<Sqlite, bool>(raw, i).map(DbValue::Bool),
            SqlType::Int => crate::sqlx_row::checked::<Sqlite, i64>(raw, i).map(DbValue::Int),
            SqlType::Float => crate::sqlx_row::checked::<Sqlite, f64>(raw, i).map(DbValue::Float),
            SqlType::Text => crate::sqlx_row::checked::<Sqlite, String>(raw, i).map(DbValue::Text),
            SqlType::Bytes => {
                crate::sqlx_row::checked::<Sqlite, Vec<u8>>(raw, i).map(DbValue::Bytes)
            }
            SqlType::Timestamp if raw.type_info().name() == "INTEGER" => {
                let us = <i64 as sqlx::Decode<Sqlite>>::decode(raw)
                    .map_err(|e| decode_error(w.name, e))?;
                return Timestamp::from_unix_micros(us)
                    .map(DbValue::Timestamp)
                    .ok_or_else(|| decode_error(w.name, "timestamp out of range"));
            }
            SqlType::Timestamp => {
                let s = <String as sqlx::Decode<Sqlite>>::decode(raw)
                    .map_err(|e| decode_error(w.name, e))?;
                return s
                    .parse()
                    .map(DbValue::Timestamp)
                    .map_err(|_| decode_error(w.name, format!("`{s}` is not a timestamp")));
            }
        };
        r.map_err(|e| decode_error(w.name, e))
    }
}

pub(crate) mod pg {
    use super::*;

    pub(crate) fn query(
        sql: &Text,
        binds: &[Bind],
    ) -> sqlx::query::Query<'static, Postgres, sqlx::postgres::PgArguments> {
        let mut q = sqlx::query::<Postgres>(sql.safe());
        for b in binds {
            q = match (&b.value, b.ty) {
                (DbValue::Null, SqlType::Bool) => q.bind(None::<bool>),
                (DbValue::Null, SqlType::Int) => q.bind(None::<i64>),
                (DbValue::Null, SqlType::Float) => q.bind(None::<f64>),
                (DbValue::Null, SqlType::Text) => q.bind(None::<String>),
                (DbValue::Null, SqlType::Bytes) => q.bind(None::<Vec<u8>>),
                (DbValue::Null, SqlType::Timestamp) => q.bind(None::<PgTs>),
                (DbValue::Bool(v), _) => q.bind(*v),
                (DbValue::Int(v), _) => q.bind(*v),
                (DbValue::Float(v), _) => q.bind(*v),
                (DbValue::Text(v), _) => q.bind(v.clone()),
                (DbValue::Bytes(v), _) => q.bind(v.clone()),
                (DbValue::Timestamp(t), _) => q.bind(PgTs(*t)),
            };
        }
        q
    }

    pub(crate) fn field(row: &sqlx::postgres::PgRow, i: usize, w: &Want) -> HyveResult<DbValue> {
        let raw = row.try_get_raw(i).map_err(|e| decode_error(w.name, e))?;
        if raw.is_null() {
            return Ok(DbValue::Null);
        }
        let r = match w.ty {
            SqlType::Bool => crate::sqlx_row::checked::<Postgres, bool>(raw, i).map(DbValue::Bool),
            SqlType::Int => match raw.type_info().name() {
                "INT2" => crate::sqlx_row::checked::<Postgres, i16>(raw, i)
                    .map(|v| DbValue::Int(v.into())),
                "INT4" => crate::sqlx_row::checked::<Postgres, i32>(raw, i)
                    .map(|v| DbValue::Int(v.into())),
                _ => crate::sqlx_row::checked::<Postgres, i64>(raw, i).map(DbValue::Int),
            },
            SqlType::Float => match raw.type_info().name() {
                "FLOAT4" => crate::sqlx_row::checked::<Postgres, f32>(raw, i)
                    .map(|v| DbValue::Float(v.into())),
                _ => crate::sqlx_row::checked::<Postgres, f64>(raw, i).map(DbValue::Float),
            },
            SqlType::Text => {
                crate::sqlx_row::checked::<Postgres, String>(raw, i).map(DbValue::Text)
            }
            SqlType::Bytes => {
                crate::sqlx_row::checked::<Postgres, Vec<u8>>(raw, i).map(DbValue::Bytes)
            }
            SqlType::Timestamp => {
                crate::sqlx_row::checked::<Postgres, PgTs>(raw, i).map(|t| DbValue::Timestamp(t.0))
            }
        };
        r.map_err(|e| decode_error(w.name, e))
    }
}

/// Resolve names once for each statement layout. SQLite can return multiple
/// layouts from one query, and duplicate names select the first column.
pub(crate) fn decode_rows<R: sqlx::Row, M>(
    rows: &[R],
    want: &[Want],
    field: impl Fn(&R, usize, &Want) -> HyveResult<DbValue>,
    map: impl Fn(&mut dyn Iterator<Item = HyveResult<DbValue>>) -> HyveResult<M>,
) -> HyveResult<Vec<M>> {
    let mut previous = None;
    let mut indices = Vec::with_capacity(want.len());
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let columns = row.columns();
        let layout = (columns.as_ptr(), columns.len());
        if previous != Some(layout) {
            indices.clear();
            for w in want {
                let index = columns
                    .iter()
                    .position(|column| column.name() == w.name)
                    .ok_or_else(|| decode_error(w.name, "not in the result"))?;
                indices.push(index);
            }
            previous = Some(layout);
        }
        let mut fields = indices
            .iter()
            .zip(want)
            .map(|(&index, w)| field(row, index, w));
        out.push(map(&mut fields)?);
    }
    Ok(out)
}
