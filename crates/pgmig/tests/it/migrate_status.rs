//! Adopting an existing database (`migrate baseline`, `migrate apply --baseline`, the dirty-database
//! check), `migrate status` and `migrate set`, on the test backend.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::revisions::{DEFAULT_SCHEMA, Kind, Revision, Table};
use pgmig::{
    ApplyOptions, Error, FileStatus, IssueKind, MigrateApplyReport, Options, RevisionStatus, SetAction, SetOptions, ShadowSpec, Source,
    TxMode, migrate_apply, migrate_baseline, migrate_set, migrate_status,
};

use crate::support::{TestDb, postgres_dsn};

/// A shadow on the test backend; keep the database alive as long as the spec is used.
fn shadow_spec() -> (ShadowSpec, Option<TestDb>) {
    if postgres_dsn().is_none() {
        return (ShadowSpec::Pgmem, None);
    }
    let db = TestDb::new();
    (ShadowSpec::Postgres(db.conninfo.clone()), Some(db))
}

/// A migration directory in a temp directory, hashed after every change; removed on drop.
struct Migrations(PathBuf);

impl Migrations {
    fn new(files: &[(&str, &str)]) -> Migrations {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig_migrate_status_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let m = Migrations(path);
        for (name, sql) in files {
            m.write(name, sql);
        }
        m
    }

