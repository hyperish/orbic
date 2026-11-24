//! `Db`: a connection pool on PostgreSQL or SQLite, and transactions.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hyve_core::{Ctx, HyveError, HyveResult};
use sqlx::postgres::{PgPool, PgPoolOptions, Postgres};
use sqlx::sqlite::{Sqlite, SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use tokio::sync::Mutex;

use crate::driver::{self, Text, Want};
use crate::migrate::Migrations;
use crate::sql::Bind;
use crate::table::Table;
use crate::value::DbValue;

/// The SQL dialects orbic supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// PostgreSQL.
    Postgres,
    /// SQLite (and LibSQL).
    Sqlite,
}

impl Dialect {
    /// `postgres` or `sqlite`, as in `hyve.toml` and the `sql!` cache.
    pub const fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
        }
    }

    /// Parses [`name`](Self::name).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "postgres" => Some(Dialect::Postgres),
            "sqlite" => Some(Dialect::Sqlite),
            _ => None,
        }
    }
}

#[derive(Clone)]
enum Pool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

/// A database: a pool of connections. Cheap to clone; provide it to the app
/// (`.provide(db)`) and take it as `Ctx<Db>`.
#[derive(Clone)]
pub struct Db {
    pool: Pool,
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Db({})", self.dialect().name())
    }
}

static MEMORY: AtomicU64 = AtomicU64::new(0);

impl Db {
    /// Connects to `url`: `postgres://...` or `sqlite:...`.
    pub async fn connect(url: &str) -> HyveResult<Db> {
        let pool = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Pool::Postgres(
                PgPoolOptions::new()
                    .connect(url)
                    .await
                    .map_err(driver::error)?,
            )
        } else if url.starts_with("sqlite:") {
            let opts: SqliteConnectOptions = url
                .parse()
                .map_err(|e| HyveError::internal(format!("invalid SQLite URL `{url}`: {e}")))?;
            Pool::Sqlite(
                SqlitePoolOptions::new()
                    .connect_with(opts)
                    .await
                    .map_err(driver::error)?,
            )
        } else {
            return Err(HyveError::internal(format!(
                "unsupported database URL `{url}`; use `postgres://...` or `sqlite:...`"
            )));
        };
        Ok(Db { pool })
    }

    /// A fresh SQLite database in memory, separate from every other one, with
    /// the workspace's migrations applied (the `migrations/` directory next to
    /// the nearest `hyve.toml` at or above the working directory, if any).
    ///
    /// # Panics
    ///
    /// If the database cannot be created or a migration fails: this is a test
    /// and development helper.
    pub async fn in_memory() -> Db {
        let db = Self::empty_in_memory().await;
        let root = std::env::current_dir()
            .ok()
            .and_then(|d| crate::migrate::workspace_root(&d));
        if let Some(root) = root {
            let migrations = Migrations::from_dir(&root.join("migrations"))
                .unwrap_or_else(|e| panic!("Db::in_memory: {e}"));
            db.migrate(&migrations)
                .await
                .unwrap_or_else(|e| panic!("Db::in_memory: {e}"));
        }
        db
    }

    /// A fresh, empty SQLite database in memory.
    pub async fn empty_in_memory() -> Db {
        let name = format!(
            "file:orbic-memory-{}-{}",
            std::process::id(),
            MEMORY.fetch_add(1, Ordering::Relaxed)
        );
        let opts = SqliteConnectOptions::new()
            .filename(&name)
            .in_memory(true)
            .shared_cache(true);
        // The database lives while a connection is open: keep one forever.
        let pool = SqlitePoolOptions::new()
            .min_connections(1)
            .max_connections(4)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .expect("Db::in_memory: SQLite in memory");
        Db {
            pool: Pool::Sqlite(pool),
        }
    }

    /// The dialect.
    pub fn dialect(&self) -> Dialect {
        match self.pool {
            Pool::Sqlite(_) => Dialect::Sqlite,
            Pool::Postgres(_) => Dialect::Postgres,
        }
    }

    /// Applies migrations that have not been applied yet.
    pub async fn migrate(&self, migrations: &Migrations) -> HyveResult<()> {
        crate::migrate::apply(self, migrations).await
    }

    /// Runs `f` in a transaction: committed if it returns `Ok`, rolled back if
    /// it returns `Err` or is cancelled (dropped) before finishing. Using the
    /// handle after the transaction ended fails.
    pub async fn tx<T, F, Fut>(&self, f: F) -> HyveResult<T>
    where
        F: FnOnce(Tx) -> Fut,
        Fut: Future<Output = HyveResult<T>>,
    {
        let conn = match &self.pool {
            Pool::Sqlite(p) => TxConn::Sqlite(p.begin().await.map_err(driver::error)?),
            Pool::Postgres(p) => TxConn::Postgres(p.begin().await.map_err(driver::error)?),
        };
        let tx = Tx {
            inner: Arc::new(Mutex::new(Some(conn))),
            dialect: self.dialect(),
        };
        let out = f(tx.clone()).await;
        let conn = tx.inner.lock().await.take();
        let Some(conn) = conn else {
            return Err(HyveError::internal(
                "the transaction ended while it was in use",
            ));
        };
        match (&out, conn) {
            (Ok(_), TxConn::Sqlite(t)) => t.commit().await.map_err(driver::error)?,
            (Ok(_), TxConn::Postgres(t)) => t.commit().await.map_err(driver::error)?,
            (Err(_), TxConn::Sqlite(t)) => t.rollback().await.map_err(driver::error)?,
            (Err(_), TxConn::Postgres(t)) => t.rollback().await.map_err(driver::error)?,
        }
        out
    }

    /// Runs raw SQL without binds (several statements allowed); for migrations
    /// and test setup.
    pub async fn execute_script(&self, sql: &str) -> HyveResult<()> {
        let sql = sqlx::AssertSqlSafe(sql.to_owned());
        match &self.pool {
            Pool::Sqlite(p) => sqlx::raw_sql(sql).execute(p).await.map(drop),
            Pool::Postgres(p) => sqlx::raw_sql(sql).execute(p).await.map(drop),
        }
        .map_err(driver::error)
    }

    #[cfg_attr(not(feature = "codegen"), allow(dead_code))]
    pub(crate) fn sqlite_pool(&self) -> Option<&SqlitePool> {
        match &self.pool {
            Pool::Sqlite(p) => Some(p),
            Pool::Postgres(_) => None,
        }
    }

    #[cfg_attr(not(feature = "codegen"), allow(dead_code))]
    pub(crate) fn pg_pool(&self) -> Option<&PgPool> {
        match &self.pool {
            Pool::Postgres(p) => Some(p),
            Pool::Sqlite(_) => None,
        }
    }

    /// Closes the pool.
    pub async fn close(&self) {
        match &self.pool {
            Pool::Sqlite(p) => p.close().await,
            Pool::Postgres(p) => p.close().await,
        }
    }
}

