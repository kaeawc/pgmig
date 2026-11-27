//! Saved plans (`schema plan`, `schema apply --plan`): a plan applies only to a database that is
//! still as it was planned, runs exactly its saved statements, and carries its own options.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, ApplySavedRequest, ErrorCode, SchemaPlanRequest};
use pgmig::{ChangeKind, Filter, Options, SavedPlan, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig-saved-plan-it-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tmp(path)
    }

    fn sql(&self, sql: &str) -> Source {
        let path = self.0.join("schema.sql");
        std::fs::write(&path, sql).expect("write schema.sql");
        Source::Sql(path)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The shadow on the test backend: pgmem, or an empty database on Postgres.
fn shadow() -> (ShadowSpec, Option<TestDb>) {
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

fn column_count(db: &TestDb, table: &str) -> String {
    let rows = db.connect().query(&format!("SELECT count(*) FROM information_schema.columns WHERE table_name = '{table}'")).expect("query");
    rows[0][0].clone().unwrap_or_default()
}

fn plan(db: &TestDb, desired: &Source, spec: &ShadowSpec, options: Options) -> SavedPlan {
    let req = SchemaPlanRequest { target: db.conninfo.clone(), desired: desired.clone(), shadow: spec.clone(), options, verify: false };
    api::schema_plan(&req).expect("plan")
}

fn apply(db: &TestDb, plan: &SavedPlan) -> Result<api::ApplySavedResponse, api::ApiError> {
    api::apply_saved(&ApplySavedRequest { target: db.conninfo.clone(), plan: plan.clone() })
}

fn public_only() -> Filter {
    Filter::parse(&["public"], &[] as &[&str]).expect("pattern")
}

const FROM: &str = "CREATE TABLE t (id int);";
const TO: &str = "CREATE TABLE t (id int, name text);";

#[test]
fn a_plan_applies_to_the_database_it_was_made_for_and_makes_the_desired_state() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let desired = t.sql(TO);
    let saved = plan(&db, &desired, &spec, Options::default());

    assert_eq!(saved.format, 1);
    assert_eq!(saved.sql, "ALTER TABLE public.t ADD COLUMN name text;\n");
    assert_eq!(saved.sql, saved.plan.to_sql());
    assert_ne!(saved.from, saved.to);
    assert!(saved.from.starts_with("h1:") && saved.to.starts_with("h1:"));
    // The target was only read.
    assert_eq!(column_count(&db, "t"), "1");

    let done = apply(&db, &saved).expect("apply");
    assert_eq!(done.report.statements, 1);
    assert!(done.compared);
    assert_eq!(done.fingerprint, saved.to);
    assert_eq!(column_count(&db, "t"), "2");
    let now = pgmig::schema_fingerprint(&Source::Database(db.conninfo.clone()), &spec, &Options::default()).expect("fingerprint");
    assert_eq!(now, saved.to);

    // The database is no longer what the plan was made from: running it again is refused.
    let again = apply(&db, &saved).expect_err("a second run is stale");
    assert_eq!(again.code, ErrorCode::PlanStale);
}

#[test]
fn a_database_changed_since_planning_is_refused_and_nothing_is_applied() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let saved = plan(&db, &t.sql(TO), &spec, Options::default());

    db.connect().batch("ALTER TABLE t ADD COLUMN other int").expect("change by hand");

    let err = apply(&db, &saved).expect_err("stale");
    assert_eq!(err.code, ErrorCode::PlanStale);
    assert!(err.message.contains(&saved.from), "{}", err.message);
    let actual = pgmig::schema_fingerprint(&Source::Database(db.conninfo.clone()), &spec, &Options::default()).expect("fingerprint");
    assert!(err.message.contains(&actual), "{}", err.message);
    assert!(err.message.contains("nothing was applied"), "{}", err.message);
    // The check alone says the same, and neither changed anything: no `name` column.
    let check = api::schema_check_saved(&ApplySavedRequest { target: db.conninfo.clone(), plan: saved }).expect_err("stale");
    assert_eq!(check.code, ErrorCode::PlanStale);
    assert_eq!(column_count(&db, "t"), "2");
    let names = db.connect().query("SELECT column_name FROM information_schema.columns WHERE table_name = 't' ORDER BY 1").expect("query");
    assert_eq!(names, [[Some("id".to_owned())], [Some("other".to_owned())]]);
}