    fn write(&self, name: &str, sql: &str) {
        std::fs::create_dir_all(&self.0).expect("create temp dir");
        std::fs::write(self.0.join(name), sql).expect("write migration");
        pgmig::migrate_hash(&self.0).expect("hash");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Migrations {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn apply(db: &TestDb, m: &Migrations, opts: &ApplyOptions) -> pgmig::Result<MigrateApplyReport> {
    migrate_apply(&db.conninfo, m.path(), opts)
}

fn set(db: &TestDb, m: &Migrations, version: &str, force: bool) -> pgmig::Result<pgmig::MigrateSet> {
    migrate_set(&db.conninfo, m.path(), version, &SetOptions { force, ..SetOptions::default() })
}

fn status(db: &TestDb, m: &Migrations) -> pgmig::MigrateStatus {
    migrate_status(&db.conninfo, m.path(), DEFAULT_SCHEMA).expect("status")
}

fn baselined(version: &str) -> ApplyOptions {
    ApplyOptions { baseline: Some(version.to_owned()), ..ApplyOptions::default() }
}

fn rows(db: &TestDb) -> Vec<Revision> {
    let mut conn = db.connect();
    match Table::open(&mut conn, DEFAULT_SCHEMA).expect("open") {
        Some(t) => t.read(&mut conn).expect("read"),
        None => vec![],
    }
}

fn has_table(db: &TestDb, name: &str) -> bool {
    let rows =
        db.connect().query(&format!("SELECT count(*) FROM pg_catalog.pg_class WHERE relname = '{name}' AND relkind = 'r'")).expect("query");
    rows[0][0].as_deref() == Some("1")
}

fn statuses(r: &MigrateApplyReport) -> Vec<(&str, FileStatus)> {
    r.files.iter().map(|f| (f.file.as_str(), f.status)).collect()
}

fn file_statuses(s: &pgmig::MigrateStatus) -> Vec<RevisionStatus> {
    s.files.iter().map(|f| f.status).collect()
}

fn issue_kinds(s: &pgmig::MigrateStatus) -> Vec<IssueKind> {
    s.issues.iter().map(|i| i.kind).collect()
}

/// The schema of an application that predates pgmig.
const EXISTING: &str = "CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('ok', 'sad');
CREATE TABLE app.users (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text NOT NULL, mood app.mood DEFAULT 'ok');
CREATE TABLE public.notes (id serial PRIMARY KEY, user_id bigint REFERENCES app.users (id), body text);
CREATE INDEX notes_user ON public.notes (user_id);
";

const A: &str = "1_a.sql";
const B: &str = "2_b.sql";
const C: &str = "3_c.sql";

/// Done when: a populated database is baselined, then later files apply on top of it.
#[test]
fn baselining_a_populated_database_then_applying_later_files() {
    let db = TestDb::new();
    db.connect().batch(EXISTING).expect("existing schema");
    db.connect().batch("INSERT INTO app.users (name) VALUES ('ann')").expect("data");
    let m = Migrations::new(&[]);
    let base = migrate_baseline(m.path(), "baseline", &db.conninfo, &Options::default()).expect("baseline");
    let name = base.path.file_name().and_then(|n| n.to_str()).expect("name").to_owned();
    assert_eq!(name, format!("{}_baseline.sql", base.version));
    pgmig::migrate_validate(m.path(), None).expect("the directory is hashed");

    // The file recreates the database's schema.
    let (shadow, _shadow_db) = shadow_spec();
    let plan =
        pgmig::schema_diff(&Source::Migrations(m.path().into()), &Source::Database(db.conninfo.clone()), &shadow, &Options::default())
            .expect("diff");
    assert!(plan.is_empty(), "{}", plan.to_sql());

    // Without --baseline the populated database is refused.
    let err = apply(&db, &m, &ApplyOptions::default()).expect_err("dirty");
    assert!(matches!(&err, Error::DirtyDatabase { count, .. } if *count >= 4), "{err}");
    assert!(err.to_string().contains("--baseline") && err.to_string().contains("table app.users"), "{err}");
    assert!(rows(&db).is_empty());

    // A later file, then the baseline: recorded without running, the later file applied.
    let later = format!("{}_add_email.sql", base.version.parse::<u64>().expect("digits") + 1);
    m.write(&later, "ALTER TABLE app.users ADD COLUMN email text;\n");
    let dry = apply(&db, &m, &ApplyOptions { dry_run: true, ..baselined(&base.version) }).expect("dry run");
    assert_eq!(statuses(&dry), [(name.as_str(), FileStatus::Baselined), (later.as_str(), FileStatus::Pending)]);
    assert!(rows(&db).is_empty(), "a dry run records nothing");
    let r = apply(&db, &m, &baselined(&base.version)).expect("apply");
    assert_eq!(statuses(&r), [(name.as_str(), FileStatus::Baselined), (later.as_str(), FileStatus::Applied)]);
    assert_eq!((r.applied, r.statements), (1, 1));
    let stored = rows(&db);
    assert_eq!(stored.iter().map(|r| r.kind).collect::<Vec<_>>(), [Kind::Baseline, Kind::Normal]);
    assert!(stored.iter().all(Revision::is_complete), "{stored:?}");
    let users = db.connect().query("SELECT name, email FROM app.users").expect("query");
    assert_eq!(users, [vec![Some("ann".to_owned()), None]], "the baseline did not run");

    // Passing --baseline again is harmless; the history is up to date.
    let again = apply(&db, &m, &baselined(&base.version)).expect("again");
    assert_eq!(again.applied, 0);
    let s = status(&db, &m);
    assert!(s.is_up_to_date(), "{s:?}");
    assert_eq!(s.current.as_deref(), Some(later.split('_').next().expect("version")));
    assert_eq!(s.files[0].kind.as_deref(), Some("baseline"));

    // A fresh database runs the baseline file like any other and ends up the same.
    let fresh = TestDb::new();
    let r = apply(&fresh, &m, &ApplyOptions::default()).expect("fresh");
    assert_eq!(r.applied, 2);
    let plan =
        pgmig::schema_diff(&Source::Database(fresh.conninfo.clone()), &Source::Database(db.conninfo.clone()), &shadow, &Options::default())
            .expect("diff");
    assert!(plan.is_empty(), "{}", plan.to_sql());
}

#[test]
fn a_baseline_must_be_the_first_file() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n")]);
    let err = migrate_baseline(m.path(), "base", &db.conninfo, &Options::default()).expect_err("not first");
    assert!(matches!(&err, Error::Options(msg) if msg.contains("first file")), "{err}");
}

#[test]
fn a_dirty_database_needs_baseline_or_allow_dirty() {
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE legacy (id int)").expect("legacy");
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\n")]);
    for opts in [ApplyOptions::default(), ApplyOptions { dry_run: true, ..ApplyOptions::default() }] {
        let err = apply(&db, &m, &opts).expect_err("dirty");
        assert!(matches!(&err, Error::DirtyDatabase { count: 1, objects } if objects == &["table public.legacy"]), "{err}");
    }
    assert!(!has_table(&db, "pgmig_revisions"), "a refused run creates nothing");
    let err = apply(&db, &m, &baselined("9")).expect_err("no such version");
    assert!(matches!(&err, Error::Migrate(_)) && err.to_string().contains("no migration has the version"), "{err}");
    // A prefix is not enough for history.
    let m2 = Migrations::new(&[("10_a.sql", "SELECT 1;\n")]);
    assert!(apply(&db, &m2, &baselined("1")).is_err());

    let r = apply(&db, &m, &ApplyOptions { allow_dirty: true, ..ApplyOptions::default() }).expect("allow dirty");
    assert_eq!(r.applied, 2);
    assert!(has_table(&db, "a") && has_table(&db, "b"));
    // With history, the check is over: a new file applies without the flag.
    m.write(C, "CREATE TABLE c (id int);\n");
    apply(&db, &m, &ApplyOptions::default()).expect("history");
    // --baseline of an applied version is ignored; of another, refused.
    assert_eq!(apply(&db, &m, &baselined("2")).expect("ignored").applied, 0);
    m.write("4_d.sql", "CREATE TABLE d (id int);\n");
    let err = apply(&db, &m, &baselined("4")).expect_err("has history");
    assert!(matches!(&err, Error::Options(msg) if msg.contains("already has migration history")), "{err}");
    assert!(!has_table(&db, "d"));

    // An empty database needs neither; the baseline of a file in the middle records the files
    // before it too.
    let clean = TestDb::new();
    apply(&clean, &m, &ApplyOptions { limit: Some(1), ..ApplyOptions::default() }).expect("clean");
    let other = TestDb::new();
    other.connect().batch("CREATE TABLE a (id int); CREATE TABLE b (id int)").expect("a and b");
    let r = apply(&other, &m, &baselined("2_b.sql")).expect("baseline by name");
    assert_eq!(
        statuses(&r),
        [(A, FileStatus::Baselined), (B, FileStatus::Baselined), (C, FileStatus::Applied), ("4_d.sql", FileStatus::Applied)]
    );
}

const PARTIAL: &str = "CREATE TABLE x (id int);\nCREATE TABLE y (id int);\nINSERT INTO nope VALUES (1);\n";

#[test]
fn status_reports_each_state() {
    // Fresh: no table, everything pending.
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\n")]);
    let s = status(&db, &m);
    assert_eq!((s.revisions_table.as_deref(), s.current.as_deref(), s.pending.clone()), (None, None, vec![A.to_owned(), B.to_owned()]));
    assert!(s.issues.is_empty() && !s.is_up_to_date(), "{s:?}");

    // Pending, then up to date.
    apply(&db, &m, &ApplyOptions { limit: Some(1), ..ApplyOptions::default() }).expect("one");
    let s = status(&db, &m);
    assert_eq!((s.current.as_deref(), s.pending.clone()), (Some("1"), vec![B.to_owned()]));
    assert_eq!(file_statuses(&s), [RevisionStatus::Applied, RevisionStatus::Pending]);
    assert_eq!(s.revisions_table.as_deref(), Some("pgmig.pgmig_revisions"));
    apply(&db, &m, &ApplyOptions::default()).expect("rest");
    assert!(status(&db, &m).is_up_to_date());

    // Partial: a failure without a transaction, then left in flight.
    m.write(C, PARTIAL);
    let _ = apply(&db, &m, &ApplyOptions { tx_mode: TxMode::None, ..ApplyOptions::default() }).expect_err("fails");
    let s = status(&db, &m);
    assert_eq!((s.current.as_deref(), s.pending.clone(), issue_kinds(&s)), (Some("2"), vec![C.to_owned()], vec![IssueKind::Failed]));
    assert_eq!(s.files[2].status, RevisionStatus::Partial);
    assert!(s.last_error.as_deref().is_some_and(|e| e.contains("42P01")), "{s:?}");
    assert!(s.issues[0].message.contains("2 of 3 statements applied"), "{:?}", s.issues);
    mark_in_flight(&db, 2);
    let s = status(&db, &m);
    assert_eq!(issue_kinds(&s), [IssueKind::InFlight]);
    assert!(s.issues[0].hint.contains("pgmig migrate set 3"), "{}", s.issues[0].hint);

    // A changed file, a missing one, an out-of-order one and a hand-edited row.
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (C, "CREATE TABLE c (id int);\n")]);
    apply(&db, &m, &ApplyOptions::default()).expect("apply");
    m.write(A, "CREATE TABLE a (id bigint);\n");
    m.write(B, "CREATE TABLE b (id int);\n");
    std::fs::remove_file(m.path().join(C)).expect("rm");
    pgmig::migrate_hash(m.path()).expect("hash");
    let s = status(&db, &m);
    assert_eq!(issue_kinds(&s), [IssueKind::Changed, IssueKind::OutOfOrder, IssueKind::Missing]);
    assert_eq!(s.missing.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(), ["3"]);
    db.connect().batch("UPDATE pgmig.pgmig_revisions SET description = 'x' WHERE version = '1'").expect("edit");
    let s = status(&db, &m);
    assert_eq!(s.files[0].status, RevisionStatus::Edited);
    assert_eq!(issue_kinds(&s)[0], IssueKind::Edited);

