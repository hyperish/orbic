//! `[orbic]` in `hyve.toml` (`docs/01-dx-and-dsl.md` §4, 설정과 위치): one
//! schema for the app, `hyve-orbic codegen` and `hyve-orbic dev`, since
//! the loader refuses keys a table's schema does not have. Each field can
//! also come from `HYPERISH_ORBIC_<FIELD>` (and `.env`), which wins over
//! `hyve.toml`.

use hyve::config::Loader;
use hyve::prelude::*;

use crate::db::Db;

/// The variable for `database_url`. `hyve-orbic dev` sets it to its
/// local database for the servers it runs.
pub const DATABASE_URL_ENV: &str = "HYPERISH_ORBIC_DATABASE_URL";

/// `[orbic]` in `hyve.toml`.
#[model]
#[derive(Debug, Clone, Default)]
pub struct OrbicConfig {
    /// The operational dialect: `postgres` or `sqlite`.
    pub dialect: Option<String>,
    /// Paths (relative to the workspace root) whose sources `codegen` does
    /// not scan, e.g. compile-fail tests.
    pub codegen_exclude: Vec<String>,
    /// The database the app connects to (`postgres://...` or `sqlite:...`),
    /// read by [`Db::from_config`].
    pub database_url: Option<String>,
}

impl OrbicConfig {
    /// Loads `[orbic]` from the nearest `hyve.toml` at or above the working
    /// directory, the `.env` next to it, and the environment.
    pub fn load() -> HyveResult<OrbicConfig> {
        Self::load_with(Loader::new())
    }

    /// Loads `[orbic]` through `loader` (another directory or environment).
    /// Command-line arguments are not read: `--set` is for `[app]`.
    pub fn load_with(loader: Loader) -> HyveResult<OrbicConfig> {
        Ok(loader
            .args(Vec::<String>::new())
            .section("orbic")
            .load::<OrbicConfig>()?
            .into_inner())
    }

    /// Connects to `database_url`.
    pub async fn connect(&self) -> HyveResult<Db> {
        match self.database_url.as_deref() {
            Some(url) => Db::connect(url).await,
            None => Err(HyveError::internal(format!(
                "no database configured: set `[orbic] database_url` in hyve.toml or {DATABASE_URL_ENV} (`hyve dev` sets it to a local database)"
            ))),
        }
    }
}

impl Db {
    /// Connects to the configured database: `[orbic] database_url`, or
    /// `HYPERISH_ORBIC_DATABASE_URL`, which `hyve dev` sets to a local
    /// database with the workspace's migrations applied.
    pub async fn from_config() -> HyveResult<Db> {
        OrbicConfig::load()?.connect().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(toml: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "orbic-config-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hyve.toml"), toml).unwrap();
        dir
    }

    #[test]
    fn the_environment_wins_over_hyve_toml() {
        let dir =
            workspace("[orbic]\ndialect = \"sqlite\"\ndatabase_url = \"postgres://decoy/prod\"\n");
        let from_file =
            OrbicConfig::load_with(Loader::new().dir(&dir).env(Vec::<(String, String)>::new()))
                .unwrap();
        assert_eq!(
            from_file.database_url.as_deref(),
            Some("postgres://decoy/prod")
        );
        assert_eq!(from_file.dialect.as_deref(), Some("sqlite"));
        let local = OrbicConfig::load_with(
            Loader::new()
                .dir(&dir)
                .env([(DATABASE_URL_ENV, "sqlite::memory:")]),
        )
        .unwrap();
        assert_eq!(local.database_url.as_deref(), Some("sqlite::memory:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_keys_in_orbic_are_errors() {
        let dir = workspace("[orbic]\ndatabse_url = \"sqlite::memory:\"\n");
        let e = OrbicConfig::load_with(Loader::new().dir(&dir).env(Vec::<(String, String)>::new()))
            .unwrap_err();
        assert!(e.message().contains("databse_url"), "{}", e.message());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_needs_a_url_and_says_where_it_comes_from() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let e = rt.block_on(OrbicConfig::default().connect()).unwrap_err();
        assert!(e.message().contains(DATABASE_URL_ENV), "{}", e.message());
        let db = rt
            .block_on(
                OrbicConfig {
                    database_url: Some("sqlite::memory:".into()),
                    ..OrbicConfig::default()
                }
                .connect(),
            )
            .unwrap();
        assert_eq!(db.dialect(), crate::Dialect::Sqlite);
    }
}
