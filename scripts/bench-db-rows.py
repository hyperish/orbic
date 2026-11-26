"""Compare generated table decoding with the published alpha.1 release on one SQLite schema."""
from pathlib import Path
from statistics import median
import json
import os
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = """
#[orbic::table("measurements")]
struct Sample {
    #[pk]
    id: i64,
    a: i32, b: i32, c: i32, d: i32, e: i32, f: i32,
    enabled: bool, title: String, optional: Option<i32>,
}

#[tokio::main(flavor="current_thread")]
async fn main() {
    let db = orbic::Db::empty_in_memory().await;
    db.execute_script("CREATE TABLE measurements (id INTEGER PRIMARY KEY, a INT,b INT,c INT,d INT,e INT,f INT,enabled INT,title TEXT,optional INT);
        WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM seq WHERE i<1024)
        INSERT INTO measurements SELECT i,i,i,i,i,i,i,1,'sensor',NULL FROM seq;").await.unwrap();
    let mut sum=0i64;
    let start=std::time::Instant::now();
    for _ in 0..8 {
        let rows=db.query::<Sample>().fetch_all().await.unwrap();
        assert_eq!(rows.len(),1024);
        sum+=rows.iter().map(|r| r.id+i64::from(r.a+r.b+r.c+r.d+r.e+r.f+r.optional.unwrap_or(0))+i64::from(r.enabled)+r.title.len() as i64).sum::<i64>();
    }
    println!("{} {}",sum,start.elapsed().as_nanos()/8192);
}
"""

def main():
    with tempfile.TemporaryDirectory(prefix="orbic-row-benchmark-") as scratch:
        root = Path(scratch)
        target = Path(os.environ.get("CARGO_TARGET_DIR", root / "target")).resolve()
        env = dict(os.environ, CARGO_TARGET_DIR=str(target))
        binaries = []
        for name, dependency in [
            ("working", "path=" + json.dumps(str(ROOT / "crates/orbic"))),
            ("published", 'version="=0.1.0-alpha.1"'),
        ]:
            package = root / name
            (package / "src").mkdir(parents=True)
            (package / "src/main.rs").write_text(SOURCE)
            (package / "Cargo.toml").write_text(f'''[package]
name="row-bench-{name}"
version="0.0.0"
edition="2024"
[features]
default=["server"]
server=["orbic/server"]
[dependencies]
orbic={{{dependency}}}
tokio={{version="1",features=["rt","macros"]}}
[workspace]
''')
            subprocess.run(["cargo","build","--release"], cwd=package, env=env, check=True)
            binaries.append(target / "release" / ("row-bench-" + name + (".exe" if os.name == "nt" else "")))
        timings = [[], []]
        for _ in range(21):
            for i, binary in enumerate(binaries):
                checksum, nanos = map(int, subprocess.check_output([binary], text=True).split())
                assert checksum == 8 * (7 * 1024 * 1025 // 2 + 7 * 1024)
                timings[i].append(nanos)
        for binary, values in zip(binaries, timings):
            print(f"{binary.name}: median {median(values)} ns per row, range {min(values)}–{max(values)}")

if __name__ == "__main__":
    main()
