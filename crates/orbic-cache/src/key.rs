//! Keys and tags built from parts (`key!("projects", user.id, cursor.after)`).
//!
//! Parts are joined with `:`; inside a part `\` and `:` are escaped (`\\`,
//! `\:`) and an absent value is `\-`, which no escaped text produces. So two
//! different part lists never give the same key (`docs/01-dx-and-dsl.md` §1,
//! 리터럴).

use std::fmt;

use hyve_core::{Id, Timestamp, Ulid};

/// A value usable as one part of a key or tag.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be part of a cache key or tag",
    label = "not a key part",
    note = "key parts are strings, integers, `bool`, `Ulid`, `Id<T>`, `Timestamp`, or `Option` of those"
)]
pub trait KeyPart {
    /// Appends the escaped part to `out`.
    fn write_part(&self, out: &mut String);
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ':' => out.push_str("\\:"),
            c => out.push(c),
        }
    }
}

impl KeyPart for str {
    fn write_part(&self, out: &mut String) {
        escape(self, out);
    }
}

impl KeyPart for String {
    fn write_part(&self, out: &mut String) {
        escape(self, out);
    }
}

impl<T: KeyPart + ?Sized> KeyPart for &T {
    fn write_part(&self, out: &mut String) {
        (**self).write_part(out);
    }
}

impl<T: KeyPart> KeyPart for Option<T> {
    fn write_part(&self, out: &mut String) {
        match self {
            Some(v) => v.write_part(out),
            None => out.push_str("\\-"),
        }
    }
}

macro_rules! display_parts {
    ($($t:ty),*) => {$(
        impl KeyPart for $t {
            fn write_part(&self, out: &mut String) {
                escape(&self.to_string(), out);
            }
        }
    )*};
}

display_parts!(
    bool, i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, Ulid, Timestamp
);

impl<T> KeyPart for Id<T> {
    fn write_part(&self, out: &mut String) {
        escape(&self.to_string(), out);
    }
}

macro_rules! key_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Starts from the first part.
            pub fn new(first: &(impl KeyPart + ?Sized)) -> Self {
                let mut s = String::new();
                first.write_part(&mut s);
                $name(s)
            }

            /// Appends a part.
            pub fn push(mut self, part: &(impl KeyPart + ?Sized)) -> Self {
                self.0.push(':');
                part.write_part(&mut self.0);
                self
            }

            /// The encoded text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), self.0)
            }
        }
    };
}

key_type!(
    /// A cache key (`key!`).
    CacheKey
);
key_type!(
    /// A tag grouping cache entries for invalidation (`tag!`).
    Tag
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_never_collide() {
        let k = |a: &str, b: &str| CacheKey::new(a).push(b).as_str().to_owned();
        assert_eq!(k("a", "b"), "a:b");
        assert_ne!(k("a:b", "c"), k("a", "b:c"));
        assert_ne!(k("a\\", "b"), k("a", "\\b"));
        assert_eq!(k("a:b\\", "c"), "a\\:b\\\\:c");
        let none: Option<&str> = None;
        assert_ne!(
            CacheKey::new("p").push(&none).as_str(),
            CacheKey::new("p").push("-").as_str()
        );
        assert_ne!(
            CacheKey::new("p").push(&none).as_str(),
            CacheKey::new("p").push("\\-").as_str()
        );
        assert_eq!(
            CacheKey::new("p").push(&Some(7u32)).push(&true).as_str(),
            "p:7:true"
        );
        assert_eq!(Tag::new("projects").push(&3i64).to_string(), "projects:3");
    }
}
