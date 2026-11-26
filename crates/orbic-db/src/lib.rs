//! `orbic-db` — PostgreSQL and SQLite access for orbic (Phase 1).
//!
//! - [`Db`]: a pool; [`Db::tx`] runs a closure in a transaction (rolled back
//!   on error or cancellation); [`Db::in_memory`] is an SQLite database in
//!   memory with the workspace's migrations, for tests.
//! - Chaining queries over `#[table]` models: [`Db::query`], [`Db::insert`],
//!   [`Db::update`], [`Db::delete`].
//! - [`Sql`]: what `sql!` builds, checked against the offline cache and
//!   refused (unsent) on a dialect it was not verified on.
//! - [`Migrations`], and [`testing::TempPostgres`] for PostgreSQL tests.
//! - [`OrbicConfig`]: `[orbic]` in `hyve.toml`; [`Db::from_config`] connects
//!   to its `database_url` (`hyve dev` points it at a local database).
//!
//! PostgreSQL and SQLite use sqlx 0.9.1. Generated tables decode borrowed
//! driver values with Orbic's specialized checked column reader.

mod config;
mod db;
#[cfg(feature = "codegen")]
mod describe;
mod driver;
mod migrate;
mod query;
mod sql;
mod sqlx_row;
mod table;
pub mod testing;
mod value;

pub use config::{DATABASE_URL_ENV, OrbicConfig};
pub use db::{Db, Dialect, Exec, Target, Tx};
#[cfg(feature = "codegen")]
pub use describe::{Described, sql_type};
#[doc(hidden)]
pub use hyve_core::HyveResult as ProjectionResult;
pub use migrate::{Migration, Migrations, workspace_root};
pub use query::{Delete, Order, Select, Update};
pub use sql::{Bind, Encode, Marker, Sql, assert_all_columns, assert_column, bind, types};
pub use table::{Column, Table, column_value, next_column};
pub use value::{DbValue, SqlType, SqlValue};