enum TxConn {
    Sqlite(sqlx::Transaction<'static, Sqlite>),
    Postgres(sqlx::Transaction<'static, Postgres>),
}

/// A transaction handle from [`Db::tx`]. Clones share the transaction.
#[derive(Clone)]
pub struct Tx {
    inner: Arc<Mutex<Option<TxConn>>>,
    dialect: Dialect,
}

impl fmt::Debug for Tx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tx({})", self.dialect.name())
    }
}

/// Where a query runs: a [`Db`] (any pooled connection) or a [`Tx`].
pub trait Target {
    #[doc(hidden)]
    fn exec(self) -> Exec;
}

#[doc(hidden)]
pub enum Exec {
    Db(Db),
    Tx(Tx),
}

impl Target for Db {
    fn exec(self) -> Exec {
        Exec::Db(self)
    }
}

impl Target for &Db {
    fn exec(self) -> Exec {
        Exec::Db(self.clone())
    }
}

impl Target for Ctx<Db> {
    fn exec(self) -> Exec {
        Exec::Db((*self).clone())
    }
}

impl Target for &Ctx<Db> {
    fn exec(self) -> Exec {
        Exec::Db((**self).clone())
    }
}

impl Target for Tx {
    fn exec(self) -> Exec {
        Exec::Tx(self)
    }
}

impl Target for &Tx {
    fn exec(self) -> Exec {
        Exec::Tx(self.clone())
    }
}

fn ended() -> HyveError {
    HyveError::internal("this transaction already ended; use the handle only inside `db.tx(...)`")
}

