//! `-- pgmig:import`: dependency order, load-once, cycles and errors.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{Error, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory of SQL files (names may contain `/`), removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-import-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        for (name, text) in files {
            let f = path.join(name);
            std::fs::create_dir_all(f.parent().expect("parent")).expect("create temp dir");
            std::fs::write(f, text).expect("write sql file");
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

fn load(path: PathBuf) -> pgmig::Result<()> {
    let (spec, _db) = shadow();
    pgmig::schema_validate(&Source::Sql(path), &spec)
}

const KIND: &str = "CREATE TYPE kind AS ENUM ('a', 'b');\n";

#[test]
fn import_loads_dependency_first() {
    // Lexical order would run a_users.sql first and fail: type "kind" does not exist.
    let files = [("a_users.sql", "-- pgmig:import b_kinds.sql\n\nCREATE TABLE users (k kind);\n"), ("b_kinds.sql", KIND)];
    let dir = Dir::new(&files);
    load(dir.0.clone()).expect("directory");
    load(dir.0.join("a_users.sql")).expect("single file loads its imports");
    // Without the import the lexical order fails.
    let dir = Dir::new(&[("a_users.sql", "CREATE TABLE users (k kind);\n"), ("b_kinds.sql", KIND)]);
    assert!(matches!(load(dir.0.clone()), Err(Error::Load { .. })));
}

#[test]
fn directory_import_and_relative_paths() {
    let dir = Dir::new(&[
        ("main/a.sql", "-- pgmig:import ../types\n\nCREATE TABLE t (x k1, y k2);\n"),
        ("types/k1.sql", "CREATE TYPE k1 AS ENUM ('a');\n"),
        ("types/k2.sql", "CREATE TYPE k2 AS ENUM ('b');\n"),
        ("types/ignored.txt", "not sql"),
    ]);
    load(dir.0.join("main")).expect("import outside the source directory");
}

#[test]
fn each_file_loads_once() {
    let dir = Dir::new(&[
        ("a.sql", "-- pgmig:import shared.sql\n\nCREATE TABLE a (k kind);\n"),
        ("b.sql", "-- pgmig:import shared.sql\n-- pgmig:import ./shared.sql\n\nCREATE TABLE b (k kind);\n"),
        ("shared.sql", KIND),
    ]);
    load(dir.0.clone()).expect("shared loads once");
}

#[test]
fn cycle_is_an_error() {
    let dir = Dir::new(&[("a.sql", "-- pgmig:import b.sql\n"), ("b.sql", "-- pgmig:import a.sql\n")]);
    let err = load(dir.0.clone()).expect_err("cycle");
    assert!(matches!(err, Error::ImportCycle { .. }), "{err}");
    let (a, b) = (dir.0.join("a.sql"), dir.0.join("b.sql"));
    assert_eq!(err.to_string(), format!("import cycle: {} -> {} -> {}", a.display(), b.display(), a.display()));
}

#[test]
fn missing_target_names_file_line_and_path() {
    let dir = Dir::new(&[("a.sql", "\n\n-- pgmig:import nope.sql\n")]);
    let err = load(dir.0.join("a.sql")).expect_err("missing");
    assert!(matches!(err, Error::ImportMissing { .. }), "{err}");
    assert!(err.to_string().contains("a.sql:3:1: cannot import nope.sql"), "{err}");
}

#[test]
fn import_without_a_path_is_an_error() {
    let dir = Dir::new(&[("a.sql", "-- pgmig:import\n")]);
    let err = load(dir.0.join("a.sql")).expect_err("no args");
    assert!(matches!(err, Error::ImportArgs { .. }), "{err}");
    assert!(err.to_string().contains("a.sql:1:1"), "{err}");
}

#[test]
fn import_directly_above_the_first_statement() {
    let dir = Dir::new(&[("a.sql", "-- pgmig:import b.sql\nCREATE TABLE a (v t);\n"), ("b.sql", "CREATE TYPE t AS ENUM ('x');\n")]);
    load(dir.0.join("a.sql")).expect("loads");
}

#[test]
fn import_after_the_first_statement_is_an_error() {
    let dir = Dir::new(&[("a.sql", "CREATE TABLE a (id int);\n-- pgmig:import b.sql\nCREATE TABLE c (id int);\n"), ("b.sql", "")]);
    let err = load(dir.0.join("a.sql")).expect_err("placement");
    assert!(matches!(err, Error::ImportPlacement { .. }), "{err}");
    let dir = Dir::new(&[("a.sql", "CREATE TABLE a (id int);\n\n-- pgmig:import b.sql\n"), ("b.sql", "")]);
    let err = load(dir.0.join("a.sql")).expect_err("trailing import");
    assert!(matches!(err, Error::ImportPlacement { .. }), "{err}");
}

#[test]
fn unknown_directives_are_left_alone() {
    let dir = Dir::new(&[("a.sql", "-- pgmig:whatever x\n\nCREATE TABLE a (id int);\n")]);
    load(dir.0.join("a.sql")).expect("loads");
}

#[cfg(unix)]
#[test]
fn a_symlinked_entry_keeps_its_lexical_place() {
    // a.sql links to a file whose real path sorts after b.sql; it must still load first.
    let dir = Dir::new(&[("b.sql", "CREATE TABLE b (v t);\n"), ("zz/real.sql", "CREATE TYPE t AS ENUM ('x');\n")]);
    std::fs::create_dir_all(dir.0.join("schema")).expect("mkdir");
    std::os::unix::fs::symlink(dir.0.join("zz/real.sql"), dir.0.join("schema/a.sql")).expect("symlink");
    std::fs::rename(dir.0.join("b.sql"), dir.0.join("schema/b.sql")).expect("move");
    load(dir.0.join("schema")).expect("a.sql loads before b.sql");
}
