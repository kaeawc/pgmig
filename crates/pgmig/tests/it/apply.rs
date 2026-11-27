//! `schema_plan_apply` + `apply` against a live target.

use std::path::PathBuf;

use pgmig::{Error, Options, ShadowSpec, Source, apply, schema_plan_apply};

use crate::support::{TestDb, postgres_dsn};

/// A desired-state file in a fresh temp directory, removed with the value.
struct Desired {
    dir: PathBuf,
}

impl Desired {
    fn new(sql: &str) -> Desired {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pgmig_apply_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("schema.sql"), sql).expect("write schema.sql");
        Desired { dir }
    }

    fn source(&self) -> Source {
        Source::Sql(self.dir.join("schema.sql"))
    }
}

impl Drop for Desired {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.dir.join("schema.sql"));
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// The shadow on the test backend: pgmem, or an empty database next to the target on Postgres.
fn shadow_spec() -> (ShadowSpec, Option<TestDb>) {
    match postgres_dsn() {
        None => (ShadowSpec::Pgmem, None),
        Some(_) => {
            let db = TestDb::new();
            (ShadowSpec::Postgres(db.conninfo.clone()), Some(db))
        }
    }
}

fn target(sql: &str) -> TestDb {
    let db = TestDb::new();
    db.connect().batch(sql).expect("load target");
    db
}

#[test]
fn apply_then_plan_again_is_empty() {
    let db = target("CREATE TABLE t (id int, gone text); CREATE TABLE old (x int);");
    let desired = Desired::new("CREATE SCHEMA app; CREATE TABLE t (id int NOT NULL, name text DEFAULT 'x'); CREATE TABLE app.u (id int);");
    let (shadow, _shadow_db) = shadow_spec();

    let plan = schema_plan_apply(&db.conninfo, &desired.source(), &shadow, &Options::default()).expect("plan");
    assert!(!plan.is_empty());
    let report = apply(&db.conninfo, &plan).expect("apply");
    assert_eq!(report.batches, 1);
    assert_eq!(report.statements, plan.statements.len());

    let again = schema_plan_apply(&db.conninfo, &desired.source(), &shadow, &Options::default()).expect("plan again");
    assert!(again.is_empty(), "changes left after apply:\n{}", again.to_sql());
}

#[test]
fn failing_statement_rolls_back_its_batch() {
    // `b` has a NULL, so SET NOT NULL fails after the column `c` has been added in the same batch.
    let db = target("CREATE TABLE t (a int, b int); INSERT INTO t VALUES (1, NULL);");
    let desired = Desired::new("CREATE TABLE t (a int, b int NOT NULL, c text);");
    let (shadow, _shadow_db) = shadow_spec();

    let plan = schema_plan_apply(&db.conninfo, &desired.source(), &shadow, &Options::default()).expect("plan");
    let failing = plan.statements.iter().position(|s| s.contains("SET NOT NULL")).expect("SET NOT NULL planned");
    assert!(plan.statements[..failing].iter().any(|s| s.contains("ADD COLUMN")), "ADD COLUMN runs first:\n{}", plan.to_sql());

    let err = apply(&db.conninfo, &plan).expect_err("apply must fail");
    let Error::Apply { statement_index, statement, source, committed_batches, committed_outside_transaction, outside_transaction } = &err
    else {
        panic!("{err}")
    };
    assert_eq!(*statement_index, Some(failing));
    assert_eq!(statement, &plan.statements[failing]);
    assert_eq!(source.code.as_deref(), Some("23502"), "{source}");
    assert_eq!((*committed_batches, *committed_outside_transaction), (0, 0));
    assert!(!outside_transaction);
    let msg = err.to_string();
    assert!(msg.contains(&format!("statement {} failed", failing + 1)) && msg.contains("SQLSTATE 23502"), "{msg}");
    assert!(msg.contains("nothing was committed"), "{msg}");

    // The ADD COLUMN before it was rolled back: the target is unchanged.
    let rows = db.connect().query("SELECT count(*) FROM information_schema.columns WHERE table_name = 't'").expect("query");
    assert_eq!(rows, vec![vec![Some("2".to_owned())]]);
}

#[test]
fn enum_value_and_its_use_apply_in_two_batches() {
    let db = target("CREATE TYPE mood AS ENUM ('sad', 'happy'); CREATE TABLE p (id int, m mood DEFAULT 'sad');");
    let desired = Desired::new("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); CREATE TABLE p (id int, m mood DEFAULT 'ok');");
    let (shadow, _shadow_db) = shadow_spec();

    let plan = schema_plan_apply(&db.conninfo, &desired.source(), &shadow, &Options::default()).expect("plan");
    assert_eq!(plan.units().len(), 2, "{}", plan.to_sql());
    let report = apply(&db.conninfo, &plan).expect("apply");
    assert_eq!(report.batches, 2);

    let again = schema_plan_apply(&db.conninfo, &desired.source(), &shadow, &Options::default()).expect("plan again");
    assert!(again.is_empty(), "changes left after apply:\n{}", again.to_sql());
}

#[test]
fn empty_plan_applies_nothing() {
    let db = target("CREATE TABLE t (id int);");
    let desired = Desired::new("CREATE TABLE t (id int);");
    let (shadow, _shadow_db) = shadow_spec();
    let plan = schema_plan_apply(&db.conninfo, &desired.source(), &shadow, &Options::default()).expect("plan");
    assert!(plan.is_empty(), "{}", plan.to_sql());
    assert_eq!(apply(&db.conninfo, &plan).expect("apply"), pgmig::ApplyReport::default());
}

#[test]
fn backslash_literals_apply_with_standard_conforming_strings_off() {
    let db = target("CREATE TABLE t (id int);");
    let name = db.connect().query("SELECT current_database()").expect("database name")[0][0].clone().expect("name");
    let set = db.connect().batch(&format!("ALTER DATABASE {name} SET standard_conforming_strings = off"));
    if let Err(e) = set {
        assert!(postgres_dsn().is_none(), "ALTER DATABASE ... SET failed on Postgres: {e}");
        eprintln!("skipped on pgmem: ALTER DATABASE ... SET is a syntax error (kaeawc/pgmem#227): {e}");
        return;
    }
    let desired = Desired::new("CREATE TABLE t (id int); COMMENT ON TABLE t IS E'a\\\\b \\'q\\' c:\\\\temp';");
    let (shadow, _shadow_db) = shadow_spec();
    let plan = schema_plan_apply(&db.conninfo, &desired.source(), &shadow, &Options::default()).expect("plan");
    apply(&db.conninfo, &plan).expect("apply");

    let rows = db.connect().query("SELECT obj_description('t'::regclass, 'pg_class')").expect("comment");
    assert_eq!(rows, vec![vec![Some("a\\b 'q' c:\\temp".to_owned())]]);
}