impl Exec {
    pub(crate) fn dialect(&self) -> Dialect {
        match self {
            Exec::Db(db) => db.dialect(),
            Exec::Tx(tx) => tx.dialect,
        }
    }

    /// Rows decoded into `M` (columns matched by name).
    pub(crate) async fn fetch<M: Table>(&self, sql: Text, binds: &[Bind]) -> HyveResult<Vec<M>> {
        let want: Vec<Want<'_>> = M::COLUMNS
            .iter()
            .zip(M::TYPES)
            .map(|(name, (ty, _))| Want { name, ty: *ty })
            .collect();
        let rows = self.rows(sql, binds, &want).await?;
        rows.into_iter().map(M::from_values).collect()
    }

    /// Rows as values in `want` order.
    pub(crate) async fn rows(
        &self,
        sql: Text,
        binds: &[Bind],
        want: &[Want<'_>],
    ) -> HyveResult<Vec<Vec<DbValue>>> {
        match self {
            Exec::Db(db) => match &db.pool {
                Pool::Sqlite(p) => {
                    let rows = driver::sqlite::query(&sql, binds)
                        .fetch_all(p)
                        .await
                        .map_err(driver::error)?;
                    rows.iter()
                        .map(|r| driver::sqlite::decode(r, want))
                        .collect()
                }
                Pool::Postgres(p) => {
                    let rows = driver::pg::query(&sql, binds)
                        .fetch_all(p)
                        .await
                        .map_err(driver::error)?;
                    rows.iter().map(|r| driver::pg::decode(r, want)).collect()
                }
            },
            Exec::Tx(tx) => {
                let mut g = tx.inner.lock().await;
                match g.as_mut().ok_or_else(ended)? {
                    TxConn::Sqlite(t) => {
                        let rows = driver::sqlite::query(&sql, binds)
                            .fetch_all(&mut **t)
                            .await
                            .map_err(driver::error)?;
                        rows.iter()
                            .map(|r| driver::sqlite::decode(r, want))
                            .collect()
                    }
                    TxConn::Postgres(t) => {
                        let rows = driver::pg::query(&sql, binds)
                            .fetch_all(&mut **t)
                            .await
                            .map_err(driver::error)?;
                        rows.iter().map(|r| driver::pg::decode(r, want)).collect()
                    }
                }
            }
        }
    }

    /// Raw SQL without binds (several statements allowed).
    pub(crate) async fn script(&self, sql: &str) -> HyveResult<()> {
        match self {
            Exec::Db(db) => db.execute_script(sql).await,
            Exec::Tx(tx) => {
                let sql = sqlx::AssertSqlSafe(sql.to_owned());
                let mut g = tx.inner.lock().await;
                match g.as_mut().ok_or_else(ended)? {
                    TxConn::Sqlite(t) => sqlx::raw_sql(sql).execute(&mut **t).await.map(drop),
                    TxConn::Postgres(t) => sqlx::raw_sql(sql).execute(&mut **t).await.map(drop),
                }
                .map_err(driver::error)
            }
        }
    }

    /// Rows affected.
    pub(crate) async fn execute(&self, sql: Text, binds: &[Bind]) -> HyveResult<u64> {
        let done = match self {
            Exec::Db(db) => match &db.pool {
                Pool::Sqlite(p) => driver::sqlite::query(&sql, binds)
                    .execute(p)
                    .await
                    .map(|r| r.rows_affected()),
                Pool::Postgres(p) => driver::pg::query(&sql, binds)
                    .execute(p)
                    .await
                    .map(|r| r.rows_affected()),
            },
            Exec::Tx(tx) => {
                let mut g = tx.inner.lock().await;
                match g.as_mut().ok_or_else(ended)? {
                    TxConn::Sqlite(t) => driver::sqlite::query(&sql, binds)
                        .execute(&mut **t)
                        .await
                        .map(|r| r.rows_affected()),
                    TxConn::Postgres(t) => driver::pg::query(&sql, binds)
                        .execute(&mut **t)
                        .await
                        .map(|r| r.rows_affected()),
                }
            }
        };
        done.map_err(driver::error)
    }
}

/// The nearest directory at or above `start` holding `hyve.toml`.
pub(crate) fn find_workspace(start: &Path) -> Option<std::path::PathBuf> {
    start
        .ancestors()
        .find(|d| d.join("hyve.toml").is_file())
        .map(Path::to_path_buf)
}
