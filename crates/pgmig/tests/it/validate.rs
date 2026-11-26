//! `schema validate`: load SQL files into the shadow database and report errors.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{Error, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory of SQL files, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-validate-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        for (name, text) in files {
            std::fs::write(path.join(name), text).expect("write sql file");
        }
        Dir(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shadow() -> (ShadowSpec, Option<TestDb>) {
    match postgres_dsn() {
        None => (ShadowSpec::Pgmem, None),
        Some(_) => {
            let db = TestDb::new();
            (ShadowSpec::Postgres(db.conninfo.clone()), Some(db))
        }
    }
}

#[test]
fn valid_directory_passes() {
    let dir = Dir::new(&[("a.sql", "CREATE TABLE a (id int);\n"), ("b.sql", "CREATE TABLE b (id int);\n")]);
    let (spec, _db) = shadow();
    pgmig::schema_validate(&Source::Sql(dir.0.clone()), &spec).expect("valid");
}

#[test]
fn invalid_second_file_names_file_and_line() {
    let dir = Dir::new(&[
        ("a.sql", "CREATE TABLE a (id int);\n"),
        ("b.sql", "CREATE TABLE b (id int);\n\nCREATE TABLE c (\n    id nosuchtype\n);\n"),
    ]);
    let (spec, _db) = shadow();
    let err = pgmig::schema_validate(&Source::Sql(dir.0.clone()), &spec).expect_err("unknown type");
    let Error::Load { path, pos, .. } = &err else { panic!("expected a load error, got {err}") };
    assert_eq!(path, &dir.0.join("b.sql"));
    // pgmem reports no error position (kaeawc/pgmem#166): the statement start stands in.
    let want = if postgres_dsn().is_some() { (4, 8) } else { (3, 1) };
    assert_eq!((pos.line, pos.col), want, "{err}");
    assert!(err.to_string().ends_with("type \"nosuchtype\" does not exist (SQLSTATE 42704)"), "{err}");
}

#[test]
fn database_source_is_rejected() {
    let (spec, _db) = shadow();
    let err = pgmig::schema_validate(&Source::Database("host=localhost".into()), &spec).expect_err("database");
    assert!(matches!(err, Error::NotSql), "{err}");
}
