//! The concurrent index policy (`Options::concurrent_indexes`): plans with statements outside a
//! transaction apply, verify and become migration files, one per unit; keys are added by building
//! their index concurrently and attaching it with `USING INDEX`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, ErrorCode, PlanApplyRequest};
use pgmig::{ApplyOptions, Error, Options, Plan, ShadowSpec, Source, apply, migrate_apply, schema_plan_apply};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory, removed on drop: desired-state files, and migrations in `migrations/`.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "pgmig-concurrent-indexes-it-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tmp(path)
    }

    fn sql(&self, sql: &str) -> Source {
        let path = self.0.join("schema.sql");
        std::fs::write(&path, sql).expect("write schema.sql");
        Source::Sql(path)
    }

    fn dir(&self) -> PathBuf {
        self.0.join("migrations")
    }

    /// Write a migration and rehash the directory.
    fn migration(&self, name: &str, sql: &str) {
        std::fs::create_dir_all(self.dir()).expect("create migrations");
        std::fs::write(self.dir().join(name), sql).expect("write migration");
        pgmig::migrate_hash(&self.dir()).expect("hash");
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

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir().join(name)).expect("read migration")
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

fn concurrent() -> Options {
    Options { concurrent_indexes: true, ..Options::default() }
}

/// The indexes of the database `db` as `(name, indisvalid)`, for user tables.
fn indexes(db: &TestDb) -> Vec<(String, bool)> {
    let rows = db
        .connect()
        .query(
            "SELECT i.relname, x.indisvalid FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
             JOIN pg_namespace n ON n.oid = i.relnamespace WHERE n.nspname = 'public' ORDER BY 1",
        )
        .expect("query indexes");
    rows.into_iter().map(|r| (r[0].clone().expect("name"), r[1].as_deref() == Some("t"))).collect()
}

fn statements_outside(plan: &Plan) -> Vec<&str> {
    plan.non_transactional.iter().map(|&i| plan.statements[i].as_str()).collect()
}

const FROM: &str = "CREATE TABLE t (id int PRIMARY KEY, a int, b int);
CREATE INDEX t_a_idx ON t (a);";
const TO: &str = "CREATE TABLE t (id int PRIMARY KEY, a int, b int);
CREATE INDEX t_b_idx ON t (b);
CREATE UNIQUE INDEX t_id_b_idx ON t (id, b);";

#[test]
fn schema_apply_creates_and_drops_indexes_concurrently() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let desired = t.sql(TO);

    let plan = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan");
    assert_eq!(
        statements_outside(&plan),
        [
            "DROP INDEX CONCURRENTLY public.t_a_idx",
            "CREATE INDEX CONCURRENTLY t_b_idx ON public.t USING btree (b)",
            "CREATE UNIQUE INDEX CONCURRENTLY t_id_b_idx ON public.t USING btree (id, b)",
        ]
    );
    assert!(plan.units().iter().all(|u| !u.transactional), "{}", plan.to_sql());

    let report = apply(&db.conninfo, &plan).expect("apply");
    assert_eq!((report.statements, report.batches, report.non_transactional), (3, 0, 3));
    assert_eq!(indexes(&db), [("t_b_idx".into(), true), ("t_id_b_idx".into(), true), ("t_pkey".into(), true)]);

    let again = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan again");
    assert!(again.is_empty(), "changes left after apply:\n{}", again.to_sql());
}

#[test]
fn without_the_policy_indexes_are_plain_and_in_one_transaction() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let plan = schema_plan_apply(&db.conninfo, &t.sql(TO), &spec, &Options::default()).expect("plan");
    assert!(plan.non_transactional.is_empty(), "{}", plan.to_sql());
    assert!(plan.statements.iter().all(|s| !s.contains("CONCURRENTLY")), "{}", plan.to_sql());
    let report = apply(&db.conninfo, &plan).expect("apply");
    assert_eq!((report.batches, report.non_transactional), (1, 0));
}

