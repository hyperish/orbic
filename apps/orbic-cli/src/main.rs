//! `hyve-orbic` — the orbic stages `hyve` runs (`docs/01-dx-and-dsl.md` §4).
//!
//! ```text
//! hyve-orbic stages
//! hyve-orbic codegen [--check] [--prune] [--root <dir>]
//! hyve-orbic preview [--web] [--native]
//! ```
//!
//! `stages` answers `hyve`'s question (`hyve/docs/01-dx-and-dsl.md` §2,
//! 프레임워크 단계 호출): the first line is orbic's version, then one stage
//! name per line. `preview` is listed only when a package of the workspace
//! declares a server (`[package.metadata.orbic.server]`).
//!
//! `preview` builds and runs the declared servers and restarts them when
//! their sources change (`preview.rs`); `--web` and `--native` choose UI
//! targets and are accepted for `hyve preview`.
//!
//! `codegen` finds every `sql!` in the workspace's Rust sources, analyzes
//! each query on databases with the migrations applied, and writes
//! `.orbic/sql/`. `--check` writes nothing and fails if a file is missing or
//! differs; `--prune` removes files no query uses. Exit codes: 0 success,
//! 1 failure, 2 usage.

mod codegen;
mod preview;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: hyve-orbic stages | hyve-orbic codegen [--check] [--prune] [--root <dir>] | hyve-orbic preview [--web] [--native]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((stage, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if stage == "stages" && rest.is_empty() {
        println!("{}", env!("CARGO_PKG_VERSION"));
        println!("codegen");
        if preview::workspace().is_ok_and(|ws| !ws.servers.is_empty()) {
            println!("preview");
        }
        return ExitCode::SUCCESS;
    }
    if stage == "preview" {
        if let Some(other) = rest.iter().find(|a| *a != "--web" && *a != "--native") {
            eprintln!("error: unknown option `{other}`\n{USAGE}");
            return ExitCode::from(2);
        }
        return match preview::workspace().and_then(|ws| preview::run(&ws)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if stage != "codegen" {
        eprintln!("error: unknown stage `{stage}`\n{USAGE}");
        return ExitCode::from(2);
    }
    let mut opts = codegen::Options::default();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => opts.check = true,
            "--prune" => opts.prune = true,
            "--root" => match it.next() {
                Some(dir) => opts.root = Some(PathBuf::from(dir)),
                None => {
                    eprintln!("error: `--root` needs a directory\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("error: unknown option `{other}`\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match rt.block_on(codegen::run(opts)) {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
