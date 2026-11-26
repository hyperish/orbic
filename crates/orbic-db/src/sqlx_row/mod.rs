//! Specialized sqlx-core 0.9.1 Row::try_get, src/row.rs at
//! 30f4eae0793ecaafbf6808623c84ff57e8001222. See LICENSE-MIT.
//! The caller already fetched the raw value to dispatch width, nulls and
//! timestamps. Reuse that same reference for sqlx type checking and decoding.
use sqlx::{Database, Decode, Error, Type, TypeInfo, ValueRef};

pub(super) fn checked<'r, DB: Database, T: Decode<'r, DB> + Type<DB>>(
    raw: DB::ValueRef<'r>,
    index: usize,
) -> Result<T, Error> {
    let ty = raw.type_info();
    if !ty.is_null() && !T::compatible(&ty) {
        return Err(Error::ColumnDecode {
            index: index.to_string(),
            source: sqlx::error::mismatched_types::<DB, T>(&ty),
        });
    }
    drop(ty);
    T::decode(raw).map_err(|source| Error::ColumnDecode {
        index: index.to_string(),
        source,
    })
}
