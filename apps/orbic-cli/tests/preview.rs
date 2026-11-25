//! `hyve-orbic preview` on a real (tiny) workspace: `stages` lists
//! `preview` only when a server is declared; the server starts, restarts on
//! a source change with the old process gone, is left alone when its
//! binary did not change, is reported when it exits on its own, and is
//! stopped with the tool when the `hyve` lifeline (stdin) closes.
#![cfg(unix)]
#![allow(missing_docs)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn workspace(declared: bool) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("orbic-preview-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let decl = if declared {
        "\n[package.metadata.orbic.server]\nbin = \"srv\"\n"
    } else {
        ""
    };
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"srv\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{decl}\n[workspace]\n"),
    )
    .unwrap();
    server_source(&dir, 1);
    dir
}

/// A server that prints its version and pid, then serves forever (version
/// 3 exits at once, like a server whose port is taken).
fn server_source(dir: &Path, version: u32) {
    std::fs::write(
        dir.join("src/main.rs"),
        format!(
            "fn main() {{\n    println!(\"srv up {version} {{}}\", std::process::id());\n    if {version} == 3 {{ std::process::exit(7); }}\n    loop {{ std::thread::sleep(std::time::Duration::from_secs(1)); }}\n}}\n"
        ),
    )
    .unwrap();
}

