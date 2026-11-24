//! `orbic-macros`: `ttl!`, `key!`, `tag!`, `#[table]`.

use std::time::Duration;

use orbic::prelude::*;

#[test]
fn ttl_is_checked_at_compile_time() {
    assert_eq!(ttl!("30s"), Duration::from_secs(30));
    assert_eq!(ttl!("1h30m"), Duration::from_secs(5400));
    assert_eq!(ttl!("250ms"), Duration::from_millis(250));
}

#[cfg(feature = "server")]
mod server {
    use hyve::prelude::*;
    use hyve::serde::to_json;
    use orbic::__private::db::{DbValue, SqlType, Table};
    use orbic_tests::{Project, User};

    use super::*;

    #[test]
    fn keys_and_tags_encode_their_parts() {
        let user = User {
            id: Id::new(),
            name: "a:b".into(),
        };
        let after: Option<String> = None;
        let k = key!("projects", user.id, after, user.name, 7, true);
        assert_eq!(k.as_str(), format!("projects:{}:\\-:a\\:b:7:true", user.id));
        assert_eq!(
            tag!("projects", user.id).as_str(),
            format!("projects:{}", user.id)
        );
    }

    #[test]
    fn tables_describe_their_columns() {
        assert_eq!((Project::NAME, Project::PK), ("projects", "id"));
        assert_eq!(
            Project::COLUMNS,
            ["id", "owner_id", "title", "is_public", "version"]
        );
        assert_eq!(Project::TYPES[3], (SqlType::Bool, false));
        assert_eq!(Project::OWNER_ID.name(), "owner_id");

        let p = Project {
            id: Id::new(),
            owner_id: Id::new(),
            title: "t".into(),
            is_public: true,
            version: 1,
        };
        let values = p.values();
        assert_eq!(values[2], DbValue::Text("t".into()));
        assert_eq!(Project::from_values(values).unwrap(), p);
        let short = Project::from_values(vec![DbValue::Text(p.id.to_string())]).unwrap_err();
        assert!(
            short
                .message()
                .contains("projects.owner_id: missing from the row"),
            "{short:?}"
        );
        // `#[pk]` is gone and the model is untouched.
        assert!(to_json(&p).contains("\"ownerId\""));
    }
}

#[test]
fn ui() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}

#[cfg(feature = "server")]
mod sql {
    use hyve::prelude::*;
    use orbic::__private::db::DbValue;
    use orbic_tests::{Project, User};

    use super::*;

    #[test]
    fn queries_bind_their_arguments_as_the_cache_says() {
        let (owner, title) = (Id::<User>::new(), String::from("t"));
        // `docs/01-dx-and-dsl.md` §1 sketch, verbatim.
        let q = sql!(
            Project,
            "INSERT INTO projects (id, owner_id, title, is_public, version)
             VALUES ($1, $2, $3, $4, 1) RETURNING *",
            Id::<Project>::new(),
            owner,
            title,
            false
        );
        assert_eq!(q.dialects(), ["postgres", "sqlite"]);
        assert_eq!(
            q.columns(),
            ["id", "owner_id", "title", "is_public", "version"]
        );
        assert_eq!(
            &q.binds()[1..],
            [
                DbValue::Text(owner.to_string()),
                DbValue::Text("t".into()),
                DbValue::Bool(false)
            ]
        );

        let q = sql!(
            Project,
            "SELECT * FROM projects WHERE owner_id = $1",
            &owner
        );
        assert_eq!(q.dialects(), ["sqlite"], "verified on SQLite only");

        let none: Option<&str> = None;
        let q = sql!(
            "UPDATE projects SET title = $1 WHERE id = $2",
            none,
            Id::<Project>::new()
        );
        assert_eq!(q.binds()[0], DbValue::Null);
        assert!(q.columns().is_empty());
    }

    #[test]
    fn ui() {
        trybuild::TestCases::new().compile_fail("tests/ui-server/*.rs");
    }
}