#[test]
fn a_file_whose_sql_is_not_its_plan_is_refused() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let saved = plan(&db, &t.sql(TO), &spec, Options::default());

    // The text a reviewer reads was changed.
    let mut text = saved.clone();
    text.sql = "SELECT 1;\n".into();
    let err = apply(&db, &text).expect_err("sql edited");
    assert_eq!(err.code, ErrorCode::SavedPlan);
    // Or the statements were.
    let mut statements = saved.clone();
    statements.plan.statements = vec!["DROP TABLE public.t".into()];
    assert_eq!(apply(&db, &statements).expect_err("plan edited").code, ErrorCode::SavedPlan);
    // And so is a file read from disk.
    let mut file: serde_json::Value = serde_json::from_str(&saved.to_json()).expect("json");
    file["sql"] = "ALTER TABLE public.t ADD COLUMN evil text;\n".into();
    assert!(matches!(SavedPlan::parse(&file.to_string()), Err(pgmig::Error::SavedPlan(_))));
    // Editing both alike is a different plan that is still what its text says: the reviewed SQL is
    // the executed SQL, and the target's fingerprint is still checked.
    file["plan"]["statements"][0] = "ALTER TABLE public.t ADD COLUMN evil text".into();
    assert!(SavedPlan::parse(&file.to_string()).is_ok());
    assert_eq!(column_count(&db, "t"), "1");
}

#[test]
fn unknown_formats_and_foreign_files_are_refused() {
    let t = Tmp::new();
    let (db, spec) = (target(FROM), ShadowSpec::Pgmem);
    let saved = plan(&db, &t.sql(TO), &spec, Options::default());
    let other = saved.to_json().replace("\"format\": 1", "\"format\": 2");
    let err = SavedPlan::parse(&other).expect_err("format 2");
    assert_eq!(api::ApiError::from(&err).code, ErrorCode::SavedPlan);
    assert!(err.to_string().contains("format 2 is not supported"), "{err}");
    assert!(SavedPlan::parse("{\"statements\": []}").is_err());
    assert!(SavedPlan::read(&t.0.join("missing.json")).is_err());

    let path = t.0.join("plan.json");
    std::fs::write(&path, saved.to_json()).expect("write");
    assert_eq!(SavedPlan::read(&path).expect("read"), saved);
}

