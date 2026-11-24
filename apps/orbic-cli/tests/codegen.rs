//! `hyve-orbic codegen` on throwaway SQLite workspaces, and `--check` on this
//! repository's fixture workspace (PostgreSQL: `HYPERISH_TEST_POSTGRES_URL`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn hyve_orbic(args: &[&str], root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hyve-orbic"))
        .args(args)
        .arg("--root")
        .arg(root)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn workspace(name: &str, hyve_toml: &str, source: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("hyve-orbic-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    std::fs::write(root.join("hyve.toml"), hyve_toml).unwrap();
    std::fs::write(
        root.join("migrations/0001_items.sql"),
        "CREATE TABLE items (id TEXT PRIMARY KEY NOT NULL, n BIGINT);",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), source).unwrap();
    root
}

const SQLITE: &str = "[orbic]\ndialect = \"sqlite\"\n";

#[test]
fn writes_checks_and_prunes_the_cache() {
    let root = workspace(
        "write",
        SQLITE,
        r#"fn f() { let q = sql!(Item, "SELECT id, n FROM items WHERE n > $1", 1); }"#,
    );
    let o = hyve_orbic(&["codegen", "--check"], &root);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("out of date"), "{}", text(&o));

    let o = hyve_orbic(&["codegen"], &root);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("1 query, 1 written"), "{}", text(&o));
    let files: Vec<_> = std::fs::read_dir(root.join(".orbic/sql"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let cached = std::fs::read_to_string(&files[0]).unwrap();
    assert_eq!(
        cached,
        "{\"sql\":\"SELECT id, n FROM items WHERE n > $1\",\"params\":[{\"type\":null}],\"columns\":[{\"name\":\"id\",\"type\":\"text\",\"nullable\":false},{\"name\":\"n\",\"type\":\"int\",\"nullable\":true}],\"dialects\":[\"sqlite\"]}\n",
        "SQLite does not type parameters"
    );
    let o = hyve_orbic(&["codegen", "--check"], &root);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("1 query up to date"), "{}", text(&o));

    std::fs::write(&files[0], cached.replace("\"int\"", "\"text\"")).unwrap();
    assert_eq!(
        hyve_orbic(&["codegen", "--check"], &root).status.code(),
        Some(1),
        "an edited file is stale"
    );

    std::fs::write(root.join(".orbic/sql/0000000000000000.json"), "{}").unwrap();
    let o = hyve_orbic(&["codegen"], &root);
    assert!(
        text(&o).contains("1 file(s) in .orbic/sql match no `sql!`"),
        "{}",
        text(&o)
    );
    hyve_orbic(&["codegen", "--prune"], &root);
    assert!(!root.join(".orbic/sql/0000000000000000.json").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reports_failing_queries_and_missing_settings() {
    let root = workspace(
        "fail",
        SQLITE,
        "fn f() {\n    let q = sql!(\"SELECT nope FROM items\");\n}\n",
    );
    let o = hyve_orbic(&["codegen"], &root);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o).contains("src/lib.rs:2") && text(&o).contains("nope"),
        "{}",
        text(&o)
    );

    let root = workspace("nodialect", "", r#"fn f() { sql!("SELECT 1"); }"#);
    let o = hyve_orbic(&["codegen"], &root);
    assert!(
        text(&o).contains("declare the operational dialect"),
        "{}",
        text(&o)
    );

    let o = Command::new(env!("CARGO_BIN_EXE_hyve-orbic"))
        .arg("build")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    let o = Command::new(env!("CARGO_BIN_EXE_hyve-orbic"))
        .args(["codegen", "--root"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}

/// The committed `.orbic/sql/` of this repository's fixture workspace is
/// what the engines report (the same check CI runs).
#[test]
fn the_fixture_cache_is_up_to_date() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let o = hyve_orbic(&["codegen", "--check"], &root);
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn stages_answer_hyve() {
    let o = Command::new(env!("CARGO_BIN_EXE_hyve-orbic"))
        .arg("stages")
        .output()
        .unwrap();
    assert!(o.status.success());
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        format!("{}\ncodegen\n", env!("CARGO_PKG_VERSION"))
    );
    let o = Command::new(env!("CARGO_BIN_EXE_hyve-orbic"))
        .args(["stages", "extra"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}