    // A directory that does not match its sum file is reported, not refused.
    std::fs::write(m.path().join(B), "CREATE TABLE bb (id int);\n").expect("edit");
    assert_eq!(issue_kinds(&status(&db, &m))[0], IssueKind::Directory);
}

/// What a run that died while running statement `index` (0-based) of the only partial row leaves.
fn mark_in_flight(db: &TestDb, index: usize) {
    let mut conn = db.connect();
    let table = Table::open(&mut conn, DEFAULT_SCHEMA).expect("open").expect("table");
    let mut row = rows(db).into_iter().find(|r| !r.is_complete()).expect("a partial row");
    row.start_statement(index, "INSERT INTO nope VALUES (1)");
    table.update(&mut conn, &row).expect("mark");
}

#[test]
fn set_resolves_a_statement_left_in_flight() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, &format!("-- pgmig:txmode none\n{PARTIAL}"))]);
    let _ = apply(&db, &m, &ApplyOptions::default()).expect_err("fails");
    mark_in_flight(&db, 2);
    assert!(matches!(apply(&db, &m, &ApplyOptions::default()), Err(Error::Resume { .. })));

    // It took effect after all (here: by hand); record the file as applied.
    db.connect().batch("CREATE TABLE nope (id int); INSERT INTO nope VALUES (1)").expect("by hand");
    // Recording it as complete needs --force: statement 3 may not have run.
    let err = set(&db, &m, "1", false).expect_err("resolve needs force");
    assert!(matches!(&err, Error::NeedsForce { reasons, .. } if reasons.len() == 1), "{err}");
    let msg = err.to_string();
    assert!(msg.contains("record version 1 (1_a.sql) as complete") && msg.contains("statement 3 was left in flight"), "{msg}");
    assert!(!rows(&db)[0].is_complete(), "a refused set changes nothing");
    let dry =
        migrate_set(&db.conninfo, m.path(), "1", &SetOptions { dry_run: true, force: true, ..SetOptions::default() }).expect("dry run");
    assert_eq!(dry.changes.iter().map(|c| c.action).collect::<Vec<_>>(), [SetAction::Resolve]);
    assert!(!rows(&db)[0].is_complete(), "a dry run changes nothing");
    let r = set(&db, &m, "1", true).expect("set");
    assert_eq!((r.version.as_deref(), r.changes.len(), r.changes[0].action), (Some("1"), 1, SetAction::Resolve));
    let row = &rows(&db)[0];
    assert!(row.is_complete() && row.in_flight().is_none(), "{row:?}");
    assert_eq!((row.applied, row.total, row.kind), (3, 3, Kind::Normal));

    // `migrate apply` continues with the next file.
    m.write(B, "CREATE TABLE b (id int);\n");
    let r = apply(&db, &m, &ApplyOptions::default()).expect("continues");
    assert_eq!(statuses(&r), [(A, FileStatus::AlreadyApplied), (B, FileStatus::Applied)]);
    assert!(status(&db, &m).is_up_to_date());
    // Nothing left to change.
    assert!(set(&db, &m, "2", false).expect("same").changes.is_empty());

    // Or it did not: undo what was applied and drop the row, so the file runs again.
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, &format!("-- pgmig:txmode none\n{PARTIAL}"))]);
    let _ = apply(&db, &m, &ApplyOptions::default()).expect_err("fails");
    mark_in_flight(&db, 2);
    db.connect().batch("DROP TABLE x; DROP TABLE y").expect("undo");
    let r = set(&db, &m, "1", false).expect("drop the partial row without --force");
    assert_eq!(r.changes.iter().map(|c| (c.version.as_str(), c.action)).collect::<Vec<_>>(), [("2", SetAction::Delete)]);
    m.write(B, &format!("-- pgmig:txmode none\n{}", PARTIAL.replace("INSERT INTO nope VALUES (1);\n", "")));
    let r = apply(&db, &m, &ApplyOptions::default()).expect("runs again");
    assert_eq!(r.files[1].resumed_after, 0);
    assert!(has_table(&db, "x") && has_table(&db, "y"));
}

