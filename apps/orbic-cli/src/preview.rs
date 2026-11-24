//! `hyve-orbic preview` (`docs/01-dx-and-dsl.md` §3, 로컬 개발·프리뷰):
//! builds every server the workspace declares
//! (`[package.metadata.orbic.server]`), runs the built binaries, and when a
//! source changes rebuilds them and restarts each one whose binary changed.
//! Under `hyve preview` (`HYVE_STAGE_LIFELINE=stdin`) it stops the servers
//! and exits when its stdin closes.
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

/// A server a package declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Server {
    pub(crate) package: String,
    pub(crate) bin: String,
    pub(crate) features: Vec<String>,
}

/// What `preview` needs from `cargo metadata`.
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
        servers.push(Server {
            package: name,
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

fn binary(ws: &Workspace, s: &Server) -> PathBuf {
    ws.target
        .join("debug")
        .join(format!("{}{}", s.bin, std::env::consts::EXE_SUFFIX))
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// `cargo build` of the server (its errors go to the terminal).
fn build(ws: &Workspace, s: &Server) -> bool {
    let mut cmd = Command::new(cargo());
    cmd.args(["build", "-p", &s.package, "--bin", &s.bin])
        .current_dir(&ws.root)
        .stdin(Stdio::null());
    if !s.features.is_empty() {
        cmd.arg("--features").arg(s.features.join(","));
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
            "no server to preview: declare one in its package's Cargo.toml, `[package.metadata.orbic.server]` with `bin = \"<binary>\"`"
                .into(),
        );
    }
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
        match Command::new(&path)
            .current_dir(&ws.root)
            .stdin(Stdio::null())
            .spawn()
        {
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
    let mut running_build: Vec<Option<SystemTime>> = vec![None; ws.servers.len()];
    for (i, s) in ws.servers.iter().enumerate() {
        if build(ws, s) {
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
    let mut stamp = sources_stamp(&ws.members, &ws.target);
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
        let now = sources_stamp(&ws.members, &ws.target);
        if now == stamp {
            continue;
        }
        // Let a burst of saves settle.
        std::thread::sleep(Duration::from_millis(150));
        stamp = sources_stamp(&ws.members, &ws.target);
        for (i, s) in ws.servers.iter().enumerate() {
            if !build(ws, s) {
                eprintln!(
                    "hyve-orbic: the build of `{}` failed; the server keeps running the last build",
                    s.bin
                );
                continue;
            }
            let fresh = modified(&binary(ws, s));
            if running_build[i].is_none() || fresh != running_build[i] {
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
            r#"{"orbic":{"server":{"bin":"api","features":["server"]}}}"#,
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