#[test]
fn indexes_on_a_new_table_stay_plain_in_the_transaction() {
    let db = target("CREATE TABLE other (id int);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let desired = t.sql("CREATE TABLE other (id int);\nCREATE TABLE n (id int, a int);\nCREATE INDEX n_a_idx ON n (a);");
    let plan = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan");
    assert!(plan.non_transactional.is_empty(), "{}", plan.to_sql());
    assert!(plan.statements.iter().any(|s| s.starts_with("CREATE INDEX n_a_idx")), "{}", plan.to_sql());
    let report = apply(&db.conninfo, &plan).expect("apply");
    assert_eq!((report.batches, report.non_transactional), (1, 0));
}

#[test]
fn an_index_dropped_with_its_table_is_dropped_by_the_table() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let plan = schema_plan_apply(&db.conninfo, &t.sql("CREATE TABLE other (id int);"), &spec, &concurrent()).expect("plan");
    assert!(plan.statements.iter().any(|s| s == "DROP TABLE public.t"), "{}", plan.to_sql());
    assert!(plan.statements.iter().all(|s| !s.contains("INDEX")), "{}", plan.to_sql());
    assert!(plan.non_transactional.is_empty());
}

#[test]
fn a_plan_with_other_changes_runs_them_in_a_transaction_around_the_concurrent_index() {
    let db = target("CREATE TABLE t (id int, a int);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let desired = t.sql("CREATE TABLE t (id int, a int, email text);\nCREATE INDEX t_email_idx ON t (lower(email));\nCOMMENT ON INDEX t_email_idx IS 'by email';");

    let plan = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan");
    let units: Vec<bool> = plan.units().iter().map(|u| u.transactional).collect();
    assert_eq!(units, [true, false, true], "{}", plan.to_sql());
    let report = apply(&db.conninfo, &plan).expect("apply");
    assert_eq!((report.statements, report.batches, report.non_transactional), (3, 2, 1));
    assert_eq!(indexes(&db), [("t_email_idx".into(), true)]);
    assert!(schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("again").is_empty());
}

#[test]
fn a_failed_concurrent_build_commits_nothing_and_says_how_to_remove_the_invalid_index() {
    let db = target("CREATE TABLE t (id int, a int); INSERT INTO t VALUES (1, 7), (2, 7);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    // The column is added in a transaction that commits first; the unique build then fails.
    let desired = t.sql("CREATE TABLE t (id int, a int, b int);\nCREATE UNIQUE INDEX t_a_idx ON t (a);");
    let plan = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan");
    let units: Vec<bool> = plan.units().iter().map(|u| u.transactional).collect();
    assert_eq!(units, [true, false], "{}", plan.to_sql());

    let err = apply(&db.conninfo, &plan).expect_err("duplicates");
    let Error::Apply { statement_index, statement, source, committed_batches, committed_outside_transaction, outside_transaction } = &err
    else {
        panic!("{err}")
    };
    assert_eq!(*statement_index, Some(1));
    assert!(statement.starts_with("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx"), "{statement}");
    assert_eq!(source.code.as_deref(), Some("23505"), "{source}");
    assert_eq!((*committed_batches, *committed_outside_transaction, *outside_transaction), (1, 0, true));
    let msg = err.to_string();
    assert!(msg.contains("SQLSTATE 23505"), "{msg}");
    assert!(msg.contains("INVALID index") && msg.contains("DROP INDEX CONCURRENTLY public.t_a_idx"), "{msg}");
    assert!(msg.contains("committed nothing") && msg.contains("1 earlier batch was committed"), "{msg}");

    // The first unit stays applied.
    let columns = db.connect().query("SELECT count(*) FROM information_schema.columns WHERE table_name = 't'").expect("query");
    assert_eq!(columns, vec![vec![Some("3".to_owned())]]);
    // Postgres leaves the failed build behind, INVALID; the statement in the message removes it.
    // pgmem leaves nothing behind (kaeawc/pgmem#279), so there is nothing to check or drop there.
    if postgres_dsn().is_some() {
        assert_eq!(indexes(&db), [("t_a_idx".into(), false)]);
        db.connect().batch("DROP INDEX CONCURRENTLY public.t_a_idx").expect("the suggested statement");
    }
    assert!(indexes(&db).is_empty());

    let json = serde_json::to_value(api::ApiError::from(&err)).expect("serialize");
    assert_eq!(json["apply"]["outcome"], "statement_failed");
    assert_eq!(json["apply"]["committed_batches"], 1);
    assert_eq!(json["sqlstate"], "23505");
}

#[test]
fn an_earlier_statement_outside_a_transaction_stays_applied_and_is_counted() {
    let db = target("CREATE TABLE t (id int, a int); INSERT INTO t VALUES (1, 7), (2, 7);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    // Built in name order: the first succeeds, the unique one over duplicates fails.
    let desired = t.sql("CREATE TABLE t (id int, a int);\nCREATE INDEX a_id_idx ON t (id);\nCREATE UNIQUE INDEX z_a_idx ON t (a);");
    let plan = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan");
    assert_eq!(plan.non_transactional.len(), 2, "{}", plan.to_sql());

    let err = apply(&db.conninfo, &plan).expect_err("duplicates");
    let Error::Apply { committed_batches, committed_outside_transaction, outside_transaction, .. } = &err else { panic!("{err}") };
    assert_eq!((*committed_batches, *committed_outside_transaction, *outside_transaction), (0, 1, true));
    let msg = err.to_string();
    // No transaction was committed, and the message does not say one was.
    assert!(msg.contains("1 earlier statement outside a transaction committed and stays applied"), "{msg}");
    assert!(!msg.contains("batch") && !msg.contains("nothing was committed"), "{msg}");
    let json = serde_json::to_value(api::ApiError::from(&err)).expect("serialize");
    assert_eq!(json["apply"]["committed_batches"], 0);
    assert_eq!(json["apply"]["committed_outside_transaction"], 1);
    assert!(indexes(&db).iter().any(|(name, valid)| name == "a_id_idx" && *valid), "{:?}", indexes(&db));
}

#[test]
fn verify_runs_statements_outside_a_transaction() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let desired = t.sql(TO);
    let req = PlanApplyRequest { target: db.conninfo.clone(), desired, shadow: spec.clone(), options: concurrent(), verify: true };
    // Verification would fail with 25001 (inside a transaction block) if it wrapped the statements
    // in a BEGIN.
    let res = api::schema_plan_apply(&req).expect("a plan that verifies");
    assert_eq!(res.plan.non_transactional.len(), 3);

    // A plan whose statements are marked as transactional is refused by the server: the marker is
    // what keeps them out of a transaction.
    let mut wrong = res.plan.clone();
    wrong.non_transactional.clear();
    let err = api::schema_verify(&api::VerifyRequest {
        target: db.conninfo.clone(),
        desired: req.desired.clone(),
        plan: wrong,
        shadow: spec,
        options: concurrent(),
    });
    // The plan's first statement is a DROP INDEX CONCURRENTLY, which Postgres refuses inside a
    // transaction; pgmem accepts that one (kaeawc/pgmem#280) and refuses the CREATE INDEX
    // CONCURRENTLY after it with the same SQLSTATE, so the check holds on both.
    let err = err.expect_err("CONCURRENTLY inside a transaction");
    assert_eq!(err.code, ErrorCode::VerifyFailed, "{err}");
    assert_eq!(err.sqlstate.as_deref(), Some("25001"), "{err}");
}

#[test]
fn migrate_diff_writes_a_concurrent_index_as_a_file_without_a_transaction() {
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let base = t.sql("CREATE TABLE t (id int, a int);");
    pgmig::migrate_diff(&t.dir(), "init", &base, &spec, &Options::default()).expect("init");

    let to = t.sql("CREATE TABLE t (id int, a int);\nCREATE INDEX t_a_idx ON t (a);");
    let res = pgmig::migrate_diff_with(&t.dir(), "add a index", &to, &spec, &concurrent(), None, true).expect("diff");
    assert_eq!(res.written.len(), 1);
    let name = res.written[0].path.file_name().and_then(|n| n.to_str()).expect("name").to_owned();
    assert_eq!(name, format!("{}_add_a_index.sql", res.written[0].version));
    assert_eq!(
        t.read(&name),
        "-- pgmig:txmode none\n-- Runs outside a transaction: Postgres refuses this statement inside one.\n\
         CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a);\n"
    );
    pgmig::migrate_validate(&t.dir(), None).expect("sum file matches");

    // `migrate apply` runs it without a transaction, so the build is allowed.
    let db = TestDb::new();
    let report = migrate_apply(&db.conninfo, &t.dir(), &ApplyOptions::default()).expect("migrate apply");
    assert_eq!(report.applied, 2);
    assert_eq!(indexes(&db), [("t_a_idx".into(), true)]);
    assert!(pgmig::schema_diff(&Source::Database(db.conninfo.clone()), &to, &spec, &Options::default()).expect("diff").is_empty());
}

#[test]
fn migrate_diff_writes_one_file_per_unit_with_consecutive_versions() {
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let base = t.sql("CREATE TABLE t (id int, a int);\nCREATE INDEX t_old_idx ON t (a);");
    pgmig::migrate_diff(&t.dir(), "init", &base, &spec, &Options::default()).expect("init");

    // A column and its index, a dropped index and a comment: transactional, outside, outside,
    // transactional.
    let to = t.sql(
        "CREATE TABLE t (id int, a int, email text);\nCREATE INDEX t_email_idx ON t (lower(email));\n\
         COMMENT ON INDEX t_email_idx IS 'by email';",
    );
    let res = pgmig::migrate_diff_with(&t.dir(), "email", &to, &spec, &concurrent(), None, true).expect("diff");
    assert_eq!(res.written.len(), 4, "{}", res.sql);
    let versions: Vec<&str> = res.written.iter().map(|w| w.version.as_str()).collect();
    for pair in versions.windows(2) {
        assert_eq!(pair[0].parse::<u64>().expect("digits") + 1, pair[1].parse::<u64>().expect("digits"), "{versions:?}");
    }
    let files = t.files();
    assert_eq!(files.len(), 5, "{files:?}");
    let names: Vec<String> = res.written.iter().map(|w| w.path.file_name().expect("name").to_string_lossy().into_owned()).collect();
    assert_eq!(
        names.iter().map(|n| n.split_once('_').expect("version").1).collect::<Vec<_>>(),
        ["email.sql", "email_2.sql", "email_3.sql", "email_4.sql"]
    );

    assert_eq!(t.read(&names[0]), "ALTER TABLE public.t ADD COLUMN email text;\n");
    assert!(t.read(&names[1]).starts_with("-- pgmig:txmode none\n"), "{}", t.read(&names[1]));
    assert!(t.read(&names[1]).ends_with("DROP INDEX CONCURRENTLY public.t_old_idx;\n"), "{}", t.read(&names[1]));
    assert!(t.read(&names[2]).starts_with("-- pgmig:txmode none\n"), "{}", t.read(&names[2]));
    assert!(t.read(&names[2]).contains("CREATE INDEX CONCURRENTLY t_email_idx ON public.t USING btree (lower(email));"));
    assert_eq!(t.read(&names[3]), "COMMENT ON INDEX public.t_email_idx IS 'by email';\n");
    // The plan as one script is what the response carries.
    assert_eq!(res.sql, res.plan.to_sql());
    assert!(res.sql.contains("-- pgmig:notx"));
    // All hashed together.
    pgmig::migrate_validate(&t.dir(), None).expect("sum file matches");

    let db = TestDb::new();
    let report = migrate_apply(&db.conninfo, &t.dir(), &ApplyOptions::default()).expect("migrate apply");
    assert_eq!(report.applied, 5);
    assert_eq!(indexes(&db), [("t_email_idx".into(), true)]);
    assert!(pgmig::schema_diff(&Source::Database(db.conninfo.clone()), &to, &spec, &Options::default()).expect("diff").is_empty());

    // Nothing left to write.
    let again = pgmig::migrate_diff(&t.dir(), "email", &to, &spec, &concurrent()).expect("again");
    assert!(again.written.is_empty() && again.plan.is_empty());
}

#[test]
fn migrate_diff_without_statements_outside_a_transaction_writes_one_file_as_before() {
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let to = t.sql("CREATE TABLE n (id int, a int);\nCREATE INDEX n_a_idx ON n (a);");
    let res = pgmig::migrate_diff(&t.dir(), "new table", &to, &spec, &concurrent()).expect("diff");
    assert_eq!(res.written.len(), 1);
    assert_eq!(t.files().len(), 1);
    assert_eq!(t.read(&t.files()[0]), res.plan.to_sql());
    assert!(!t.read(&t.files()[0]).contains("txmode"));
}

#[cfg(unix)]
#[test]
fn migrate_diff_edit_refuses_a_plan_that_needs_several_files() {
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let base = t.sql("CREATE TABLE t (id int, a int);");
    pgmig::migrate_diff(&t.dir(), "init", &base, &spec, &Options::default()).expect("init");
    let to = t.sql("CREATE TABLE t (id int, a int);\nCREATE INDEX t_a_idx ON t (a);");
    // An editor that would leave a trace if it ran.
    let marker = t.0.join("editor-ran");
    let editor = vec!["sh".to_owned(), "-c".to_owned(), format!("touch '{}'", marker.display()), "editor".to_owned()];
    let err = pgmig::migrate_diff_edited(&t.dir(), "idx", &to, &spec, &concurrent(), &editor).expect_err("refused");
    assert!(matches!(&err, Error::Options(m) if m.contains("--edit cannot be combined with concurrent_indexes")), "{err}");
    assert_eq!(api::ApiError::from(&err).code, ErrorCode::Usage);
    assert!(!marker.exists(), "the editor was not run");
    assert_eq!(t.files().len(), 1);

    // Without the policy the same change can be edited.
    pgmig::migrate_diff_edited(&t.dir(), "idx", &to, &spec, &Options::default(), &["true".to_owned()]).expect("plain edit");
    assert_eq!(t.files().len(), 2);
}

#[test]
fn the_api_lists_every_file_written() {
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let base = t.sql("CREATE TABLE t (id int);");
    pgmig::migrate_diff(&t.dir(), "init", &base, &spec, &Options::default()).expect("init");
    let req = api::MigrateDiffRequest {
        dir: t.dir(),
        name: "email".into(),
        to: t.sql("CREATE TABLE t (id int, email text);\nCREATE INDEX t_email_idx ON t (email);"),
        shadow: spec,
        options: concurrent(),
        editor: None,
        verify: false,
    };
    let res = api::migrate_diff(&req).expect("diff");
    assert_eq!(res.files.len(), 2);
    assert_eq!(res.file.as_ref(), Some(&res.files[0].path));
    assert_eq!(res.version.as_ref(), Some(&res.files[0].version));
    let json = serde_json::to_value(&res).expect("serialize");
    assert_eq!(json["files"].as_array().map(Vec::len), Some(2));
    assert_eq!(json["plan"]["non_transactional"], serde_json::json!([1]));
}

#[test]
fn the_policy_travels_in_the_options_json() {
    let opts: Options = serde_json::from_str(r#"{"concurrent_indexes":true}"#).expect("options");
    assert!(opts.concurrent_indexes);
    assert_eq!(
        serde_json::to_string(&opts).expect("serialize"),
        r#"{"filter":{"include":[],"exclude":[]},"schema":null,"concurrent_indexes":true}"#
    );
    // Left out when false: the JSON of options that never heard of it is unchanged.
    let json = serde_json::to_string(&Options::default()).expect("serialize");
    assert!(!json.contains("concurrent_indexes"), "{json}");
    let req: api::DiffRequest = serde_json::from_str(
        r#"{"from":{"kind":"sql","data":"a.sql"},"to":{"kind":"sql","data":"b.sql"},"options":{"concurrent_indexes":true}}"#,
    )
    .expect("request");
    assert!(req.options.concurrent_indexes);
}

#[test]
fn a_plan_diff_prints_the_concurrent_statements_and_marks_them() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let res = api::schema_diff(&api::DiffRequest {
        from: Source::Database(db.conninfo.clone()),
        to: t.sql(TO),
        shadow: spec,
        options: concurrent(),
    })
    .expect("diff");
    assert!(res.sql.contains("-- pgmig:notx\nDROP INDEX CONCURRENTLY public.t_a_idx;\n"), "{}", res.sql);
    // The text of a plan reads back as the same plan (what `--edit` relies on).
    assert_eq!(pgmig::plan_from_sql(&res.sql).expect("parse"), res.plan);
}

#[test]
fn indexes_on_a_partitioned_table_are_inspected_and_stay_plain() {
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE p (id int, a int) PARTITION BY RANGE (id); CREATE INDEX p_a_idx ON p (a);").expect("partitioned");
    let model = pgmig::inspect(&mut db.connect()).expect("inspect");
    let p = &model.tables[&pgmig_model::QualName::new("public", "p")];
    assert!(p.indexes.iter().all(|i| i.on_partitioned_table), "{:?}", p.indexes);
    assert_eq!(p.indexes.len(), 1);

    let changes = pgmig::diff(&pgmig::Model::empty_database(), &model);
    let planned = pgmig::plan_with(&changes, &pgmig::PlanOptions { concurrent_indexes: true, ..Default::default() }).expect("plan");
    assert!(planned.non_transactional.is_empty(), "{}", planned.to_sql());
}

#[test]
fn a_saved_plan_is_refused_as_a_migration_and_the_error_says_what_to_do() {
    let db = target(FROM);
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    // What `schema diff --concurrent-indexes > m.sql` writes.
    let plan = pgmig::schema_diff(&Source::Database(db.conninfo.clone()), &t.sql(TO), &spec, &concurrent()).expect("diff");
    assert!(plan.to_sql().contains("-- pgmig:notx"));
    t.migration("1_saved.sql", &plan.to_sql());

    let target = TestDb::new();
    let err = migrate_apply(&target.conninfo, &t.dir(), &ApplyOptions::default()).expect_err("refused");
    let Error::Directive { pos, message, .. } = &err else { panic!("{err}") };
    assert_eq!(pos.line, 2, "{err}");
    assert!(message.contains("pgmig:notx belongs to the text of a plan") && message.contains("`-- pgmig:txmode none`"), "{err}");
    assert!(indexes(&target).is_empty());
    // Replaying the directory (`migrate validate`, `migrate diff`) refuses it the same way.
    let err = pgmig::migrate_validate(&t.dir(), Some(&spec)).expect_err("replay refuses it");
    assert!(matches!(&err, Error::Directive { message, .. } if message.contains("pgmig:notx")), "{err}");
    // `plan_from_sql` still reads it: that is what the directive is for.
    assert_eq!(pgmig::plan_from_sql(&plan.to_sql()).expect("a plan"), plan);
}

#[test]
fn a_failed_concurrent_build_in_a_migration_file_names_the_index_to_drop() {
    let t = Tmp::new();
    t.migration("1_data.sql", "CREATE TABLE t (id int, a int);\nINSERT INTO t VALUES (1, 7), (2, 7);\n");
    t.migration("2_index.sql", "-- pgmig:txmode none\nCREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a);\n");
    let db = TestDb::new();
    let err = migrate_apply(&db.conninfo, &t.dir(), &ApplyOptions::default()).expect_err("duplicates");
    let msg = err.to_string();
    assert!(matches!(&err, Error::MigrationFailed { source, .. } if source.code.as_deref() == Some("23505")), "{msg}");
    assert!(msg.contains("2_index.sql:2:1") && msg.contains("SQLSTATE 23505"), "{msg}");
    assert!(msg.contains("INVALID index") && msg.contains("DROP INDEX CONCURRENTLY public.t_a_idx"), "{msg}");

    // In a file that runs in a transaction the server refuses the statement before building
    // anything (25001): there is no index to remove, and the message does not suggest one.
    let t = Tmp::new();
    t.migration("1_index.sql", "CREATE TABLE t (id int, a int);\nCREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a);\n");
    let db = TestDb::new();
    let err = migrate_apply(&db.conninfo, &t.dir(), &ApplyOptions::default()).expect_err("in a transaction");
    assert!(matches!(&err, Error::MigrationFailed { source, .. } if source.code.as_deref() == Some("25001")), "{err}");
    assert!(!err.to_string().contains("INVALID"), "{err}");
}

/// The primary key and unique constraints of `db` as `(constraint, index, indisvalid)`: the index
/// each one owns (`conindid`).
fn key_indexes(db: &TestDb) -> Vec<(String, String, bool)> {
    let rows = db
        .connect()
        .query(
            "SELECT co.conname, i.relname, x.indisvalid FROM pg_constraint co \
             JOIN pg_class i ON i.oid = co.conindid JOIN pg_index x ON x.indexrelid = co.conindid \
             JOIN pg_namespace n ON n.oid = i.relnamespace WHERE co.contype IN ('p', 'u') AND n.nspname = 'public' ORDER BY 1",
        )
        .expect("query constraints");
    rows.into_iter().map(|r| (r[0].clone().expect("name"), r[1].clone().expect("index"), r[2].as_deref() == Some("t"))).collect()
}

#[test]
fn schema_apply_adds_a_key_by_building_its_index_concurrently_and_attaching_it() {
    let db = target(
        "CREATE TABLE t (id int NOT NULL, a int); CREATE TABLE c (id int, t_a int); \
         INSERT INTO t VALUES (1, 10), (2, 20);",
    );
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let desired = t.sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int, CONSTRAINT t_a_key UNIQUE (a));\n\
         CREATE TABLE c (id int, t_a int CONSTRAINT c_t_a_fkey REFERENCES t (a));",
    );

    let plan = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan");
    assert_eq!(
        plan.statements,
        [
            "CREATE UNIQUE INDEX CONCURRENTLY t_a_key ON public.t USING btree (a)",
            "ALTER TABLE public.t ADD CONSTRAINT t_a_key UNIQUE USING INDEX t_a_key",
            // After the key it uses, in the transaction that attaches it.
            "ALTER TABLE public.c ADD CONSTRAINT c_t_a_fkey FOREIGN KEY (t_a) REFERENCES public.t(a)",
            "CREATE UNIQUE INDEX CONCURRENTLY t_pkey ON public.t USING btree (id)",
            "ALTER TABLE public.t ADD CONSTRAINT t_pkey PRIMARY KEY USING INDEX t_pkey",
        ],
        "{}",
        plan.to_sql()
    );
    let units: Vec<bool> = plan.units().iter().map(|u| u.transactional).collect();
    assert_eq!(units, [false, true, false, true]);

    let report = apply(&db.conninfo, &plan).expect("apply");
    assert_eq!((report.statements, report.batches, report.non_transactional), (5, 2, 2));
    // Each constraint owns the index built for it, under its own name, and the index is valid.
    assert_eq!(key_indexes(&db), [("t_a_key".into(), "t_a_key".into(), true), ("t_pkey".into(), "t_pkey".into(), true)]);
    let again = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan again");
    assert!(again.is_empty(), "changes left after apply:\n{}", again.to_sql());
}

