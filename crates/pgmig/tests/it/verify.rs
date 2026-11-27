//! `--verify`: a plan is applied in a shadow database to a copy of the current state and the result
//! compared with the desired state, before anything runs for real or is written.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, ErrorCode, PlanApplyRequest};
use pgmig::{
    Change, ChangeKind, Current, Error, Filter, Model, Options, Plan, PlanOptions, ShadowSpec, Source, diff, inspect, load_model,
    plan_with, verify_plan, without_schema,
};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-verify-it-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tmp(path)
    }

    fn sql(&self, name: &str, sql: &str) -> Source {
        let path = self.0.join(name);
        std::fs::write(&path, sql).expect("write sql");
        Source::Sql(path)
    }

    fn dir(&self) -> PathBuf {
        self.0.join("migrations")
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".sql")).collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The shadow on the test backend: pgmem, or an empty database on Postgres. Each verification
/// wipes it, so one serves a whole test.
fn shadow() -> (ShadowSpec, Option<TestDb>) {
    match postgres_dsn() {
        None => (ShadowSpec::Pgmem, None),
        Some(_) => {
            let db = TestDb::new();
            (ShadowSpec::Postgres(db.conninfo.clone()), Some(db))
        }
    }
}

const FROM: &str = "CREATE TYPE mood AS ENUM ('sad', 'ok');
CREATE TABLE users (id int PRIMARY KEY, name text, n int);
CREATE TABLE old (x int);";

const TO: &str = "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
CREATE TYPE color AS ENUM ('red', 'green');
CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL, n bigint, mood mood DEFAULT 'ok', c color,
    CONSTRAINT name_nonempty CHECK (name <> ''));
CREATE INDEX users_name ON users (name);
CREATE TABLE posts (id int PRIMARY KEY, user_id int REFERENCES users (id), body text);";

/// A target holding `from`, its whole model (what verification copies), the desired model (as
/// `opts` manages it), and the plan between them.
struct Case {
    db: TestDb,
    tmp: Tmp,
    from: Model,
    to: Model,
    plan: Plan,
}

fn case(from: &str, to: &str, spec: &ShadowSpec, opts: &Options) -> Case {
    let db = TestDb::new();
    db.connect().batch(from).expect("load target");
    let tmp = Tmp::new();
    let from = without_schema(&inspect(&mut db.connect()).expect("inspect"), &opts.revisions_schema);
    let to = opts.managed(&load_model(&tmp.sql("to.sql", to), spec).expect("load"));
    let (changes, _) = opts.apply_policy(&diff(&opts.managed(&from), &to), &PlanOptions::default());
    let plan = plan_with(&changes, &PlanOptions::default()).expect("plan");
    Case { db, tmp, from, to, plan }
}

fn kinds(remaining: &[Change]) -> Vec<ChangeKind> {
    remaining.iter().map(Change::kind).collect()
}

#[test]
fn a_correct_plan_verifies() {
    let (spec, _keep) = shadow();
    let opts = Options::default();
    let c = case(FROM, TO, &spec, &opts);
    assert!(c.plan.statements.len() > 5, "{:?}", c.plan);
    assert!(c.plan.commit_after.is_some(), "the new enum value needs a commit point: {:?}", c.plan);
    verify_plan(Current::Model(&c.from), &c.plan, &c.to, &spec, &opts).expect("verifies");
    // Through the API, before anything is applied.
    let req = PlanApplyRequest {
        target: c.db.conninfo.clone(),
        desired: c.tmp.sql("to.sql", TO),
        shadow: spec.clone(),
        options: opts,
        verify: true,
    };
    let res = api::schema_plan_apply(&req).expect("plan and verify");
    assert_eq!(res.plan, c.plan);
    // Verification only reads the target.
    let n = c.db.connect().query("SELECT count(*) FROM pg_class WHERE relname = 'posts'").expect("query");
    assert_eq!(n[0][0].as_deref(), Some("0"));
}