#[test]
fn set_refuses_to_drop_complete_rows_without_force() {
    let db = TestDb::new();
    let m = Migrations::new(&[
        (A, "CREATE TABLE a (id int);\n"),
        (B, "CREATE TABLE b (id int);\n"),
        (C, "CREATE TABLE IF NOT EXISTS c (id int);\n"),
    ]);
    apply(&db, &m, &ApplyOptions::default()).expect("apply");
    let err = set(&db, &m, "2", false).expect_err("needs force");
    assert!(matches!(&err, Error::NeedsForce { reasons, .. } if reasons.len() == 1), "{err}");
    assert!(err.to_string().contains("drop the row of applied version 3 (3_c.sql)") && err.to_string().contains("--force"), "{err}");
    assert_eq!(rows(&db).len(), 3, "nothing changed");
    let r = set(&db, &m, "2", true).expect("forced");
    assert_eq!(r.changes.iter().map(|c| (c.version.as_str(), c.action)).collect::<Vec<_>>(), [("3", SetAction::Delete)]);
    assert_eq!(status(&db, &m).pending, [C]);
    apply(&db, &m, &ApplyOptions::default()).expect("re-applies 3");

    // An applied file edited since needs --force to be recorded as it is now.
    m.write(A, "CREATE TABLE a (id int); -- edited\n");
    assert!(matches!(apply(&db, &m, &ApplyOptions::default()), Err(Error::History(_))));
    let err = set(&db, &m, "3", false).expect_err("rehash needs force");
    assert!(err.to_string().contains("record 1_a.sql as it is now"), "{err}");
    assert_eq!(set(&db, &m, "3", true).expect("rehash").changes[0].action, SetAction::Rehash);
    apply(&db, &m, &ApplyOptions::default()).expect("accepted");

    // A hand-edited row is rewritten.
    db.connect().batch("UPDATE pgmig.pgmig_revisions SET execution_ms = 0, description = 'oops' WHERE version = '2'").expect("edit");
    assert!(matches!(apply(&db, &m, &ApplyOptions::default()), Err(Error::Revisions(_))));
    assert_eq!(set(&db, &m, "3", false).expect("repair").changes[0].action, SetAction::Repair);
    assert_eq!(rows(&db)[1].description, "b");

    // `0` clears the history; set records files without running them.
    assert!(set(&db, &m, "0", false).is_err());
    let r = set(&db, &m, "0", true).expect("clear");
    assert_eq!((r.version, r.changes.len()), (None, 3));
    assert!(rows(&db).is_empty());
    let r = set(&db, &m, "3", false).expect("record");
    assert!(r.changes.iter().all(|c| c.action == SetAction::Insert), "{r:?}");
    assert_eq!(apply(&db, &m, &ApplyOptions::default()).expect("nothing").applied, 0);
    // A version no file has is refused.
    assert!(matches!(set(&db, &m, "7", true), Err(Error::Migrate(_))));
}

