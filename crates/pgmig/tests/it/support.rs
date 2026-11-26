//! Test databases: pgmem by default, real Postgres when `PGMIG_TEST_DSN` is set.

use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{Conn, Shadow};

pub fn postgres_dsn() -> Option<String> {
    std::env::var("PGMIG_TEST_DSN").ok().filter(|s| !s.is_empty())
}

pub fn backend_name() -> &'static str {
    if postgres_dsn().is_some() { "postgres" } else { "pgmem" }
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
                static NEXT: AtomicU32 = AtomicU32::new(0);
                let name = format!("pgmig_test_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
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

/// A shadow database on the test backend. Keep the `TestDb` alive as long as the shadow.
pub fn shadow() -> (Shadow, Option<TestDb>) {
    if postgres_dsn().is_none() {
        return (Shadow::pgmem().expect("pgmem shadow"), None);
    }
    let db = TestDb::new();
    (Shadow::postgres(&db.conninfo).expect("postgres shadow"), Some(db))
}
