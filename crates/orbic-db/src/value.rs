//! Values crossing the database boundary, independent of the dialect.

use hyve_core::{HyveError, Id, Timestamp, Ulid};

/// Column types as `sql!`'s offline cache records them. Drivers map them to
/// their own types (`Int` is `INTEGER`/`int8`, `Timestamp` is
/// `TEXT`/`timestamptz`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlType {
    /// Boolean.
    Bool,
    /// 64-bit signed integer (smaller Rust integers are range-checked).
    Int,
    /// 64-bit float.
    Float,
    /// Text (also identifiers: `Ulid`, `Id<T>`).
    Text,
    /// Bytes.
    Bytes,
    /// A point in time (UTC, microseconds).
    Timestamp,
}

impl SqlType {
    /// The name used in the offline cache.
    pub const fn name(self) -> &'static str {
        match self {
            SqlType::Bool => "bool",
            SqlType::Int => "int",
            SqlType::Float => "float",
            SqlType::Text => "text",
            SqlType::Bytes => "bytes",
            SqlType::Timestamp => "timestamp",
        }
    }

    /// Parses [`name`](Self::name).
    pub fn parse(name: &str) -> Option<Self> {
        [
            SqlType::Bool,
            SqlType::Int,
            SqlType::Float,
            SqlType::Text,
            SqlType::Bytes,
            SqlType::Timestamp,
        ]
        .into_iter()
        .find(|t| t.name() == name)
    }
}

/// A database value.
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    /// `NULL`.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Float.
    Float(f64),
    /// Text.
    Text(String),
    /// Bytes.
    Bytes(Vec<u8>),
    /// Timestamp.
    Timestamp(Timestamp),
}

/// A Rust type stored in one column.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be stored in a database column",
    label = "not a column type",
    note = "column types are `bool`, integers up to `i64`/`u32`, `f32`/`f64`, `String`, `Vec<u8>`, `Ulid`, `Id<T>`, `Timestamp`, and `Option` of those"
)]
pub trait SqlValue: Sized + Send + 'static {
    /// The column type.
    const TYPE: SqlType;
    /// Whether the column may be `NULL` (`Option<T>`).
    const NULLABLE: bool = false;

    /// Encodes the value.
    fn to_db(&self) -> DbValue;

    /// Decodes a value read from a column.
    fn from_db(value: DbValue) -> Result<Self, String>;
}

fn mismatch(expected: &str, found: &DbValue) -> String {
    let found = match found {
        DbValue::Null => "NULL",
        DbValue::Bool(_) => "a boolean",
        DbValue::Int(_) => "an integer",
        DbValue::Float(_) => "a float",
        DbValue::Text(_) => "text",
        DbValue::Bytes(_) => "bytes",
        DbValue::Timestamp(_) => "a timestamp",
    };
    format!("expected {expected}, found {found}")
}

impl SqlValue for bool {
    const TYPE: SqlType = SqlType::Bool;
    fn to_db(&self) -> DbValue {
        DbValue::Bool(*self)
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        match v {
            DbValue::Bool(b) => Ok(b),
            // SQLite stores booleans as integers.
            DbValue::Int(0) => Ok(false),
            DbValue::Int(1) => Ok(true),
            other => Err(mismatch("a boolean", &other)),
        }
    }
}

macro_rules! ints {
    ($($t:ty),*) => {$(
        impl SqlValue for $t {
            const TYPE: SqlType = SqlType::Int;
            fn to_db(&self) -> DbValue {
                DbValue::Int(i64::from(*self))
            }
            fn from_db(v: DbValue) -> Result<Self, String> {
                match v {
                    DbValue::Int(i) => <$t>::try_from(i).map_err(|_| format!("{i} is out of range for {}", stringify!($t))),
                    other => Err(mismatch("an integer", &other)),
                }
            }
        }
    )*};
}

ints!(i8, i16, i32, i64, u8, u16, u32);

