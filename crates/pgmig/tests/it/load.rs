//! Loading desired-state SQL files into the shadow database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{Error, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory of SQL files, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-load-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
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

/// The shadow spec for the test backend; keep the `TestDb` alive while it is used.
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
fn semicolons_inside_bodies_and_literals() {
    let dir = Dir::new(&[(
        "schema.sql",
        "CREATE TABLE t (note text DEFAULT 'a;b', \"odd;name\" int);\n\
         CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
           RETURN 1;\n\
         END;\n\
         $$;\n\
         CREATE TABLE u (id int);\n",
    )]);
    let (spec, _db) = shadow();
    let model = pgmig::load_model(&Source::Sql(dir.0.join("schema.sql")), &spec).expect("load");
    let names: Vec<_> = model.tables.keys().map(|k| k.name.as_str()).collect();
    assert_eq!(names, ["t", "u"]);
}

#[test]
fn errors_point_at_file_line_and_column() {
    let dir = Dir::new(&[
        ("a.sql", "CREATE TABLE a (id int);\n"),
        ("b.sql", "CREATE TABLE b (id int);\nCREATE TABLE c (id int);\n\nCREATE TABLE d (\n    id nosuchtype\n);\n"),
    ]);
    let (spec, _db) = shadow();
    let err = pgmig::load_model(&Source::Sql(dir.0.clone()), &spec).expect_err("unknown type");
    let Error::Load { path, pos, source } = &err else { panic!("expected a load error, got {err}") };
    assert_eq!(path, &dir.0.join("b.sql"));
    assert_eq!(source.code.as_deref(), Some("42704"), "{err}");
    assert_eq!(source.message, "type \"nosuchtype\" does not exist");
    // pgmem reports no error position (kaeawc/pgmem#166), so pgmig falls back to the statement start.
    let want = if postgres_dsn().is_some() { (5, 8) } else { (4, 1) };
    assert_eq!((pos.line, pos.col), want, "{err}");
    assert_eq!(err.to_string(), format!("{}:{}:{}: type \"nosuchtype\" does not exist (SQLSTATE 42704)", path.display(), want.0, want.1));
}

#[test]
fn split_errors_point_at_the_token() {
    let dir = Dir::new(&[("schema.sql", "CREATE TABLE a (id int);\nCREATE TABLE b (note text DEFAULT 'open);\n")]);
    let (spec, _db) = shadow();
    let err = pgmig::load_model(&Source::Sql(dir.0.join("schema.sql")), &spec).expect_err("unterminated");
    assert!(matches!(err, Error::Split { .. }), "{err}");
    assert!(err.to_string().ends_with("schema.sql:2:35: unterminated quoted string"), "{err}");
}
