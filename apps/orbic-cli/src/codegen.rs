//! The `codegen` stage: the `sql!` offline cache.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use hyve::prelude::*;
use hyve_config::Loader;
use orbic_db::testing::TempPostgres;
use orbic_db::{Db, Described, Dialect, Migrations, SqlType};
use orbic_sql_cache::{CACHE_DIR, CachedQuery, Column, Param, file_name, scan};

/// `[orbic]` in `hyve.toml`.
#[model]
#[derive(Debug, Clone, Default)]
pub struct OrbicConfig {
    /// The operational dialect: `postgres` or `sqlite`.
    pub dialect: Option<String>,
    /// Paths (relative to the workspace root) whose sources are not scanned,
    /// e.g. compile-fail tests.
    pub codegen_exclude: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Options {
    pub check: bool,
    pub prune: bool,
    pub root: Option<PathBuf>,
}

pub async fn run(opts: Options) -> Result<String, String> {
    let root = match opts.root {
        Some(r) => r,
        None => {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            hyve_config::find_root(&cwd).ok_or_else(|| {
                format!(
                    "no hyve.toml at or above {}; run inside a hyve workspace",
                    cwd.display()
                )
            })?
        }
    };
    let config = Loader::new()
        .dir(&root)
        .args(Vec::<String>::new())
        .section("orbic")
        .load::<OrbicConfig>()
        .map_err(|e| e.to_string())?
        .into_inner();

    let mut queries: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut report = String::new();
    let excluded: Vec<PathBuf> = config
        .codegen_exclude
        .iter()
        .map(|p| root.join(p))
        .collect();
    for file in rust_files(&root, &excluded) {
        let text =
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        if !text.contains("sql!") && !text.contains("sql !") {
            continue;
        }
        let shown = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .display()
            .to_string();
        let Ok((found, bad)) = scan(&text) else {
            continue;
        };
        for f in found {
            queries
                .entry(f.sql)
                .or_default()
                .push(format!("{shown}:{}", f.line));
        }
        for (line, why) in bad {
            let _ = writeln!(
                report,
                "warning: {shown}:{line}: skipped a `sql!` that does not parse: {why}"
            );
        }
    }

    let cache_dir = root.join(CACHE_DIR);
    let mut generated: BTreeMap<String, String> = BTreeMap::new();
    if !queries.is_empty() {
        let Some(dialect) = config.dialect.as_deref() else {
            return Err("the workspace uses `sql!`; declare the operational dialect: `[orbic] dialect = \"postgres\"` (or \"sqlite\") in hyve.toml".into());
        };
        let dialect = Dialect::parse(dialect).ok_or_else(|| {
            format!("[orbic] dialect = {dialect:?}: use \"postgres\" or \"sqlite\"")
        })?;
        let migrations =
            Migrations::from_dir(&root.join("migrations")).map_err(|e| e.message().to_owned())?;
        let engines = Engines::open(dialect, &migrations).await?;
        let mut failures = Vec::new();
        for (sql, sites) in &queries {
            match engines.analyze(sql).await {
                Ok(q) => {
                    generated.insert(file_name(sql), q.to_json() + "\n");
                }
                Err(why) => failures.push(format!("{}: {why}\n    {sql}", sites.join(", "))),
            }
        }
        engines.close().await;
        if !failures.is_empty() {
            return Err(format!(
                "{} quer{} failed on {}:\n  {}",
                failures.len(),
                if failures.len() == 1 { "y" } else { "ies" },
                dialect.name(),
                failures.join("\n  ")
            ));
        }
    }

    let existing: Vec<String> = std::fs::read_dir(&cache_dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default();
    let unused: Vec<&String> = existing
        .iter()
        .filter(|n| !generated.contains_key(*n))
        .collect();

    if opts.check {
        let stale: Vec<&String> = generated
            .iter()
            .filter(|(name, text)| {
                std::fs::read_to_string(cache_dir.join(name)).ok().as_ref() != Some(*text)
            })
            .map(|(name, _)| name)
            .collect();
        if !stale.is_empty() {
            let names: Vec<&str> = stale.iter().map(|s| s.as_str()).collect();
            return Err(format!(
                "the `sql!` cache is out of date ({}); run `hyve codegen` and commit `{CACHE_DIR}/`",
                names.join(", ")
            ));
        }
        let _ = writeln!(
            report,
            "{CACHE_DIR}: {} quer{} up to date",
            generated.len(),
            plural(generated.len())
        );
    } else {
        if !generated.is_empty() {
            std::fs::create_dir_all(&cache_dir)
                .map_err(|e| format!("{}: {e}", cache_dir.display()))?;
        }
        let mut written = 0;
        for (name, text) in &generated {
            let path = cache_dir.join(name);
            if std::fs::read_to_string(&path).ok().as_ref() != Some(text) {
                std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
                written += 1;
            }
        }
        if opts.prune {
            for n in &unused {
                std::fs::remove_file(cache_dir.join(n)).map_err(|e| format!("{n}: {e}"))?;
            }
        }
        let _ = writeln!(
            report,
            "{CACHE_DIR}: {} quer{}, {written} written",
            generated.len(),
            plural(generated.len())
        );
    }
    if !unused.is_empty() && !opts.prune {
        let _ = writeln!(
            report,
            "note: {} file(s) in {CACHE_DIR} match no `sql!` (remove them with --prune)",
            unused.len()
        );
    }
    Ok(report)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

/// `.rs` files under `root`, skipping `target`, hidden directories and
/// `excluded` paths.
fn rust_files(root: &Path, excluded: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || name == "target"
                || excluded.iter().any(|x| path.starts_with(x))
            {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if name.ends_with(".rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The databases queries are analyzed on: the operational dialect (must
/// pass) and, for PostgreSQL projects, SQLite (recorded where it agrees).
struct Engines {
    operational: (Dialect, Db, Option<TempPostgres>),
    sqlite: Option<Db>,
}

impl Engines {
    async fn open(dialect: Dialect, migrations: &Migrations) -> Result<Engines, String> {
        let sqlite = Db::empty_in_memory().await;
        sqlite
            .migrate(migrations)
            .await
            .map_err(|e| format!("migrations on sqlite: {}", e.message()))?;
        Ok(match dialect {
            Dialect::Sqlite => Engines {
                operational: (dialect, sqlite, None),
                sqlite: None,
            },
            Dialect::Postgres => {
                let pg = TempPostgres::with_migrations(migrations)
                    .await
                    .map_err(|e| e.message().to_owned())?;
                Engines {
                    operational: (dialect, pg.db().clone(), Some(pg)),
                    sqlite: Some(sqlite),
                }
            }
        })
    }

    async fn close(self) {
        if let (_, _, Some(pg)) = self.operational {
            pg.close().await;
        }
    }

    async fn analyze(&self, sql: &str) -> Result<CachedQuery, String> {
        let (dialect, db, _) = &self.operational;
        let main = db.describe(sql).await.map_err(|e| e.message().to_owned())?;
        let mut dialects = vec![dialect.name()];
        let mut merged = main.clone();
        if let Some(lite) = &self.sqlite
            && let Ok(other) = lite.describe(sql).await
            && let Some(m) = merge(&main, &other)
        {
            merged = m;
            dialects.push(Dialect::Sqlite.name());
        }
        dialects.sort_unstable();
        Ok(CachedQuery {
            sql: sql.to_owned(),
            params: merged
                .params
                .iter()
                .map(|t| Param {
                    ty: t.map(|t| t.name().to_owned()),
                })
                .collect(),
            columns: merged
                .columns
                .iter()
                .map(|(name, ty, nullable)| Column {
                    name: name.clone(),
                    ty: ty.map(|t| t.name().to_owned()),
                    nullable: *nullable,
                })
                .collect(),
            dialects: dialects.into_iter().map(str::to_owned).collect(),
        })
    }
}

/// Facts both engines agree on; `None` if they disagree (the second dialect
/// is then not recorded as verified). A type one engine does not know is
/// taken from the other; a column is nullable if either says so, and of
/// unknown nullability if either does not know.
fn merge(main: &Described, other: &Described) -> Option<Described> {
    if main.params.len() != other.params.len() || main.columns.len() != other.columns.len() {
        return None;
    }
    let ty = |a: Option<SqlType>, b: Option<SqlType>| match (a, b) {
        (Some(a), Some(b)) if a != b => Err(()),
        (a, b) => Ok(a.or(b)),
    };
    let params = main
        .params
        .iter()
        .zip(&other.params)
        .map(|(a, b)| ty(*a, *b))
        .collect::<Result<_, _>>()
        .ok()?;
    let columns = main
        .columns
        .iter()
        .zip(&other.columns)
        .map(|((n, ta, na), (m, tb, nb))| {
            if n != m {
                return Err(());
            }
            let nullable = match (na, nb) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            };
            Ok((n.clone(), ty(*ta, *tb)?, nullable))
        })
        .collect::<Result<_, _>>()
        .ok()?;
    Some(Described { params, columns })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engines_merge_what_they_agree_on() {
        let d = |params: Vec<Option<SqlType>>,
                 columns: Vec<(&str, Option<SqlType>, Option<bool>)>| Described {
            params,
            columns: columns
                .into_iter()
                .map(|(n, t, b)| (n.to_owned(), t, b))
                .collect(),
        };
        let pg = d(
            vec![Some(SqlType::Text)],
            vec![("n", Some(SqlType::Int), None), ("t", None, Some(false))],
        );
        let lite = d(
            vec![None],
            vec![
                ("n", Some(SqlType::Int), Some(false)),
                ("t", Some(SqlType::Timestamp), Some(false)),
            ],
        );
        let m = merge(&pg, &lite).unwrap();
        assert_eq!(m.params, [Some(SqlType::Text)]);
        assert_eq!(m.columns[0], ("n".into(), Some(SqlType::Int), None));
        assert_eq!(
            m.columns[1],
            ("t".into(), Some(SqlType::Timestamp), Some(false))
        );
        let wrong = d(
            vec![Some(SqlType::Int)],
            vec![("n", Some(SqlType::Int), None), ("t", None, None)],
        );
        assert!(merge(&pg, &wrong).is_none());
    }
}