#[test]
fn set_needs_force_to_resolve_a_failed_or_interrupted_row() {
    // A failure without a transaction: statements 1-2 applied, 3 failed.
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, &format!("-- pgmig:txmode none\n{PARTIAL}"))]);
    let _ = apply(&db, &m, &ApplyOptions::default()).expect_err("fails");
    let err = set(&db, &m, "2", false).expect_err("failed row needs force");
    let msg = err.to_string();
    assert!(msg.contains("record version 2 (2_b.sql) as complete, though it failed at statement 3: statement 3 may not have run"), "{msg}");
    // Below the target too.
    m.write(C, "CREATE TABLE c (id int);\n");
    let err = set(&db, &m, "3", false).expect_err("below the target");
    assert!(matches!(&err, Error::NeedsForce { reasons, .. } if reasons.len() == 1 && reasons[0].contains("2_b.sql")), "{err}");
    assert_eq!(rows(&db).len(), 2, "nothing changed");

    // Stopped part way without an error.
    let mut conn = db.connect();
    let table = Table::open(&mut conn, DEFAULT_SCHEMA).expect("open").expect("table");
    let mut row = rows(&db).into_iter().find(|r| !r.is_complete()).expect("a partial row");
    row.error = None;
    row.error_stmt = None;
    row.applied = 1;
    row.partial_hashes.truncate(1);
    table.update(&mut conn, &row).expect("interrupted");
    let err = set(&db, &m, "3", false).expect_err("interrupted row needs force");
    assert!(err.to_string().contains("it stopped after 1 of 3 statements: statements 2-3 may not have run"), "{err}");

    // With --force: resolved, and the file without a row recorded.
    let r = set(&db, &m, "3", true).expect("forced");
    assert_eq!(
        r.changes.iter().map(|c| (c.version.as_str(), c.action)).collect::<Vec<_>>(),
        [("2", SetAction::Resolve), ("3", SetAction::Insert)]
    );
    assert!(rows(&db).iter().all(Revision::is_complete));
}

