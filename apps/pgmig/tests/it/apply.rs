//! `pgmig schema apply`.

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

fn columns(db: &TestDb) -> String {
    let rows = db.connect().query("SELECT count(*) FROM information_schema.columns WHERE table_name = 't'").expect("query");
    rows[0][0].clone().unwrap_or_default()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn setup() -> (TestDb, SqlFile, (String, Option<TestDb>)) {
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE t (id int);").expect("load target");
    (db, SqlFile::new("CREATE TABLE t (id int, name text);"), shadow_arg())
}

#[test]
fn dry_run_prints_the_plan_and_changes_nothing() {
    let (db, desired, (shadow, _keep)) = setup();
    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "ALTER TABLE public.t ADD COLUMN name text;\n");
    assert_eq!(columns(&db), "1");
}

#[test]
fn refuses_to_prompt_without_a_terminal() {
    let (db, desired, (shadow, _keep)) = setup();
    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--auto-approve"), "{}", text(&out.stderr));
    assert_eq!(columns(&db), "1");
}

#[test]
fn auto_approve_applies_and_a_second_run_has_no_changes() {
    let (db, desired, (shadow, _keep)) = setup();
    let args = ["schema", "apply", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--auto-approve"];
    let out = pgmig(&args);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(columns(&db), "2");

    let again = pgmig(&args);
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert_eq!(text(&again.stdout), "");
    assert_eq!(text(&again.stderr), "no changes\n");
}

#[test]
fn a_failing_statement_exits_non_zero_and_names_it() {
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE t (id int); INSERT INTO t VALUES (NULL);").expect("load target");
    let desired = SqlFile::new("CREATE TABLE t (id int NOT NULL);");
    let (shadow, _keep) = shadow_arg();
    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--auto-approve"]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(
        err.contains("statement 1 failed: ALTER TABLE public.t ALTER COLUMN id SET NOT NULL") && err.contains("SQLSTATE 23502"),
        "{err}"
    );
}
