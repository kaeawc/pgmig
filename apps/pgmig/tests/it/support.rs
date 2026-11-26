//! The CLI under test, and target databases: pgmem by default, real Postgres when
//! `PGMIG_TEST_DSN` is set.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::Conn;

static NEXT: AtomicU32 = AtomicU32::new(0);

fn postgres_dsn() -> Option<String> {
    std::env::var("PGMIG_TEST_DSN").ok().filter(|s| !s.is_empty())
}

/// A new, empty database, dropped with the value.
pub struct TestDb {
    pub conninfo: String,
    keep: Keep,
}

enum Keep {
    Pgmem { _server: pgmem::Server },
    Postgres { admin: String, name: String },
}

impl TestDb {
    pub fn new() -> TestDb {
        match postgres_dsn() {
            None => {
                let server = pgmem::Server::start().expect("start pgmem");
                // As a new Postgres database has it (kaeawc/pgmem#178).
                Conn::connect(&server.conninfo()).and_then(|mut c| c.batch(pgmig::PUBLIC_SCHEMA_COMMENT_SQL)).expect("comment public");
                TestDb { conninfo: server.conninfo(), keep: Keep::Pgmem { _server: server } }
            }
            Some(admin) => {
                let name = format!("pgmig_cli_test_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
                Conn::connect(&admin).and_then(|mut c| c.batch(&format!("CREATE DATABASE {name}"))).expect("create test database");
                TestDb { conninfo: format!("{admin} dbname={name}"), keep: Keep::Postgres { admin, name } }
            }
        }
    }

    pub fn connect(&self) -> Conn {
        Conn::connect(&self.conninfo).expect("connect to test database")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if let Keep::Postgres { admin, name } = &self.keep
            && let Ok(mut c) = Conn::connect(admin)
        {
            let _ = c.batch(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
        }
    }
}

/// The `--shadow` argument for the test backend; keep the `TestDb` alive while it is used.
pub fn shadow_arg() -> (String, Option<TestDb>) {
    match postgres_dsn() {
        None => ("pgmem".to_owned(), None),
        Some(_) => {
            let db = TestDb::new();
            (db.conninfo.clone(), Some(db))
        }
    }
}

/// A desired-state file in a fresh temp directory, removed with the value.
pub struct SqlFile {
    dir: PathBuf,
}

impl SqlFile {
    pub fn new(sql: &str) -> SqlFile {
        let dir = std::env::temp_dir().join(format!("pgmig_cli_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("schema.sql"), sql).expect("write schema.sql");
        SqlFile { dir }
    }

    pub fn path(&self) -> String {
        self.dir.join("schema.sql").to_string_lossy().into_owned()
    }
}

impl Drop for SqlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.dir.join("schema.sql"));
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Runs `pgmig` with `args` and stdin that is not a terminal.
pub fn pgmig(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pgmig")).args(args).stdin(Stdio::null()).output().expect("run pgmig")
}

/// Whether tests run against real Postgres rather than pgmem.
pub fn shadow_is_postgres() -> bool {
    postgres_dsn().is_some()
}