fn tool(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hyve-orbic"));
    cmd.current_dir(dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env_remove("CARGO_TARGET_DIR");
    cmd
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn stages_list_preview_only_with_a_declared_server() {
    for (declared, want) in [(false, "codegen"), (true, "codegen\npreview")] {
        let dir = workspace(declared);
        let out = tool(&dir).arg("stages").output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        let stages: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(stages.join("\n"), want);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn preview_runs_restarts_and_stops_the_server() {
    let dir = workspace(true);
    let mut child = tool(&dir)
        .args(["preview", "--web"])
        .env("HYVE_STAGE_LIFELINE", "stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let log = Arc::new(Mutex::new(String::new()));
    let mut err = child.stderr.take().unwrap();
    let log2 = log.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(n) = err.read(&mut buf) {
            if n == 0 {
                break;
            }
            log2.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    let up = |version: u32| -> u32 {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    if let Some(pid) = line.strip_prefix(&format!("srv up {version} ")) {
                        return pid.parse().unwrap();
                    }
                }
                Err(_) => panic!(
                    "no `srv up {version}`; hyve-orbic said:\n{}",
                    log.lock().unwrap()
                ),
            }
        }
    };

    let first = up(1);
    assert!(alive(first));

    // A source change rebuilds and restarts; the old process is gone first.
    std::thread::sleep(Duration::from_millis(1100));
    server_source(&dir, 2);
    let second = up(2);
    assert_ne!(first, second);
    assert!(!alive(first), "the old server was stopped");
    assert!(alive(second));

    // A change that leaves the binary as it was does not restart it.
    std::fs::write(dir.join("NOTES.md"), "not a source of the server\n").unwrap();
    std::thread::sleep(Duration::from_secs(4));
    assert!(rx.try_recv().is_err(), "no restart");
    assert!(alive(second));

    // A server that exits on its own is reported.
    server_source(&dir, 3);
    let third = up(3);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log.lock().unwrap().contains("`srv` exited") {
        assert!(
            Instant::now() < deadline,
            "not reported:\n{}",
            log.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive(third));
    server_source(&dir, 4);
    let fourth = up(4);

    // The lifeline closes: the server stops with the tool.
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "hyve-orbic did not stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status}");
    assert!(!alive(fourth), "the server stopped with the tool");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A running `hyve-orbic preview` under a lifeline, with its stdout lines
/// and stderr text.
struct Preview {
    child: std::process::Child,
    lines: mpsc::Receiver<String>,
    log: Arc<Mutex<String>>,
}

impl Preview {
    fn start(dir: &Path, env: &[(&str, &str)]) -> Preview {
        let mut cmd = tool(dir);
        cmd.args(["preview", "--web"])
            .env("HYVE_STAGE_LIFELINE", "stdin")
            .env_remove("HYPERISH_ORBIC_DATABASE_URL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let (tx, lines) = mpsc::channel::<String>();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let log = Arc::new(Mutex::new(String::new()));
        let mut err = child.stderr.take().unwrap();
        let log2 = log.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                log2.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Preview { child, lines, log }
    }

    /// The pid and database URL of the next `srv up <version>`.
    fn up(&self, version: u32) -> (u32, String) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Some(rest) = line.strip_prefix(&format!("srv up {version} ")) {
                        let (pid, url) = rest.split_once(' ').unwrap();
                        return (pid.parse().unwrap(), url.to_owned());
                    }
                }
                Err(_) => panic!(
                    "no `srv up {version}`; hyve-orbic said:\n{}",
                    self.log.lock().unwrap()
                ),
            }
        }
    }

    fn stop(mut self) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                assert!(s.success(), "{s}");
                return;
            }
            assert!(Instant::now() < deadline, "hyve-orbic did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A server that prints its version, pid and `HYPERISH_ORBIC_DATABASE_URL`,
/// then serves forever. Its `sql!` sit in code that is never compiled, so
/// the crate needs no orbic, but `codegen` finds them.
fn db_server_source(dir: &Path, version: u32, queries: &[&str]) {
    let calls: String = queries
        .iter()
        .map(|q| format!("    sql!({q:?});\n"))
        .collect();
    std::fs::write(
        dir.join("src/main.rs"),
        format!(
            "fn main() {{\n    let url = std::env::var(\"HYPERISH_ORBIC_DATABASE_URL\").unwrap_or_default();\n    println!(\"srv up {version} {{}} {{url}}\", std::process::id());\n    loop {{ std::thread::sleep(std::time::Duration::from_secs(1)); }}\n}}\n\n#[cfg(any())]\nfn queries() {{\n{calls}}}\n"
        ),
    )
    .unwrap();
}

fn db_workspace(dialect: &str) -> PathBuf {
    let dir = workspace(true);
    // A decoy: preview's local database wins over it.
    std::fs::write(
        dir.join("hyve.toml"),
        format!("[orbic]\ndialect = \"{dialect}\"\ndatabase_url = \"sqlite:/nonexistent/decoy.sqlite\"\n"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("migrations")).unwrap();
    std::fs::write(
        dir.join("migrations/0001_notes.sql"),
        "CREATE TABLE notes (id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL);\n",
    )
    .unwrap();
    db_server_source(&dir, 1, &["SELECT id, body FROM notes"]);
    dir
}

fn cached(dir: &Path) -> usize {
    std::fs::read_dir(dir.join(".orbic/sql")).map_or(0, |d| d.count())
}

fn exec(url: &str, sql: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let db = orbic_db::Db::connect(url)
            .await
            .map_err(|e| e.message().to_owned())?;
        let r = db
            .execute_script(sql)
            .await
            .map_err(|e| e.message().to_owned());
        db.close().await;
        r
    })
}

#[test]
fn preview_gives_the_servers_a_local_database_and_keeps_the_sql_cache() {
    let dir = db_workspace("sqlite");
    let preview = Preview::start(&dir, &[]);
    let (first, url) = preview.up(1);
    assert!(
        url.starts_with("sqlite:") && url.contains("orbic-preview") && !url.contains("decoy"),
        "{url}"
    );
    // The migrations were applied, and codegen cached the query.
    exec(&url, "INSERT INTO notes (id, body) VALUES (1, 'kept')").unwrap();
    assert_eq!(cached(&dir), 1);

    // A new migration: applied, and the server restarts on the new schema
    // with the data still there.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(
        dir.join("migrations/0002_tags.sql"),
        "CREATE TABLE tags (id INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL);\n",
    )
    .unwrap();
    let (second, again) = preview.up(1);
    assert_ne!(first, second);
    assert!(!alive(first));
    assert_eq!(again, url, "the same database across restarts");
    exec(&url, "INSERT INTO tags (id, name) VALUES (1, 't')").unwrap();
    let dup = exec(&url, "INSERT INTO notes (id, body) VALUES (1, 'again')").unwrap_err();
    assert!(dup.to_lowercase().contains("unique"), "{dup}");

    // A new `sql!`: cached before the build.
    db_server_source(
        &dir,
        2,
        &["SELECT id, body FROM notes", "SELECT name FROM tags"],
    );
    preview.up(2);
    assert_eq!(cached(&dir), 2);
    preview.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_database_url_in_the_environment_is_passed_through_untouched() {
    let dir = db_workspace("sqlite");
    let theirs = dir.join("theirs.sqlite");
    let url = format!("sqlite:{}?mode=rwc", theirs.display());
    let preview = Preview::start(&dir, &[("HYPERISH_ORBIC_DATABASE_URL", &url)]);
    let (_, got) = preview.up(1);
    assert_eq!(got, url);
    preview.stop();
    assert!(!theirs.exists(), "preview created nothing on it");
    assert!(!dir.join("target/orbic-preview").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_postgres_workspace_gets_its_own_database_on_the_test_server() {
    let server = std::env::var("HYPERISH_TEST_POSTGRES_URL").expect(
        "HYPERISH_TEST_POSTGRES_URL is not set; PostgreSQL tests need a server (e.g. postgres://postgres@127.0.0.1:5432/postgres)",
    );
    let dir = db_workspace("postgres");
    let preview = Preview::start(&dir, &[("HYPERISH_TEST_POSTGRES_URL", &server)]);
    let (_, url) = preview.up(1);
    let name = url
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned();
    assert!(name.starts_with("orbic_preview_"), "{url}");
    exec(
        &url,
        "INSERT INTO notes (id, body) VALUES (1, 'on postgres')",
    )
    .unwrap();
    assert_eq!(cached(&dir), 1);
    preview.stop();
    // A second preview of the same workspace reuses it.
    let preview = Preview::start(&dir, &[("HYPERISH_TEST_POSTGRES_URL", &server)]);
    let (_, again) = preview.up(1);
    assert_eq!(again, url);
    preview.stop();
    let _ = exec(
        &server,
        &format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