#[test]
fn baseline_on_an_empty_database_runs_every_file() {
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\n"), (C, "CREATE TABLE c (id int);\n")]);
    // An empty database (an empty extra schema does not count): every file runs.
    let db = TestDb::new();
    db.connect().batch("CREATE SCHEMA spare").expect("empty schema");
    let dry = apply(&db, &m, &ApplyOptions { dry_run: true, ..baselined("2") }).expect("dry run");
    assert!(dry.baseline_skipped);
    assert_eq!(statuses(&dry), [(A, FileStatus::Pending), (B, FileStatus::Pending), (C, FileStatus::Pending)]);
    let r = apply(&db, &m, &baselined("2")).expect("apply");
    assert!(r.baseline_skipped);
    assert_eq!(statuses(&r), [(A, FileStatus::Applied), (B, FileStatus::Applied), (C, FileStatus::Applied)]);
    assert!(has_table(&db, "a") && has_table(&db, "b") && has_table(&db, "c"));
    assert!(rows(&db).iter().all(|r| r.kind == Kind::Normal && r.is_complete()));
    // Then it has history with that version: ignored.
    let again = apply(&db, &m, &baselined("2")).expect("again");
    assert_eq!((again.applied, again.baseline_skipped), (0, false));
    // A version no file has is still refused on an empty database.
    let empty = TestDb::new();
    assert!(matches!(apply(&empty, &m, &baselined("9")), Err(Error::Migrate(_))));

    // A populated database is baselined.
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE a (id int); CREATE TABLE b (id int)").expect("a and b");
    let r = apply(&db, &m, &baselined("2")).expect("baseline");
    assert!(!r.baseline_skipped);
    assert_eq!(statuses(&r), [(A, FileStatus::Baselined), (B, FileStatus::Baselined), (C, FileStatus::Applied)]);
}

#[test]
fn only_schemas_with_objects_make_a_database_dirty() {
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n")]);
    let db = TestDb::new();
    db.connect().batch("CREATE SCHEMA spare; CREATE SCHEMA other").expect("empty schemas");
    assert_eq!(apply(&db, &m, &ApplyOptions { dry_run: true, ..ApplyOptions::default() }).expect("clean").files.len(), 1);
    db.connect().batch("CREATE VIEW spare.v AS SELECT 1 AS one").expect("view");
    let err = apply(&db, &m, &ApplyOptions::default()).expect_err("a view is an object");
    assert!(matches!(&err, Error::DirtyDatabase { count: 1, objects } if objects == &["view spare.v"]), "{err}");
}