#[test]
fn a_failed_key_build_says_how_to_remove_the_invalid_index_and_leaves_the_constraint_out() {
    let db = target("CREATE TABLE t (id int, a int); INSERT INTO t VALUES (1, 7), (2, 7);");
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let desired = t.sql("CREATE TABLE t (id int, a int, CONSTRAINT t_a_key UNIQUE (a));");
    let plan = schema_plan_apply(&db.conninfo, &desired, &spec, &concurrent()).expect("plan");
    assert_eq!(plan.non_transactional, [0], "{}", plan.to_sql());

    let err = apply(&db.conninfo, &plan).expect_err("duplicates");
    let Error::Apply { statement_index, source, outside_transaction, .. } = &err else { panic!("{err}") };
    assert_eq!((*statement_index, *outside_transaction), (Some(0), true));
    assert_eq!(source.code.as_deref(), Some("23505"), "{source}");
    let msg = err.to_string();
    assert!(msg.contains("INVALID index") && msg.contains("DROP INDEX CONCURRENTLY public.t_a_key"), "{msg}");
    assert!(key_indexes(&db).is_empty());
    // Postgres leaves the failed build behind, INVALID (pgmem leaves nothing, kaeawc/pgmem#279).
    if postgres_dsn().is_some() {
        assert_eq!(indexes(&db), [("t_a_key".into(), false)]);
        db.connect().batch("DROP INDEX CONCURRENTLY public.t_a_key").expect("the suggested statement");
    }
    assert!(indexes(&db).is_empty());
}

