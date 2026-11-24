//! Query analysis for `hyve-orbic codegen` (feature `codegen`): what the
//! engine reports about a query's parameters and result columns, mapped to
//! the `sql!` cache types.

use hyve_core::{HyveError, HyveResult};
use sqlx::{Column as _, Either, Executor as _, SqlSafeStr as _, TypeInfo as _};

use crate::db::{Db, Dialect};
use crate::driver;
use crate::value::SqlType;

/// A described query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// Parameter types in order (`None`: the engine does not say).
    pub params: Vec<Option<SqlType>>,
    /// Result columns: name, type, nullability (`None`: unknown).
    pub columns: Vec<(String, Option<SqlType>, Option<bool>)>,
}

/// The cache type of an engine type name; `None` for types orbic does not
/// map (they stay unchecked until decoding).
pub fn sql_type(dialect: Dialect, engine: &str) -> Option<SqlType> {
    let t = engine.to_ascii_uppercase();
    match dialect {
        Dialect::Sqlite => match t.as_str() {
            "BOOLEAN" => Some(SqlType::Bool),
            "INTEGER" => Some(SqlType::Int),
            "REAL" => Some(SqlType::Float),
            "TEXT" => Some(SqlType::Text),
            "BLOB" => Some(SqlType::Bytes),
            "DATETIME" => Some(SqlType::Timestamp),
            _ => None,
        },
        Dialect::Postgres => match t.as_str() {
            "BOOL" => Some(SqlType::Bool),
            "INT2" | "INT4" | "INT8" => Some(SqlType::Int),
            "FLOAT4" | "FLOAT8" => Some(SqlType::Float),
            "TEXT" | "VARCHAR" | "BPCHAR" | "CHAR" | "NAME" => Some(SqlType::Text),
            "BYTEA" => Some(SqlType::Bytes),
            "TIMESTAMPTZ" | "TIMESTAMP" => Some(SqlType::Timestamp),
            _ => None,
        },
    }
}

impl Db {
    /// Asks the engine about `sql` without running it.
    pub async fn describe(&self, sql: &str) -> HyveResult<Described> {
        let text = || sqlx::AssertSqlSafe(sql.to_owned()).into_sql_str();
        let dialect = self.dialect();
        macro_rules! described {
            ($d:expr) => {{
                let d = $d.map_err(|e| match e {
                    sqlx::Error::Database(e) => HyveError::invalid_request(e.message().to_owned()),
                    e => driver::error(e),
                })?;
                let params = match d.parameters() {
                    Some(Either::Left(types)) => {
                        types.iter().map(|t| sql_type(dialect, t.name())).collect()
                    }
                    Some(Either::Right(n)) => vec![None; n],
                    None => Vec::new(),
                };
                let columns = d
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        (
                            c.name().to_owned(),
                            sql_type(dialect, c.type_info().name()),
                            d.nullable(i),
                        )
                    })
                    .collect();
                Described { params, columns }
            }};
        }
        Ok(match (self.sqlite_pool(), self.pg_pool()) {
            (Some(p), _) => described!(p.describe(text()).await),
            (_, Some(p)) => described!(p.describe(text()).await),
            _ => unreachable!("a Db has one pool"),
        })
    }
}