#[test]
fn an_injected_planner_bug_is_caught() {
    let (spec, _keep) = shadow();
    let opts = Options::default();
    let c = case(FROM, TO, &spec, &opts);

    // A statement left out: what it did remains to do.
    let i = c.plan.statements.iter().position(|s| s.starts_with("CREATE INDEX")).expect("index statement");
    let mut dropped = c.plan.clone();
    dropped.statements.remove(i);
    if dropped.commit_after.is_some_and(|a| a >= i) {
        dropped.commit_after = dropped.commit_after.map(|a| a - 1);
    }
    let err = verify_plan(Current::Model(&c.from), &dropped, &c.to, &spec, &opts).expect_err("misses the index");
    let Error::VerifyFailed { remaining } = &err else { panic!("{err}") };
    assert_eq!(kinds(remaining), [ChangeKind::CreateIndex], "{err}");
    assert!(err.to_string().starts_with("verification failed:"), "{err}");
    assert!(err.to_string().contains("create_index public.users_name"), "{err}");

    // A statement that does the wrong thing: the column gets the wrong type.
    let mut wrong = c.plan.clone();
    let i = wrong.statements.iter().position(|s| s.contains("TYPE bigint")).expect("retype statement");
    wrong.statements[i] = wrong.statements[i].replace("TYPE bigint", "TYPE integer");
    let err = verify_plan(Current::Model(&c.from), &wrong, &c.to, &spec, &opts).expect_err("wrong type");
    let Error::VerifyFailed { remaining } = &err else { panic!("{err}") };
    assert_eq!(kinds(remaining), [ChangeKind::AlterColumnType], "{err}");

    // The API reports it as verify_failed with the changes left.
    let e = api::ApiError::from(&err);
    assert_eq!(e.code, ErrorCode::VerifyFailed);
    assert_eq!(e.code.as_str(), "verify_failed");
    assert_eq!(kinds(&e.verify.expect("verify detail").remaining), [ChangeKind::AlterColumnType]);
}

#[test]
fn a_statement_failing_in_the_shadow_is_reported_with_the_server_error() {
    let (spec, _keep) = shadow();
    let opts = Options::default();
    let c = case(FROM, TO, &spec, &opts);
    let mut bogus = c.plan.clone();
    bogus.statements.insert(0, "ALTER TABLE public.nosuch ADD COLUMN x integer".to_owned());
    bogus.commit_after = bogus.commit_after.map(|a| a + 1);
    let err = verify_plan(Current::Model(&c.from), &bogus, &c.to, &spec, &opts).expect_err("fails");
    let Error::VerifyStatement { statement_index, statement, source } = &err else { panic!("{err}") };
    assert_eq!(*statement_index, Some(0));
    assert_eq!(statement, "ALTER TABLE public.nosuch ADD COLUMN x integer");
    assert_eq!(source.code.as_deref(), Some("42P01"), "{err}");
    let msg = err.to_string();
    assert!(msg.starts_with("verification failed: statement 1 failed in the shadow database: ALTER TABLE public.nosuch"), "{msg}");
    assert!(msg.contains("42P01") && msg.contains("the target was not changed"), "{msg}");
    let e = api::ApiError::from(&err);
    assert_eq!((e.code, e.sqlstate.as_deref()), (ErrorCode::VerifyFailed, Some("42P01")));
    assert_eq!(e.verify.expect("detail").statement_index, Some(0));

    // The shadow was wiped: it verifies the correct plan next.
    verify_plan(Current::Model(&c.from), &c.plan, &c.to, &spec, &opts).expect("verifies after a failure");
}

#[test]
fn skipped_changes_are_expected_to_remain() {
    let (spec, _keep) = shadow();
    let opts = Options { skip: vec![ChangeKind::DropTable], ..Options::default() };
    let c = case(FROM, TO, &spec, &opts);
    assert!(!c.plan.statements.iter().any(|s| s.contains("DROP TABLE")), "{:?}", c.plan);
    verify_plan(Current::Model(&c.from), &c.plan, &c.to, &spec, &opts).expect("old stays, as skipped");
    let req = PlanApplyRequest {
        target: c.db.conninfo.clone(),
        desired: c.tmp.sql("to.sql", TO),
        shadow: spec.clone(),
        options: opts,
        verify: true,
    };
    let res = api::schema_plan_apply(&req).expect("verifies");
    assert_eq!(res.skipped.len(), 1, "{:?}", res.skipped);
    // Without the policy, the same plan leaves the drop to do.
    let err = verify_plan(Current::Model(&c.from), &c.plan, &c.to, &spec, &Options::default()).expect_err("old remains");
    let Error::VerifyFailed { remaining } = &err else { panic!("{err}") };
    assert_eq!(kinds(remaining), [ChangeKind::DropTable], "{err}");
}

