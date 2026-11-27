//! `pgmig schema plan` and `pgmig schema apply --plan`.

use std::path::PathBuf;
use std::process::Output;

use serde_json::{Value, json};

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn code(out: &Output) -> Option<i32> {
    out.status.code()
}

fn columns(db: &TestDb) -> String {
    let rows = db.connect().query("SELECT count(*) FROM information_schema.columns WHERE table_name = 't'").expect("query");
    rows[0][0].clone().unwrap_or_default()
}

/// A plan file path in its own temp directory, removed with the value.
struct PlanFile(PathBuf);

impl PlanFile {
    fn new() -> PlanFile {
        let dir = std::env::temp_dir().join(format!(
            "pgmig_cli_plan_{}_{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().expect("clock").as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        PlanFile(dir.join("plan.json"))
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.0).expect("read plan")).expect("plan json")
    }
}

impl Drop for PlanFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn setup() -> (TestDb, SqlFile, (String, Option<TestDb>)) {
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE t (id int);").expect("load target");
    (db, SqlFile::new("CREATE TABLE t (id int, name text);"), shadow_arg())
}

#[test]
fn plan_writes_a_file_and_apply_runs_it() {
    let (db, desired, (shadow, _keep)) = setup();
    let file = PlanFile::new();
    let out = pgmig(&["schema", "plan", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--out", &file.path()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    assert!(text(&out.stderr).contains("1 statement planned"), "{}", text(&out.stderr));
    assert_eq!(columns(&db), "1");
    let plan = file.json();
    assert_eq!(plan["format"], 1);
    assert_eq!(plan["sql"], "ALTER TABLE public.t ADD COLUMN name text;\n");
    assert_eq!(plan["plan"]["statements"], json!(["ALTER TABLE public.t ADD COLUMN name text"]));
    assert!(plan["from"].as_str().is_some_and(|f| f.starts_with("h1:")) && plan["to"].as_str().is_some_and(|f| f.starts_with("h1:")));

    // No --to, no shadow: the file says what to run.
    let args = ["schema", "apply", "--url", &db.conninfo, "--plan", &file.path()];
    let dry = pgmig(&[&args[..], &["--dry-run"]].concat());
    assert!(dry.status.success(), "{}", text(&dry.stderr));
    assert_eq!(text(&dry.stdout), "ALTER TABLE public.t ADD COLUMN name text;\n");
    assert_eq!(columns(&db), "1");

    // stdin is not a terminal and nothing approves the plan.
    let asked = pgmig(&args);
    assert_eq!(code(&asked), Some(1));
    assert!(text(&asked.stderr).contains("--auto-approve"), "{}", text(&asked.stderr));
    assert_eq!(columns(&db), "1");

    let out = pgmig(&[&args[..], &["--auto-approve"]].concat());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(err.contains("applied 1 statements in 1 transaction(s)") && err.contains("the target now has the planned schema"), "{err}");
    assert_eq!(columns(&db), "2");
}

#[test]
fn plan_prints_the_file_without_out() {
    let (db, desired, (shadow, _keep)) = setup();
    let out = pgmig(&["schema", "plan", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let plan: Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    assert_eq!(plan["sql"], "ALTER TABLE public.t ADD COLUMN name text;\n");
    assert_eq!(plan["pgmig_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(plan["options"], json!({"filter": {"include": [], "exclude": []}, "schema": null}));
    assert_eq!(columns(&db), "1");
}

#[test]
fn a_database_that_changed_since_planning_is_refused() {
    let (db, desired, (shadow, _keep)) = setup();
    let file = PlanFile::new();
    let out = pgmig(&["schema", "plan", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--out", &file.path()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let from = file.json()["from"].as_str().expect("from").to_owned();

    db.connect().batch("ALTER TABLE t ADD COLUMN other int").expect("change by hand");
    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--plan", &file.path(), "--auto-approve"]);
    assert_eq!(code(&out), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("the plan is stale") && err.contains(&from) && err.contains("nothing was applied"), "{err}");
    assert_eq!(text(&out.stdout), "");
    assert_eq!(columns(&db), "2");

    // --dry-run checks too.
    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--plan", &file.path(), "--dry-run"]);
    assert_eq!(code(&out), Some(1));
    assert!(text(&out.stderr).contains("the plan is stale"), "{}", text(&out.stderr));

    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--plan", &file.path(), "--auto-approve", "--format", "json"]);
    assert_eq!(code(&out), Some(1));
    let json: Value = serde_json::from_slice(&out.stdout).expect("json error");
    assert_eq!(json["code"], "plan_stale");
}

#[test]
fn an_edited_or_foreign_file_is_refused() {
    let (db, desired, (shadow, _keep)) = setup();
    let file = PlanFile::new();
    pgmig(&["schema", "plan", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--out", &file.path()]);
    let mut plan = file.json();
    let run = || pgmig(&["schema", "apply", "--url", &db.conninfo, "--plan", &file.path(), "--auto-approve"]);

    plan["plan"]["statements"] = json!(["DROP TABLE public.t"]);
    std::fs::write(&file.0, plan.to_string()).expect("write");
    let out = run();
    assert_eq!(code(&out), Some(1));
    assert!(text(&out.stderr).contains("`sql` is not the SQL of `plan`"), "{}", text(&out.stderr));

    plan["format"] = json!(2);
    std::fs::write(&file.0, plan.to_string()).expect("write");
    let out = run();
    assert_eq!(code(&out), Some(1));
    assert!(text(&out.stderr).contains("format 2 is not supported"), "{}", text(&out.stderr));

    std::fs::write(&file.0, "CREATE TABLE t ();").expect("write");
    assert_eq!(code(&run()), Some(1));
    assert_eq!(columns(&db), "1");
}

#[test]
fn plan_conflicts_with_the_flags_the_file_carries() {
    let (db, desired, (shadow, _keep)) = setup();
    let file = PlanFile::new();
    let base = ["schema", "apply", "--url", &db.conninfo, "--plan", &file.path()];
    for extra in [
        vec!["--to", &desired.path()],
        vec!["--include", "public"],
        vec!["--exclude", "public"],
        vec!["--schema", "public"],
        vec!["--skip", "drop_table"],
        vec!["--concurrent-indexes"],
        vec!["--if-exists"],
        vec!["--no-if-exists"],
        vec!["--cascade"],
        vec!["--no-cascade"],
        vec!["--verify"],
        vec!["--edit"],
        vec!["--shadow", &shadow],
    ] {
        let out = pgmig(&[&base[..], &extra[..]].concat());
        assert_eq!(code(&out), Some(2), "{extra:?}: {}", text(&out.stderr));
        assert!(text(&out.stderr).contains("cannot be used with"), "{extra:?}: {}", text(&out.stderr));
    }
}

#[test]
fn json_output_and_empty_plans() {
    let (db, desired, (shadow, _keep)) = setup();
    let file = PlanFile::new();
    pgmig(&["schema", "plan", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--out", &file.path()]);
    let saved = file.json();

    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--plan", &file.path(), "--auto-approve", "--format", "json"]);
    assert_eq!(code(&out), Some(0), "{}", text(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!((&json["applied"], &json["from"], &json["to"]), (&json!(true), &saved["from"], &saved["to"]));
    assert_eq!(json["report"], json!({"batches": 1, "statements": 1}));
    assert_eq!(json["fingerprint"], saved["to"]);
    assert_eq!(json["plan"]["statements"], saved["plan"]["statements"]);
    assert_eq!(json["sql"], saved["sql"]);

    // Planning what is already there saves an empty plan, which applies as a no-op.
    let empty = PlanFile::new();
    let out = pgmig(&["schema", "plan", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--out", &empty.path()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(empty.json()["plan"]["statements"], json!([]));
    assert_eq!(empty.json()["sql"], "");
    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--plan", &empty.path(), "--auto-approve"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stderr), "no changes\n");
    assert_eq!(columns(&db), "2");
}
