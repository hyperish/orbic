//! `orbic-db` — the value and table layer `#[table]` and `sql!` expand to.
//!
//! Drivers, chaining queries, transactions, migrations and `Db::in_memory()`
//! arrive in Phase 1 step 4 (`docs/05-implementation-plan.md` §2).

mod sql;
mod table;
mod value;

pub use sql::{Encode, Sql, assert_all_columns, assert_column, bind, types};
pub use table::{Column, Table, column_value};
pub use value::{DbValue, SqlType, SqlValue};
