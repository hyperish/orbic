//! Migrations: `.sql` files in the workspace's `migrations/`, applied in name
//! order, each once (`docs/01-dx-and-dsl.md` §4, 설정과 위치).
//!
//! `0001_projects.sql` applies on every dialect; `0002_x.postgres.sql` and
//! `0002_x.sqlite.sql` apply only on that dialect, for DDL the two engines
//! spell differently. Applied migrations are recorded in `_orbic_migrations`
//! with a checksum; editing one after it was applied is an error.

use std::path::{Path, PathBuf};

use hyve_core::{HyveError, HyveResult};
use hyve_serde::fnv1a;

use crate::db::{Db, Dialect};

/// One migration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The file name.
    pub name: String,
    /// The only dialect it applies to, if any.
    pub dialect: Option<Dialect>,
    /// The SQL (several statements allowed).
    pub sql: String,
}

impl Migration {
    fn checksum(&self) -> String {
        format!("{:016x}", fnv1a(self.sql.as_bytes()))
    }
}

/// Migrations in application order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Migrations(Vec<Migration>);

impl Migrations {
    /// The `.sql` files of `dir` in name order; none if `dir` does not exist.
    pub fn from_dir(dir: &Path) -> HyveResult<Self> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(Migrations::default());
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let sql = std::fs::read_to_string(&p).map_err(|e| {
                    HyveError::internal(format!("cannot read migration {}: {e}", p.display()))
                })?;
                let stem = name.trim_end_matches(".sql");
                let dialect = stem.rsplit_once('.').and_then(|(_, d)| Dialect::parse(d));
                Ok(Migration { name, dialect, sql })
            })
            .collect::<HyveResult<_>>()
            .map(Migrations)
    }

    /// Migrations from `(name, sql)` pairs, in the given order.
    pub fn new(list: impl IntoIterator<Item = (String, String)>) -> Self {
        Migrations(
            list.into_iter()
                .map(|(name, sql)| {
                    let dialect = name
                        .trim_end_matches(".sql")
                        .rsplit_once('.')
                        .and_then(|(_, d)| Dialect::parse(d));
                    Migration { name, dialect, sql }
                })
                .collect(),
        )
    }

    /// The migrations that apply to `dialect`.
    pub fn for_dialect(&self, dialect: Dialect) -> impl Iterator<Item = &Migration> {
        self.0
            .iter()
            .filter(move |m| m.dialect.is_none_or(|d| d == dialect))
    }
}

/// The nearest directory at or above `start` holding `hyve.toml`.
pub fn workspace_root(start: &Path) -> Option<PathBuf> {
    crate::db::find_workspace(start)
}

const TABLE: &str = "CREATE TABLE IF NOT EXISTS _orbic_migrations (name TEXT PRIMARY KEY NOT NULL, checksum TEXT NOT NULL)";

pub(crate) async fn apply(db: &Db, migrations: &Migrations) -> HyveResult<()> {
    db.execute_script(TABLE).await?;
    let applied = crate::query::applied_migrations(db).await?;
    for m in migrations.for_dialect(db.dialect()) {
        let sum = m.checksum();
        match applied.iter().find(|(n, _)| *n == m.name) {
            Some((_, s)) if *s == sum => continue,
            Some(_) => {
                return Err(HyveError::internal(format!(
                    "migration `{}` changed after it was applied; add a new migration instead",
                    m.name
                )));
            }
            None => {}
        }
        let (name, sql) = (m.name.clone(), m.sql.clone());
        db.tx(|tx| async move {
            tx.execute_script(&sql).await.map_err(|e| {
                HyveError::internal(format!("migration `{name}` failed: {}", e.message()))
            })?;
            crate::query::record_migration(&tx, &name, &sum).await
        })
        .await?;
    }
    Ok(())
}
