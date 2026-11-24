//! The temporary PostgreSQL harness (`docs/01-dx-and-dsl.md` §4): each
//! [`TempPostgres`] is a new database on the server named by
//! `HYPERISH_TEST_POSTGRES_URL`, with the workspace's migrations applied,
//! dropped when the harness is dropped. Without the variable it fails rather
//! than skipping.

use std::sync::atomic::{AtomicU64, Ordering};

use hyve_core::{HyveError, HyveResult};

use crate::db::Db;
use crate::migrate::{Migrations, workspace_root};

/// The variable holding the server's URL (a database on it the harness may
/// connect to, usually `postgres`).
pub const POSTGRES_URL_ENV: &str = "HYPERISH_TEST_POSTGRES_URL";

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A temporary PostgreSQL database.
pub struct TempPostgres {
    db: Db,
    server: String,
    name: String,
}

impl TempPostgres {
    /// Creates the database and applies the workspace's migrations.
    ///
    /// # Panics
    ///
    /// If `HYPERISH_TEST_POSTGRES_URL` is not set or the server refuses.
    pub async fn new() -> TempPostgres {
        let migrations = std::env::current_dir()
            .ok()
            .and_then(|d| workspace_root(&d))
            .map(|root| Migrations::from_dir(&root.join("migrations")))
            .transpose()
            .unwrap_or_else(|e| panic!("TempPostgres: {e}"))
            .unwrap_or_default();
        Self::with_migrations(&migrations)
            .await
            .unwrap_or_else(|e| panic!("TempPostgres: {e}"))
    }

    /// Creates the database and applies `migrations`.
    pub async fn with_migrations(migrations: &Migrations) -> HyveResult<TempPostgres> {
        let server = std::env::var(POSTGRES_URL_ENV).map_err(|_| {
            HyveError::internal(format!(
                "{POSTGRES_URL_ENV} is not set; PostgreSQL tests need a server (e.g. postgres://postgres@127.0.0.1:5432/postgres)"
            ))
        })?;
        let name = format!(
            "orbic_test_{}_{}_{:x}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            hyve_core::Ulid::new()
                .to_string()
                .to_ascii_lowercase()
                .chars()
                .rev()
                .take(8)
                .fold(0u64, |h, c| h.wrapping_mul(31).wrapping_add(c as u64))
        );
        let admin = Db::connect(&server).await?;
        admin
            .execute_script(&format!("CREATE DATABASE \"{name}\""))
            .await?;
        admin.close().await;
        let db = Db::connect(&with_database(&server, &name)).await?;
        db.migrate(migrations).await?;
        Ok(TempPostgres { db, server, name })
    }

    /// The database.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Closes the pool and drops the database (dropping the harness does the
    /// same from a thread of its own).
    pub async fn close(mut self) {
        self.db.close().await;
        drop_database(&self.server, &self.name).await;
        self.name.clear();
    }
}

async fn drop_database(server: &str, name: &str) {
    if let Ok(admin) = Db::connect(server).await {
        let _ = admin
            .execute_script(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await;
        admin.close().await;
    }
}

/// `url` with its database replaced by `name`.
fn with_database(url: &str, name: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let base = match base[scheme_end..].find('/') {
        Some(slash) => &base[..scheme_end + slash],
        None => base,
    };
    match query {
        Some(q) => format!("{base}/{name}?{q}"),
        None => format!("{base}/{name}"),
    }
}

impl Drop for TempPostgres {
    fn drop(&mut self) {
        if self.name.is_empty() {
            return;
        }
        // Drop the database this harness created, and nothing else, on a
        // thread with its own runtime: dropping may happen inside a runtime
        // that is now blocked here, so the pool (whose connections belong to
        // that runtime) is not touched; `WITH (FORCE)` ends its sessions.
        let (server, name) = (self.server.clone(), self.name.clone());
        let done = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(drop_database(&server, &name));
        });
        let _ = done.join();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn database_names_replace_the_path() {
        assert_eq!(
            super::with_database("postgres://u@h:1/postgres", "t"),
            "postgres://u@h:1/t"
        );
        assert_eq!(
            super::with_database("postgres://u@h:1/postgres?sslmode=disable", "t"),
            "postgres://u@h:1/t?sslmode=disable"
        );
        assert_eq!(
            super::with_database("postgres://u@h:1", "t"),
            "postgres://u@h:1/t"
        );
    }
}