#[test]
fn schema_mode_verifies() {
    let (spec, _keep) = shadow();
    let opts = Options { schema: Some("app".into()), ..Options::default() };
    let c =
        case("CREATE SCHEMA app; CREATE TABLE app.t (id int);", "CREATE SCHEMA app; CREATE TABLE app.t (id int, v text);", &spec, &opts);
    verify_plan(Current::Model(&c.from), &c.plan, &c.to, &spec, &opts).expect("verifies");
}

/// The copy holds the whole target, not only the managed part: a managed table's foreign key to an
/// unmanaged one has its target, and an unmanaged one's to a managed table stops the drop there as
/// it would on the target.
#[test]
fn unmanaged_objects_are_copied_too() {
    let (spec, _keep) = shadow();
    let exclude = |table: &str| Options { filter: Filter::parse::<&str, _>(&[], &[table]).expect("filter"), ..Options::default() };
    let tables = "CREATE TABLE parent (id int PRIMARY KEY); CREATE TABLE child (id int, p int REFERENCES parent (id));";

    let opts = exclude("table:public.parent");
    let to = format!("{tables} ALTER TABLE child ADD COLUMN v text;");
    let c = case(tables, &to, &spec, &opts);
    assert_eq!(c.plan.statements.len(), 1, "{:?}", c.plan);
    verify_plan(Current::Model(&c.from), &c.plan, &c.to, &spec, &opts).expect("verifies");
    let req = PlanApplyRequest {
        target: c.db.conninfo.clone(),
        desired: c.tmp.sql("to.sql", &to),
        shadow: spec.clone(),
        options: opts,
        verify: true,
    };
    api::schema_plan_apply(&req).expect("plan and verify");

    let opts = exclude("table:public.child");
    let c = case(tables, "CREATE TABLE child (id int, p int);", &spec, &opts);
    assert!(c.plan.statements.iter().any(|s| s.starts_with("DROP TABLE public.parent")), "{:?}", c.plan);
    let err = verify_plan(Current::Model(&c.from), &c.plan, &c.to, &spec, &opts).expect_err("child's foreign key needs parent");
    assert!(matches!(&err, Error::VerifyStatement { source, .. } if source.code.as_deref() == Some("2BP01")), "{err}");
}

#[test]
fn an_edited_plan_is_verified_against_the_target() {
    let (spec, _keep) = shadow();
    let c = case(FROM, TO, &spec, &Options::default());
    let req = |plan: Plan| api::VerifyRequest {
        target: c.db.conninfo.clone(),
        desired: c.tmp.sql("to.sql", TO),
        plan,
        shadow: spec.clone(),
        options: Options::default(),
    };
    api::schema_verify(&req(c.plan.clone())).expect("verifies");
    let mut edited = c.plan.clone();
    edited.statements.retain(|s| !s.starts_with("CREATE INDEX"));
    let err = api::schema_verify(&req(edited)).expect_err("misses the index");
    assert_eq!(err.code, ErrorCode::VerifyFailed, "{err}");
}

#[test]
fn migrate_diff_verifies_before_writing() {
    let (spec, _keep) = shadow();
    let t = Tmp::new();
    let opts = Options::default();
    let first = pgmig::migrate_diff_with(&t.dir(), "init", &t.sql("a.sql", FROM), &spec, &opts, None, true).expect("first");
    assert!(first.written.is_some());
    let next = pgmig::migrate_diff_with(&t.dir(), "next", &t.sql("b.sql", TO), &spec, &opts, None, true).expect("next");
    assert!(next.written.is_some());
    assert_eq!(t.files().len(), 2);
}