#[test]
fn migrate_diff_writes_a_key_as_a_file_without_a_transaction_then_a_transactional_one() {
    let t = Tmp::new();
    let (spec, _keep) = shadow();
    let base = t.sql("CREATE TABLE t (id int, a int);");
    pgmig::migrate_diff(&t.dir(), "init", &base, &spec, &Options::default()).expect("init");

    let to = t.sql("CREATE TABLE t (id int, a int, CONSTRAINT t_a_key UNIQUE (a));");
    let res = pgmig::migrate_diff_with(&t.dir(), "a key", &to, &spec, &concurrent(), None, true).expect("diff");
    assert_eq!(res.written.len(), 2, "{}", res.sql);
    let names: Vec<String> = res.written.iter().map(|w| w.path.file_name().expect("name").to_string_lossy().into_owned()).collect();
    assert_eq!(
        t.read(&names[0]),
        "-- pgmig:txmode none\n-- Runs outside a transaction: Postgres refuses this statement inside one.\n\
         CREATE UNIQUE INDEX CONCURRENTLY t_a_key ON public.t USING btree (a);\n"
    );
    assert_eq!(t.read(&names[1]), "ALTER TABLE public.t ADD CONSTRAINT t_a_key UNIQUE USING INDEX t_a_key;\n");
    pgmig::migrate_validate(&t.dir(), None).expect("sum file matches");

    let db = TestDb::new();
    let report = migrate_apply(&db.conninfo, &t.dir(), &ApplyOptions::default()).expect("migrate apply");
    assert_eq!(report.applied, 3);
    assert_eq!(key_indexes(&db), [("t_a_key".into(), "t_a_key".into(), true)]);
    assert!(pgmig::schema_diff(&Source::Database(db.conninfo.clone()), &to, &spec, &Options::default()).expect("diff").is_empty());
}

#[test]
fn constraints_are_inspected_with_the_index_they_own() {
    let db = TestDb::new();
    db.connect()
        .batch("CREATE TABLE t (id int PRIMARY KEY, a int UNIQUE, b int CHECK (b > 0)); CREATE TABLE p (id int, UNIQUE (id)) PARTITION BY RANGE (id);")
        .expect("tables");
    let model = pgmig::inspect(&mut db.connect()).expect("inspect");
    let t = &model.tables[&pgmig_model::QualName::new("public", "t")];
    let index = |name: &str| t.constraint(name).expect("constraint").index_definition.clone();
    assert_eq!(index("t_pkey").as_deref(), Some("CREATE UNIQUE INDEX t_pkey ON public.t USING btree (id)"));
    assert_eq!(index("t_a_key").as_deref(), Some("CREATE UNIQUE INDEX t_a_key ON public.t USING btree (a)"));
    assert_eq!(index("t_b_check"), None);
    assert!(t.constraints.iter().all(|c| !c.on_partitioned_table));
    let p = &model.tables[&pgmig_model::QualName::new("public", "p")];
    assert!(p.constraints.iter().all(|c| c.on_partitioned_table), "{:?}", p.constraints);
}
