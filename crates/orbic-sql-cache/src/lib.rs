//! The offline metadata cache of `sql!` (`docs/01-dx-and-dsl.md` §4).
//!
//! `hyve-orbic codegen` analyzes each query on a database with the migrations
//! applied and writes one file per query to `.orbic/sql/<hash>.json` at the
//! workspace root; the `sql!` macro only reads these files and never connects
//! to a database. The format is internal to orbic:
//!
//! ```json
//! {"sql": "SELECT ...", "params": [{"type": "text"}],
//!  "columns": [{"name": "id", "type": "text", "nullable": false}],
//!  "dialects": ["postgres", "sqlite"]}
//! ```
//!
//! Types are `bool`, `int`, `float`, `text`, `bytes`, `timestamp`, or `null`
//! when no verifying engine could tell (SQLite does not type parameters);
//! `nullable` is `null` when unknown (PostgreSQL for most expressions).
//! Unknown facts are not checked at compile time and are checked when rows
//! are decoded. `dialects` lists where the query was verified.

use std::path::{Path, PathBuf};

mod scan;

pub use scan::{Found, Scanned, SqlInput, scan};

use hyve_core::Value;
use hyve_serde::{fnv1a, from_json, to_json};

/// The cache directory, relative to the workspace root.
pub const CACHE_DIR: &str = ".orbic/sql";

/// The type names a cache may contain.
pub const TYPES: [&str; 6] = ["bool", "int", "float", "text", "bytes", "timestamp"];

/// The dialects orbic verifies queries on.
pub const DIALECTS: [&str; 2] = ["postgres", "sqlite"];

/// One bind parameter (`$1`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// Type name, if known.
    pub ty: Option<String>,
}

/// One result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Type name, if known.
    pub ty: Option<String>,
    /// Whether it may be `NULL`, if known.
    pub nullable: Option<bool>,
}

/// What the cache records for one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedQuery {
    /// The query text, exactly as written in `sql!`.
    pub sql: String,
    /// Parameters in order.
    pub params: Vec<Param>,
    /// Result columns in order (empty for statements without rows).
    pub columns: Vec<Column>,
    /// Dialects the query was verified on.
    pub dialects: Vec<String>,
}

fn opt_text(t: Option<&str>) -> Value {
    t.map_or(Value::Null, Value::from)
}

/// The file name of a query: 16 hex digits of the FNV-1a hash of its text.
pub fn file_name(sql: &str) -> String {
    format!("{:016x}.json", fnv1a(sql.as_bytes()))
}

/// The nearest directory at or above `start` that has a `.orbic/sql` cache.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join(CACHE_DIR).is_dir())
        .map(Path::to_path_buf)
}

impl CachedQuery {
    /// The JSON text of the cache file.
    pub fn to_json(&self) -> String {
        let mut v = Value::map();
        v.insert("sql", self.sql.as_str());
        let params: Vec<Value> = self
            .params
            .iter()
            .map(|p| {
                let mut m = Value::map();
                m.insert("type", opt_text(p.ty.as_deref()));
                m
            })
            .collect();
        v.insert("params", Value::List(params));
        let columns: Vec<Value> = self
            .columns
            .iter()
            .map(|c| {
                let mut m = Value::map();
                m.insert("name", c.name.as_str());
                m.insert("type", opt_text(c.ty.as_deref()));
                m.insert("nullable", c.nullable.map_or(Value::Null, Value::from));
                m
            })
            .collect();
        v.insert("columns", Value::List(columns));
        v.insert(
            "dialects",
            Value::List(
                self.dialects
                    .iter()
                    .map(|d| Value::from(d.as_str()))
                    .collect(),
            ),
        );
        to_json(&v)
    }

    /// Parses and checks a cache file.
    pub fn parse(text: &str) -> Result<Self, String> {
        let v = from_json::<Value>(text).map_err(|e| e.to_string())?;
        let s = |v: &Value, k: &str| match v.get(k) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(format!("missing text field `{k}`")),
        };
        let b = |v: &Value, k: &str| match v.get(k) {
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::Null) => Ok(None),
            _ => Err(format!("missing field `{k}` (a boolean or null)")),
        };
        let list = |v: &Value, k: &str| match v.get(k) {
            Some(Value::List(l)) => Ok(l.clone()),
            _ => Err(format!("missing list field `{k}`")),
        };
        let ty = |v: &Value| match v.get("type") {
            Some(Value::Null) => Ok(None),
            Some(Value::String(t)) if TYPES.contains(&t.as_str()) => Ok(Some(t.clone())),
            Some(Value::String(t)) => Err(format!("unknown type `{t}`")),
            _ => Err("missing field `type` (a type name or null)".to_owned()),
        };
        let params = list(&v, "params")?
            .iter()
            .map(|p| Ok(Param { ty: ty(p)? }))
            .collect::<Result<_, String>>()?;
        let columns = list(&v, "columns")?
            .iter()
            .map(|c| {
                Ok(Column {
                    name: s(c, "name")?,
                    ty: ty(c)?,
                    nullable: b(c, "nullable")?,
                })
            })
            .collect::<Result<_, String>>()?;
        let dialects = list(&v, "dialects")?
            .iter()
            .map(|d| match d {
                Value::String(d) if DIALECTS.contains(&d.as_str()) => Ok(d.clone()),
                other => Err(format!("unknown dialect {other:?}")),
            })
            .collect::<Result<_, String>>()?;
        Ok(CachedQuery {
            sql: s(&v, "sql")?,
            params,
            columns,
            dialects,
        })
    }

    /// Writes the cache file under `root/.orbic/sql`.
    pub fn write(&self, root: &Path) -> std::io::Result<PathBuf> {
        let dir = root.join(CACHE_DIR);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(file_name(&self.sql));
        std::fs::write(&path, self.to_json() + "\n")?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_checks() {
        let q = CachedQuery {
            sql: "SELECT id FROM projects WHERE owner_id = $1".into(),
            params: vec![
                Param {
                    ty: Some("text".into()),
                },
                Param { ty: None },
            ],
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: Some("text".into()),
                    nullable: Some(false),
                },
                Column {
                    name: "n".into(),
                    ty: None,
                    nullable: None,
                },
            ],
            dialects: vec!["sqlite".into()],
        };
        assert_eq!(CachedQuery::parse(&q.to_json()).unwrap(), q);
        assert!(
            CachedQuery::parse(&q.to_json().replace("\"text\"", "\"uuid\""))
                .unwrap_err()
                .contains("unknown type")
        );
        assert!(
            CachedQuery::parse(&q.to_json().replace("sqlite", "mysql"))
                .unwrap_err()
                .contains("unknown dialect")
        );
        assert_eq!(file_name("x").len(), 21);
        assert_ne!(file_name("SELECT 1"), file_name("SELECT  1"));
    }
}