impl SqlValue for f64 {
    const TYPE: SqlType = SqlType::Float;
    fn to_db(&self) -> DbValue {
        DbValue::Float(*self)
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        match v {
            DbValue::Float(f) => Ok(f),
            DbValue::Int(i) => Ok(i as f64),
            other => Err(mismatch("a float", &other)),
        }
    }
}

impl SqlValue for f32 {
    const TYPE: SqlType = SqlType::Float;
    fn to_db(&self) -> DbValue {
        DbValue::Float(f64::from(*self))
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        f64::from_db(v).map(|f| f as f32)
    }
}

impl SqlValue for String {
    const TYPE: SqlType = SqlType::Text;
    fn to_db(&self) -> DbValue {
        DbValue::Text(self.clone())
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        match v {
            DbValue::Text(s) => Ok(s),
            other => Err(mismatch("text", &other)),
        }
    }
}

impl SqlValue for Vec<u8> {
    const TYPE: SqlType = SqlType::Bytes;
    fn to_db(&self) -> DbValue {
        DbValue::Bytes(self.clone())
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        match v {
            DbValue::Bytes(b) => Ok(b),
            other => Err(mismatch("bytes", &other)),
        }
    }
}

impl SqlValue for Ulid {
    const TYPE: SqlType = SqlType::Text;
    fn to_db(&self) -> DbValue {
        DbValue::Text(self.to_string())
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        match v {
            DbValue::Text(s) => s.parse().map_err(|_| format!("`{s}` is not a ULID")),
            other => Err(mismatch("a ULID", &other)),
        }
    }
}

impl<T: 'static> SqlValue for Id<T> {
    const TYPE: SqlType = SqlType::Text;
    fn to_db(&self) -> DbValue {
        DbValue::Text(self.to_string())
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        Ulid::from_db(v).map(Id::from_ulid)
    }
}

impl SqlValue for Timestamp {
    const TYPE: SqlType = SqlType::Timestamp;
    fn to_db(&self) -> DbValue {
        DbValue::Timestamp(*self)
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        match v {
            DbValue::Timestamp(t) => Ok(t),
            DbValue::Text(s) => s
                .parse()
                .map_err(|_| format!("`{s}` is not an RFC 3339 timestamp")),
            DbValue::Int(us) => Timestamp::from_unix_micros(us)
                .ok_or_else(|| format!("{us} is out of the timestamp range")),
            other => Err(mismatch("a timestamp", &other)),
        }
    }
}

impl<T: SqlValue> SqlValue for Option<T> {
    const TYPE: SqlType = T::TYPE;
    const NULLABLE: bool = true;
    fn to_db(&self) -> DbValue {
        self.as_ref().map_or(DbValue::Null, T::to_db)
    }
    fn from_db(v: DbValue) -> Result<Self, String> {
        match v {
            DbValue::Null => Ok(None),
            v => T::from_db(v).map(Some),
        }
    }
}

/// Wraps a column decoding failure.
pub(crate) fn column_error(table: &str, column: &str, why: String) -> HyveError {
    HyveError::internal(format!("{table}.{column}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip() {
        fn rt<T: SqlValue + PartialEq + std::fmt::Debug>(v: T) {
            assert_eq!(T::from_db(v.to_db()).unwrap(), v);
        }
        rt(true);
        rt(-5i16);
        rt(u32::MAX);
        rt(0.5f64);
        rt(String::from("x"));
        rt(vec![1u8, 2]);
        rt(Id::<()>::new());
        rt(Some(3i64));
        rt(None::<String>);
        assert!(u8::from_db(DbValue::Int(300)).is_err());
        assert!(bool::from_db(DbValue::Int(1)).unwrap());
        assert_eq!(SqlType::parse("timestamp"), Some(SqlType::Timestamp));
        const { assert!(<Option<u8> as SqlValue>::NULLABLE) };
    }
}