#[test]
fn changes_outside_the_planned_scope_do_not_make_a_plan_stale() {
    let db = target("CREATE SCHEMA other; CREATE TABLE other.x (id int); CREATE TABLE public.t (id int);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let options = Options { filter: public_only(), ..Options::default() };
    let saved = plan(&db, &t.sql(TO), &spec, options);
    assert_eq!(saved.sql, "ALTER TABLE public.t ADD COLUMN name text;\n");
    assert_eq!(saved.options.filter, public_only());

    db.connect().batch("ALTER TABLE other.x ADD COLUMN y int").expect("change outside the scope");
    let done = apply(&db, &saved).expect("the plan still applies: the unmanaged schema is not part of its fingerprint");
    assert_eq!(done.fingerprint, saved.to);
    assert_eq!(column_count(&db, "t"), "2");

    // A change inside the scope does.
    let db = target("CREATE SCHEMA other; CREATE TABLE other.x (id int); CREATE TABLE public.t (id int);");
    let options = Options { filter: public_only(), ..Options::default() };
    let saved = plan(&db, &t.sql(TO), &spec, options);
    db.connect().batch("ALTER TABLE public.t ADD COLUMN y int").expect("change inside the scope");
    assert_eq!(apply(&db, &saved).expect_err("stale").code, ErrorCode::PlanStale);
}

#[test]
fn schema_mode_is_carried_by_the_file() {
    let db = target("CREATE SCHEMA app; CREATE TABLE app.t (id int); CREATE TABLE public.u (id int);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let options = Options { schema: Some("app".into()), ..Options::default() };
    let desired = t.sql("CREATE SCHEMA app; CREATE TABLE app.t (id int, name text);");
    let saved = plan(&db, &desired, &spec, options.clone());
    assert_eq!(saved.options, options);
    // A plan that is executed names its objects in full.
    assert_eq!(saved.sql, "ALTER TABLE app.t ADD COLUMN name text;\n");

    db.connect().batch("ALTER TABLE public.u ADD COLUMN y int").expect("change outside the schema");
    apply(&db, &saved).expect("apply");
    assert_eq!(column_count(&db, "t"), "2");
}

#[test]
fn an_empty_plan_is_saved_and_applies_as_a_no_op() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let saved = plan(&db, &t.sql(FROM), &spec, Options::default());
    assert!(saved.plan.is_empty());
    assert_eq!((saved.sql.as_str(), saved.from == saved.to), ("", true));

    let done = apply(&db, &saved).expect("no-op");
    assert_eq!((done.report.statements, done.report.batches), (0, 0));
    assert_eq!(done.fingerprint, saved.to);

    // Still checked against the target: an empty plan for a database that changed is stale.
    db.connect().batch("ALTER TABLE t ADD COLUMN y int").expect("change");
    assert_eq!(apply(&db, &saved).expect_err("stale").code, ErrorCode::PlanStale);
}

#[test]
fn statements_outside_a_transaction_run_as_planned() {
    let db = target("CREATE TABLE t (id int, a int);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let options = Options { concurrent_indexes: true, ..Options::default() };
    let saved = plan(&db, &t.sql("CREATE TABLE t (id int, a int); CREATE INDEX t_a_idx ON t (a);"), &spec, options);
    assert_eq!(saved.plan.non_transactional, [0]);
    assert!(saved.sql.contains("-- pgmig:notx") || saved.sql.contains("CONCURRENTLY"), "{}", saved.sql);

    // Through a file.
    let back = SavedPlan::parse(&saved.to_json()).expect("parse");
    assert_eq!(back, saved);
    let done = apply(&db, &back).expect("apply");
    assert_eq!((done.report.statements, done.report.non_transactional), (1, 1));
    assert_eq!(done.fingerprint, saved.to);
}

#[test]
fn if_exists_and_cascade_are_planned_carried_in_the_file_and_run() {
    let db = target("CREATE TABLE t (id int); CREATE TABLE gone (id int);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let options = Options { if_exists: true, cascade: true, ..Options::default() };
    let saved = plan(&db, &t.sql("CREATE TABLE t (id int); CREATE TABLE fresh (id int);"), &spec, options.clone());
    assert_eq!(
        saved.plan.statements,
        ["CREATE TABLE IF NOT EXISTS public.fresh (\n    id integer\n)", "DROP TABLE IF EXISTS public.gone CASCADE"]
    );
    assert_eq!(saved.options, options);
    let back = SavedPlan::parse(&saved.to_json()).expect("parse");
    assert_eq!(back, saved);
    let done = apply(&db, &back).expect("apply");
    assert_eq!(done.fingerprint, saved.to);
}

#[test]
fn a_policy_that_skips_changes_does_not_expect_the_desired_fingerprint() {
    let db = target("CREATE TABLE t (id int); CREATE TABLE keep (id int);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let options = Options { skip: vec![ChangeKind::DropTable], ..Options::default() };
    let saved = plan(&db, &t.sql("CREATE TABLE t (id int, name text);"), &spec, options);
    assert_eq!(saved.skipped.len(), 1);

    let done = apply(&db, &saved).expect("apply");
    assert!(!done.compared);
    assert_ne!(done.fingerprint, saved.to);
    assert_eq!(column_count(&db, "t"), "2");
}

#[test]
fn a_result_that_is_not_the_planned_state_is_an_error_after_the_changes_are_made() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let mut saved = plan(&db, &t.sql(TO), &spec, Options::default());
    // As if the plan did not make the state it says it makes.
    saved.to = saved.from.clone();

    let err = apply(&db, &saved).expect_err("diverged");
    assert_eq!(err.code, ErrorCode::PlanDiverged);
    assert!(err.message.contains("applied 1 statements") && err.message.contains("stay applied"), "{}", err.message);
    assert_eq!(column_count(&db, "t"), "2");
}