#[cfg(unix)]
#[test]
fn migrate_diff_verify_checks_the_edited_file_and_writes_nothing_on_failure() {
    let (spec, _keep) = shadow();
    let t = Tmp::new();
    let opts = Options::default();
    pgmig::migrate_diff_with(&t.dir(), "init", &t.sql("a.sql", FROM), &spec, &opts, None, true).expect("first");
    let to = t.sql("b.sql", TO);
    let editor = |script: &str| vec!["sh".to_owned(), "-c".to_owned(), script.to_owned(), "editor".to_owned()];

    // The index removed by hand: verification fails, and no file is written.
    let drop_index = editor("grep -v 'CREATE INDEX' \"$1\" > \"$1.new\"; cat \"$1.new\" > \"$1\"; rm \"$1.new\"");
    let err = pgmig::migrate_diff_with(&t.dir(), "next", &to, &spec, &opts, Some(&drop_index), true).expect_err("misses the index");
    let Error::VerifyFailed { remaining } = &err else { panic!("{err}") };
    assert_eq!(kinds(remaining), [ChangeKind::CreateIndex], "{err}");
    assert_eq!(t.files().len(), 1);
    pgmig::migrate_validate(&t.dir(), None).expect("the sum file is unchanged");

    // A statement of the edited file failing: located in the file.
    let bogus = editor("printf 'ALTER TABLE public.nosuch ADD COLUMN x integer;\\n' >> \"$1\"");
    let err = pgmig::migrate_diff_with(&t.dir(), "next", &to, &spec, &opts, Some(&bogus), true).expect_err("fails");
    assert!(matches!(&err, Error::VerifyStatement { source, .. } if source.code.as_deref() == Some("42P01")), "{err}");
    assert_eq!(t.files().len(), 1);

    // Without a transaction, as `-- pgmig:txmode none` says; an extra statement of the user's own
    // that leaves no managed change is fine.
    let no_tx = editor(
        "{ printf -- '-- pgmig:txmode none\\n'; cat \"$1\"; printf 'SELECT 1;\\n'; } > \"$1.new\"; cat \"$1.new\" > \"$1\"; rm \"$1.new\"",
    );
    let res = pgmig::migrate_diff_with(&t.dir(), "next", &to, &spec, &opts, Some(&no_tx), true).expect("verifies");
    assert!(res.edited && res.written.is_some());
    assert_eq!(t.files().len(), 2);
}

/// `migrate diff` replays the directory for the copy: a hand-written migration's objects that pgmig
/// does not model (a function a default calls) are there. Rebuilt from the model, as `schema apply`
/// does, they are not, and setup says why.
#[test]
fn migrate_diff_verify_copies_unmodelled_objects() {
    let (spec, _keep) = shadow();
    let t = Tmp::new();
    let opts = Options::default();
    let first = "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT 1';\nCREATE TABLE t (id int DEFAULT f());\n";
    let new = pgmig::migrate_new(&t.dir(), "init").expect("new");
    std::fs::write(&new.path, first).expect("write migration");
    pgmig::migrate_hash(&t.dir()).expect("hash");
    let to = t.sql("to.sql", &format!("{first}ALTER TABLE t ADD COLUMN v text;\n"));
    let res = pgmig::migrate_diff_with(&t.dir(), "next", &to, &spec, &opts, None, true).expect("verifies");
    assert_eq!(res.plan.statements.len(), 1, "{:?}", res.plan);
    assert_eq!(t.files().len(), 2);

    let db = TestDb::new();
    db.connect().batch(first).expect("load target");
    let req = PlanApplyRequest { target: db.conninfo.clone(), desired: to, shadow: spec.clone(), options: opts, verify: true };
    let err = api::schema_plan_apply(&req).expect_err("f is not copied");
    assert_eq!(err.code, ErrorCode::VerifyFailed, "{err}");
    assert!(err.message.contains("objects outside that (extensions, functions, views, ...) are not copied"), "{err}");
}

#[test]
fn schema_mode_does_not_copy_unmanaged_objects_it_does_not_need() {
    let (spec, _keep) = shadow();
    let opts = Options { schema: Some("app".into()), ..Options::default() };
    // `public.u` uses a function pgmig does not model; nothing in `app` needs it.
    let loaded = "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT 1'; CREATE TABLE public.u (id int DEFAULT f());\n\
                  CREATE SCHEMA app; CREATE TABLE app.t (id int);";
    let db = TestDb::new();
    db.connect().batch(loaded).expect("load target");
    let t = Tmp::new();
    let to = t.sql("to.sql", "CREATE SCHEMA app; CREATE TABLE app.t (id int, v text);");
    let req = PlanApplyRequest { target: db.conninfo.clone(), desired: to, shadow: spec.clone(), options: opts, verify: true };
    let res = api::schema_plan_apply(&req).expect("plan and verify");
    assert_eq!(res.plan.statements.len(), 1, "{:?}", res.plan);
}
