//! The serde surface (`pgmig::api`): the same results as the functions it wraps, and complete.

use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, ErrorCode};
use pgmig::{Change, Options, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// A temporary SQL file, removed on drop.
struct File(std::path::PathBuf);

impl File {
    fn new(sql: &str) -> File {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-api-{}-{}.sql", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, sql).expect("write sql");
        File(path)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn shadow() -> (ShadowSpec, Option<TestDb>) {
    let db = postgres_dsn().map(|_| TestDb::new());
    (db.as_ref().map_or(ShadowSpec::Pgmem, |d| ShadowSpec::Postgres(d.conninfo.clone())), db)
}

#[test]
fn diff_returns_the_plan_and_its_changes() {
    let from = File::new("CREATE TABLE t (id int);\n");
    let to = File::new("CREATE TABLE t (id int NOT NULL);\n");
    let (spec, _db) = shadow();
    let req = api::DiffRequest {
        from: Source::Sql(from.0.clone()),
        to: Source::Sql(to.0.clone()),
        shadow: spec.clone(),
        options: Options::default(),
    };
    let res = api::schema_diff(&req).expect("diff");
    let plan = pgmig::schema_diff(&req.from, &req.to, &spec, &req.options).expect("diff");
    assert_eq!(res.plan, plan);
    assert_eq!(res.plan.to_sql(), "ALTER TABLE public.t ALTER COLUMN id SET NOT NULL;\n");
    assert!(matches!(res.changes.as_slice(), [Change::SetNotNull { column, .. }] if column == "id"), "{:?}", res.changes);
}

#[test]
fn inspect_returns_the_managed_model() {
    let file = File::new("CREATE SCHEMA app;\nCREATE TABLE app.t (id int);\nCREATE TABLE public.u (id int);\n");
    let (spec, _db) = shadow();
    let options = Options { schema: Some("app".into()), ..Options::default() };
    let res = api::schema_inspect(&api::InspectRequest { source: Source::Sql(file.0.clone()), shadow: spec, options }).expect("inspect");
    assert_eq!(res.plan.to_sql(), "CREATE TABLE t (\n    id integer\n);\n");
    let tables: Vec<String> = res.model.tables.keys().map(ToString::to_string).collect();
    assert_eq!(tables, ["app.t"]);
}

#[test]
fn validate_reports_diagnostics_with_their_position() {
    let file = File::new("CREATE TABLE a (id int);\n\nCREATE TABLE c (\n    id nosuchtype\n);\n");
    let (spec, _db) = shadow();
    let res = api::schema_validate(&api::ValidateRequest { source: Source::Sql(file.0.clone()), shadow: spec.clone() }).expect("checked");
    let [d] = res.diagnostics.as_slice() else { panic!("one diagnostic: {res:?}") };
    assert_eq!(d.code, ErrorCode::Load);
    assert_eq!(d.sqlstate.as_deref(), Some("42704"));
    assert_eq!(d.file.as_deref(), Some(file.0.as_path()));
    // pgmem reports no error position (kaeawc/pgmem#166): the statement start stands in.
    let want = if postgres_dsn().is_some() { (4, 8) } else { (3, 1) };
    assert_eq!((d.line, d.col), (Some(want.0), Some(want.1)), "{d}");

    let good = File::new("CREATE TABLE a (id int);\n");
    let res = api::schema_validate(&api::ValidateRequest { source: Source::Sql(good.0.clone()), shadow: spec.clone() }).expect("checked");
    assert!(res.is_valid(), "{res:?}");

    let err = api::schema_validate(&api::ValidateRequest { source: Source::Database("host=nowhere".into()), shadow: spec })
        .expect_err("database");
    assert_eq!(err.code, ErrorCode::NotSql);
}

#[test]
fn plan_apply_then_apply() {
    let db = TestDb::new();
    let desired = File::new("CREATE TABLE t (id int);\n");
    let (spec, _shadow_db) = shadow();
    let req = api::PlanApplyRequest {
        target: db.conninfo.clone(),
        desired: Source::Sql(desired.0.clone()),
        shadow: spec.clone(),
        options: Options::default(),
    };
    let res = api::schema_plan_apply(&req).expect("plan");
    assert!(matches!(res.changes.as_slice(), [Change::CreateTable(_)]), "{:?}", res.changes);
    let report = api::apply(&api::ApplyRequest { target: db.conninfo.clone(), plan: res.plan }).expect("apply");
    assert_eq!((report.batches, report.statements), (1, 1));
    assert!(api::schema_plan_apply(&req).expect("plan again").plan.is_empty());
}
