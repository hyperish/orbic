//! `hyve-orbic dev` (`docs/01-dx-and-dsl.md` §3, 로컬 개발 환경):
//! builds every server the workspace declares
//! (`[package.metadata.orbic.server]`), runs the built binaries, and when a
//! source changes rebuilds them and restarts each one whose binary changed.
//! Under `hyve dev` (`HYVE_STAGE_LIFELINE=stdin`) it stops the servers
//! and exits when its stdin closes.
//!
//! In a hyve workspace (a `hyve.toml`), it also keeps the `sql!` cache
//! current (`hyve-orbic codegen`, as a child process, before each build) and
//! gives the servers a local database through `HYPERISH_ORBIC_DATABASE_URL`,
//! which wins over `[orbic] database_url`: SQLite in the target directory,
//! or a database on the `HYPERISH_TEST_POSTGRES_URL` server when the
//! operational dialect is PostgreSQL. It keeps its data across restarts and
//! gets the workspace's migrations, at start and when `migrations/` changes.
//! When the user sets `HYPERISH_ORBIC_DATABASE_URL`, the CLI passes it to
//! the server and skips local database setup.
//!
//! ```toml
//! [package.metadata.orbic.server]
//! bin = "my-server"          # default: the package's only binary
//! features = ["server"]      # optional
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hyve::Value;
use orbic_db::testing::{POSTGRES_URL_ENV, with_database};
use orbic_db::{DATABASE_URL_ENV, Db, Dialect, Migrations, OrbicConfig};

/// A server a package declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Server {
    pub(crate) package: String,
    pub(crate) bin: String,
    pub(crate) cache_features: Vec<String>,
    pub(crate) features: Vec<String>,
}

/// What `dev` needs from `cargo metadata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    pub(crate) target: PathBuf,
    /// Member package directories (the watched sources).
    pub(crate) members: Vec<PathBuf>,
    pub(crate) servers: Vec<Server>,
}

fn cargo() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn string(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

fn list(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::List(items)) => items,
        _ => &[],
    }
}

/// The workspace enclosing the working directory.
pub(crate) fn workspace() -> Result<Workspace, String> {
    let out = Command::new(cargo())
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(format!("`cargo metadata` failed: {}", why.trim()));
    }
    let meta = hyve::serde::from_json::<Value>(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| format!("unexpected `cargo metadata` output: {e}"))?;
    parse(&meta)
}

/// Reads the members and declared servers out of `cargo metadata`.
pub(crate) fn parse(meta: &Value) -> Result<Workspace, String> {
    let root = string(meta.get("workspace_root")).ok_or("`cargo metadata`: no workspace_root")?;
    let target =
        string(meta.get("target_directory")).ok_or("`cargo metadata`: no target_directory")?;
    let mut members = Vec::new();
    let mut servers = Vec::new();
    for p in list(meta.get("packages")) {
        let name = string(p.get("name")).unwrap_or_default().to_owned();
        if let Some(dir) = string(p.get("manifest_path")).and_then(|m| Path::new(m).parent()) {
            members.push(dir.to_path_buf());
        }
        let declared = p
            .get("metadata")
            .and_then(|m| m.get("orbic"))
            .and_then(|o| o.get("server"));
        let Some(declared) = declared else {
            continue;
        };
        if !matches!(declared, Value::Map(_)) {
            return Err(format!(
                "`[package.metadata.orbic.server]` of `{name}` must be a table: bin = \"<binary>\", features = [..]"
            ));
        }
        let bins: Vec<&str> = list(p.get("targets"))
            .iter()
            .filter(|t| {
                list(t.get("kind"))
                    .iter()
                    .any(|k| matches!(k, Value::String(k) if k == "bin"))
            })
            .filter_map(|t| string(t.get("name")))
            .collect();
        let bin = match string(declared.get("bin")) {
            Some(b) if bins.contains(&b) => b.to_owned(),
            Some(b) => {
                return Err(format!(
                    "`{name}` declares the server binary `{b}`, but its binaries are: {}",
                    bins.join(", ")
                ));
            }
            None => match bins.as_slice() {
                [one] => (*one).to_owned(),
                _ if bins.contains(&name.as_str()) => name.clone(),
                _ => {
                    return Err(format!(
                        "`{name}` has several binaries; name the server: [package.metadata.orbic.server] bin = \"..\""
                    ));
                }
            },
        };
        let features = list(declared.get("features"))
            .iter()
            .filter_map(|f| string(Some(f)).map(str::to_owned))
            .collect();
        let cache_features = match declared.get("cache_features") {
            Some(Value::List(features)) => features
                .iter()
                .map(|f| {
                    string(Some(f))
                        .map(str::to_owned)
                        .ok_or("cache_features must contain feature names")
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(format!("cache_features for `{name}` must be a list")),
            None if p
                .get("features")
                .and_then(|f| f.get("cache-redis"))
                .is_some() =>
            {
                vec!["cache-redis".into()]
            }
            None => Vec::new(),
        };
        servers.push(Server {
            package: name,
            cache_features,
            bin,
            features,
        });
    }
    Ok(Workspace {
        root: root.into(),
        target: target.into(),
        members,
        servers,
    })
}

/// A cheap fingerprint of the sources: newest modification time and file
/// count, skipping the target directory and hidden directories.
fn sources_stamp(dirs: &[PathBuf], target: &Path) -> (Option<SystemTime>, usize) {
    fn walk(dir: &Path, target: &Path, newest: &mut Option<SystemTime>, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let hidden = e.file_name().to_string_lossy().starts_with('.');
            if p.is_dir() {
                if !hidden && p != target {
                    walk(&p, target, newest, n);
                }
            } else if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                *n += 1;
                if newest.is_none_or(|t| m > t) {
                    *newest = Some(m);
                }
            }
        }
    }
    let (mut newest, mut n) = (None, 0);
    for d in dirs {
        walk(d, target, &mut newest, &mut n);
    }
    (newest, n)
}

/// The local database the servers get, in a hyve workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Harness {
    /// The directory with `hyve.toml` (migrations, `.orbic/`).
    pub(crate) root: PathBuf,
    /// What the servers get as `HYPERISH_ORBIC_DATABASE_URL`, or `None` when
    /// the user's own value is passed through.
    pub(crate) url: Option<String>,
}

/// FNV-1a of the workspace root identifies its persistent development database.
/// The prefix is part of the database name used by earlier CLI releases.
fn workspace_hash(root: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn workspace_database_name(root: &Path) -> String {
    format!("orbic_preview_{:016x}", workspace_hash(root))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Decides the servers' database. `env` is the variable lookup (the
/// process environment outside tests).
pub(crate) fn harness(
    ws: &Workspace,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Harness>, String> {
    let Some(root) = hyve_config::find_root(&ws.root) else {
        return Ok(None);
    };
    if env(DATABASE_URL_ENV).is_some() {
        eprintln!(
            "hyve-orbic: the servers use {DATABASE_URL_ENV} from the environment (skipping local database setup)"
        );
        return Ok(Some(Harness { root, url: None }));
    }
    let config = OrbicConfig::load_with(hyve_config::Loader::new().dir(&root))
        .map_err(|e| e.message().to_owned())?;
    let dialect = config.dialect.as_deref().and_then(Dialect::parse);
    if dialect == Some(Dialect::Postgres) {
        if let Some(server) = env(POSTGRES_URL_ENV) {
            let name = workspace_database_name(&root);
            runtime()
                .block_on(async {
                    let admin = Db::connect(&server).await?;
                    let made = admin
                        .execute_script(&format!("CREATE DATABASE \"{name}\""))
                        .await;
                    admin.close().await;
                    match made {
                        Err(e) if !e.message().contains("already exists") => Err(e),
                        _ => Ok(()),
                    }
                })
                .map_err(|e| format!("the local development database `{name}`: {}", e.message()))?;
            return Ok(Some(Harness {
                url: Some(with_database(&server, &name)),
                root,
            }));
        }
        eprintln!(
            "hyve-orbic: the operational dialect is PostgreSQL but {POSTGRES_URL_ENV} is not set; the local development database is SQLite, where a `sql!` verified only on PostgreSQL returns an error"
        );
    }
    // Earlier CLI releases store the workspace's SQLite data here as well.
    let dir = ws.target.join("orbic-preview");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let url = format!("sqlite:{}?mode=rwc", dir.join("db.sqlite").display());
    Ok(Some(Harness {
        root,
        url: Some(url),
    }))
}

/// Applies the workspace's migrations to the local development database.
fn migrate(h: &Harness) -> Result<(), String> {
    let (Some(url), dir) = (&h.url, h.root.join("migrations")) else {
        return Ok(());
    };
    if !dir.is_dir() {
        return Ok(());
    }
    runtime().block_on(async {
        let migrations = Migrations::from_dir(&dir).map_err(|e| e.message().to_owned())?;
        let db = Db::connect(url).await.map_err(|e| e.message().to_owned())?;
        let done = db.migrate(&migrations).await;
        db.close().await;
        done.map_err(|e| e.message().to_owned())
    })
}

/// `hyve-orbic codegen` as a child (it finishes, and drops any temporary
/// database, even if the development session stops meanwhile). Quiet when nothing
/// changed; a failure is reported and the build goes on.
fn codegen(root: &Path) {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let out = Command::new(me)
        .arg("codegen")
        .arg("--root")
        .arg(root)
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let quiet = text
                .lines()
                .all(|l| l.ends_with(", 0 written") && !l.starts_with("warning"));
            if !quiet {
                for line in text.lines() {
                    eprintln!("hyve-orbic: codegen: {line}");
                }
            }
        }
        Ok(o) => eprintln!(
            "hyve-orbic: codegen failed; a `sql!` it could not cache fails the build:\n{}",
            String::from_utf8_lossy(&o.stderr).trim_end()
        ),
        Err(e) => eprintln!("hyve-orbic: codegen could not run: {e}"),
    }
}

fn binary(ws: &Workspace, s: &Server) -> PathBuf {
    ws.target
        .join("debug")
        .join(format!("{}{}", s.bin, std::env::consts::EXE_SUFFIX))
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// `cargo build` of the server (its errors go to the terminal).
fn build(ws: &Workspace, s: &Server, shared_cache: bool) -> bool {
    let mut cmd = Command::new(cargo());
    cmd.args(["build", "-p", &s.package, "--bin", &s.bin])
        .current_dir(&ws.root)
        .stdin(Stdio::null());
    let mut features = s.features.clone();
    if shared_cache {
        features.extend_from_slice(&s.cache_features);
    }
    if !features.is_empty() {
        cmd.arg("--features").arg(features.join(","));
    }
    cmd.status().is_ok_and(|st| st.success())
}

/// The running servers; `stopping` once the lifeline closed.
#[derive(Default)]
struct Running {
    children: Vec<Option<Child>>,
    stopping: bool,
}

fn stop(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Runs the servers until stopped (Ctrl-C, or the lifeline closing).
pub(crate) fn run(ws: &Workspace) -> Result<(), String> {
    if ws.servers.is_empty() {
        return Err(
            "declare a local server in its package's Cargo.toml, `[package.metadata.orbic.server]` with `bin = \"<binary>\"`"
                .into(),
        );
    }
    let harness = harness(ws, |k| std::env::var(k).ok())?;
    let cache_url = std::env::var_os("HYPERISH_ORBIC_CACHE_URL")
        .or_else(|| std::env::var_os("HYPERISH_TEST_REDIS_URL"));
    let cache_namespace = std::env::var_os("HYPERISH_ORBIC_CACHE_NAMESPACE").unwrap_or_else(|| {
        let root = ws.root.canonicalize().unwrap_or_else(|_| ws.root.clone());
        format!("dev-{:016x}", workspace_hash(&root)).into()
    });
    let running = Arc::new(Mutex::new(Running {
        children: ws.servers.iter().map(|_| None).collect(),
        stopping: false,
    }));
    if std::env::var("HYVE_STAGE_LIFELINE").as_deref() == Ok("stdin") {
        let running = running.clone();
        std::thread::spawn(move || {
            let mut sink = [0u8; 256];
            let mut stdin = std::io::stdin();
            while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
            let mut r = running.lock().unwrap_or_else(|e| e.into_inner());
            r.stopping = true;
            for c in &mut r.children {
                stop(c);
            }
            std::process::exit(0);
        });
    }
    let start = |i: usize, s: &Server| -> Option<SystemTime> {
        let path = binary(ws, s);
        let mut r = running.lock().unwrap_or_else(|e| e.into_inner());
        if r.stopping {
            return None;
        }
        // The old process is gone before the new one binds its port.
        stop(&mut r.children[i]);
        let mut cmd = Command::new(&path);
        cmd.current_dir(&ws.root).stdin(Stdio::null());
        if let Some(url) = harness.as_ref().and_then(|h| h.url.as_ref()) {
            cmd.env(DATABASE_URL_ENV, url);
        }
        if let Some(url) = &cache_url {
            cmd.env("HYPERISH_ORBIC_CACHE_URL", url)
                .env("HYPERISH_ORBIC_CACHE_NAMESPACE", &cache_namespace);
        }
        match cmd.spawn() {
            Ok(child) => {
                eprintln!("hyve-orbic: `{}` is running", s.bin);
                r.children[i] = Some(child);
                modified(&path)
            }
            Err(e) => {
                eprintln!("hyve-orbic: `{}` could not start: {e}", s.bin);
                None
            }
        }
    };
    let migrations = harness.as_ref().map(|h| h.root.join("migrations"));
    let prepare = |migrations_changed: bool| {
        if let Some(h) = &harness {
            if migrations_changed && let Err(e) = migrate(h) {
                eprintln!("hyve-orbic: the migrations failed: {e}");
            }
            codegen(&h.root);
        }
    };
    prepare(true);
    let mut running_build: Vec<Option<SystemTime>> = vec![None; ws.servers.len()];
    for (i, s) in ws.servers.iter().enumerate() {
        if build(ws, s, cache_url.is_some()) {
            running_build[i] = start(i, s);
        } else {
            eprintln!(
                "hyve-orbic: the build of `{}` failed; it starts after the next source change",
                s.bin
            );
        }
    }
    eprintln!(
        "hyve-orbic: watching sources; saving a file rebuilds and restarts the server (Ctrl-C stops)"
    );
    // The workspace root's `migrations/` need not be inside a member.
    let watched: Vec<PathBuf> = ws
        .members
        .iter()
        .cloned()
        .chain(migrations.clone())
        .collect();
    let migrations_stamp = || {
        migrations
            .as_ref()
            .map(|m| sources_stamp(std::slice::from_ref(m), &ws.target))
    };
    let mut stamp = sources_stamp(&watched, &ws.target);
    let mut applied = migrations_stamp();
    loop {
        std::thread::sleep(Duration::from_millis(400));
        // A server that exits on its own (a panic, a port in use) is
        // reported, not restarted in a loop.
        {
            let mut r = running.lock().unwrap_or_else(|e| e.into_inner());
            for (i, c) in r.children.iter_mut().enumerate() {
                if let Some(status) = c.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
                    eprintln!(
                        "hyve-orbic: `{}` exited ({status}); it starts again after the next source change",
                        ws.servers[i].bin
                    );
                    *c = None;
                    running_build[i] = None;
                }
            }
        }
        let now = sources_stamp(&watched, &ws.target);
        if now == stamp {
            continue;
        }
        // Let a burst of saves settle.
        std::thread::sleep(Duration::from_millis(150));
        stamp = sources_stamp(&watched, &ws.target);
        // New migrations: applied, then every server restarts on the new
        // schema.
        let schema_changed = migrations_stamp() != applied;
        applied = migrations_stamp();
        prepare(schema_changed);
        for (i, s) in ws.servers.iter().enumerate() {
            if !build(ws, s, cache_url.is_some()) {
                eprintln!(
                    "hyve-orbic: the build of `{}` failed; the server keeps running the last build",
                    s.bin
                );
                continue;
            }
            let fresh = modified(&binary(ws, s));
            if schema_changed || running_build[i].is_none() || fresh != running_build[i] {
                running_build[i] = start(i, s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(server: &str, bins: &[&str]) -> Value {
        let targets: Vec<String> = bins
            .iter()
            .map(|b| format!(r#"{{"kind":["bin"],"name":"{b}"}}"#))
            .chain(std::iter::once(
                r#"{"kind":["lib"],"name":"app"}"#.to_owned(),
            ))
            .collect();
        let json = format!(
            r#"{{"workspace_root":"/ws","target_directory":"/ws/target","packages":[
                {{"name":"app","manifest_path":"/ws/app/Cargo.toml","targets":[{}],"metadata":{server}}},
                {{"name":"lib","manifest_path":"/ws/lib/Cargo.toml","targets":[],"metadata":null}}]}}"#,
            targets.join(",")
        );
        hyve::serde::from_json(&json).unwrap()
    }

    #[test]
    fn servers_come_from_package_metadata() {
        let ws = parse(&meta(
            r#"{"orbic":{"server":{"bin":"api","features":["server"],"cache_features":["cache-redis","orbic/cache-redis"]}}}"#,
            &["app", "api"],
        ))
        .unwrap();
        assert_eq!(ws.root, PathBuf::from("/ws"));
        assert_eq!(
            ws.members,
            [PathBuf::from("/ws/app"), PathBuf::from("/ws/lib")]
        );
        assert_eq!(
            ws.servers,
            [Server {
                cache_features: vec!["cache-redis".into(), "orbic/cache-redis".into()],
                package: "app".into(),
                bin: "api".into(),
                features: vec!["server".into()],
            }]
        );
        // The only binary, or the one named after the package.
        let ws = parse(&meta(r#"{"orbic":{"server":{}}}"#, &["srv"])).unwrap();
        assert_eq!(ws.servers[0].bin, "srv");
        let ws = parse(&meta(r#"{"orbic":{"server":{}}}"#, &["app", "x"])).unwrap();
        assert_eq!(ws.servers[0].bin, "app");
        // None declared.
        assert!(parse(&meta("null", &["app"])).unwrap().servers.is_empty());
    }

    #[test]
    fn bad_declarations_are_errors() {
        let e = parse(&meta(r#"{"orbic":{"server":{"bin":"nope"}}}"#, &["app"])).unwrap_err();
        assert!(e.contains("`nope`") && e.contains("app"), "{e}");
        let e = parse(&meta(r#"{"orbic":{"server":true}}"#, &["app"])).unwrap_err();
        assert!(e.contains("must be a table"), "{e}");
        let e = parse(&meta(r#"{"orbic":{"server":{}}}"#, &["a", "b"])).unwrap_err();
        assert!(e.contains("several binaries"), "{e}");
    }
}
