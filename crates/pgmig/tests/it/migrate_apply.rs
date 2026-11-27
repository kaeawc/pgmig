//! `migrate apply`: transaction modes, execution orders, history checks, hooks and the advisory
//! lock, on the test backend.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::revisions::{DEFAULT_SCHEMA, Revision, Table};
use pgmig::{ApplyOptions, Error, ExecOrder, FileStatus, MigrateApplyReport, MigrationOutcome, TxMode, history, migrate_apply};

use crate::support::TestDb;

/// A migration directory in a temp directory, hashed after every change; removed on drop.
struct Migrations(PathBuf);

impl Migrations {
    fn new(files: &[(&str, &str)]) -> Migrations {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig_migrate_apply_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        let m = Migrations(path);
        for (name, sql) in files {
            m.write(name, sql);
        }
        m
    }

    /// Write (or overwrite) a file and rehash.
    fn write(&self, name: &str, sql: &str) {
        std::fs::write(self.0.join(name), sql).expect("write migration");
        pgmig::migrate_hash(&self.0).expect("hash");
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.0.join(name)).expect("remove migration");
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

fn opts() -> ApplyOptions {
    ApplyOptions::default()
}

fn with_mode(tx_mode: TxMode) -> ApplyOptions {
    ApplyOptions { tx_mode, ..ApplyOptions::default() }
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

const A: &str = "1_a.sql";
const B: &str = "2_b.sql";
const C: &str = "3_c.sql";

#[test]
fn applies_pending_files_once() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\nCREATE INDEX b_id ON b (id);\n")]);
    let r = apply(&db, &m, &opts()).expect("apply");
    assert_eq!((r.applied, r.statements), (2, 3));
    assert_eq!(statuses(&r), [(A, FileStatus::Applied), (B, FileStatus::Applied)]);
    assert!(has_table(&db, "a") && has_table(&db, "b"));
    let stored = rows(&db);
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(Revision::is_complete), "{stored:?}");
    assert_eq!(
        stored[1].partial_hashes,
        [history::statement_hash("CREATE TABLE b (id int)"), history::statement_hash("CREATE INDEX b_id ON b (id)")]
    );

    let again = apply(&db, &m, &opts()).expect("again");
    assert_eq!((again.applied, again.statements), (0, 0));
    assert_eq!(statuses(&again), [(A, FileStatus::AlreadyApplied), (B, FileStatus::AlreadyApplied)]);

    // A new file and a limit.
    m.write(C, "CREATE TABLE c (id int);\n");
    m.write("4_d.sql", "CREATE TABLE d (id int);\n");
    let r = apply(&db, &m, &ApplyOptions { limit: Some(1), ..opts() }).expect("limit");
    assert_eq!(statuses(&r)[2..], [(C, FileStatus::Applied), ("4_d.sql", FileStatus::Pending)]);
    assert!(!has_table(&db, "d"));
}

#[test]
fn dry_run_changes_nothing() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n")]);
    let r = apply(&db, &m, &ApplyOptions { dry_run: true, ..opts() }).expect("dry run");
    assert!(r.dry_run);
    assert_eq!((r.applied, statuses(&r)), (0, vec![(A, FileStatus::Pending)]));
    assert!(!has_table(&db, "a"));
    assert!(!has_table(&db, "pgmig_revisions"), "a dry run does not create the revisions table");
}

#[test]
fn file_mode_failure_rolls_back_the_file_and_records_the_error() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\nINSERT INTO nope VALUES (1);\n")]);
    let err = apply(&db, &m, &opts()).expect_err("fails");
    let Error::MigrationFailed { path, pos, statement_index, source, outcome, applied_files, .. } = &err else { panic!("{err}") };
    assert_eq!((path.file_name().and_then(|n| n.to_str()), pos.line, *statement_index), (Some(B), 2, 1));
    assert_eq!((source.code.as_deref(), *outcome, *applied_files), (Some("42P01"), MigrationOutcome::FileRolledBack, 1));
    assert!(err.to_string().contains("2_b.sql:2:") && err.to_string().contains("rolled back"), "{err}");
    assert!(has_table(&db, "a"));
    assert!(!has_table(&db, "b"), "the failing file's transaction is rolled back");
    let stored = rows(&db);
    assert_eq!(stored.len(), 2);
    assert!(stored[0].is_complete());
    let failed = &stored[1];
    assert_eq!((failed.applied, failed.total), (0, 2));
    assert!(failed.error.as_deref().is_some_and(|e| e.contains("42P01")), "{failed:?}");
    assert_eq!(failed.error_stmt.as_deref(), Some("INSERT INTO nope VALUES (1)"));

    // Fixed: the file runs again from the start.
    m.write(B, "CREATE TABLE b (id int);\nINSERT INTO b VALUES (1);\n");
    let r = apply(&db, &m, &opts()).expect("fixed");
    assert_eq!(statuses(&r), [(A, FileStatus::AlreadyApplied), (B, FileStatus::Applied)]);
    assert!(rows(&db).iter().all(Revision::is_complete));
}

const PARTIAL: &str = "CREATE TABLE x (id int);\nCREATE TABLE y (id int);\nINSERT INTO nope VALUES (1);\nCREATE TABLE z (id int);\n";

#[test]
fn no_transaction_records_progress_and_resumes() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, PARTIAL)]);
    let err = apply(&db, &m, &with_mode(TxMode::None)).expect_err("fails");
    assert!(matches!(&err, Error::MigrationFailed { outcome: MigrationOutcome::Partial, statement_index: 2, .. }), "{err}");
    assert!(has_table(&db, "x") && has_table(&db, "y") && !has_table(&db, "z"));
    let row = rows(&db).remove(0);
    assert_eq!((row.applied, row.total, row.partial_hashes.len()), (2, 4, 2));
    assert_eq!(row.error_stmt.as_deref(), Some("INSERT INTO nope VALUES (1)"));

    // Changing a statement that was applied refuses the resume.
    m.write(A, &PARTIAL.replace("TABLE y", "TABLE y2"));
    let err = apply(&db, &m, &with_mode(TxMode::None)).expect_err("changed");
    assert!(matches!(&err, Error::Resume { .. }), "{err}");
    assert!(err.to_string().contains("statement 2"), "{err}");

    // Fixing the failing one resumes after the applied ones, in any mode.
    m.write(A, &PARTIAL.replace("INSERT INTO nope", "INSERT INTO x"));
    let r = apply(&db, &m, &opts()).expect("resume");
    assert_eq!((r.files[0].resumed_after, r.files[0].statements, r.statements), (2, 2, 2));
    assert!(has_table(&db, "z"));
    let row = rows(&db).remove(0);
    assert!(row.is_complete() && row.partial_hashes.len() == 4, "{row:?}");
    assert_eq!(db.connect().query("SELECT count(*) FROM x").expect("count")[0][0].as_deref(), Some("1"));
}

#[test]
fn txmode_directive_runs_a_file_without_a_transaction() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, &format!("-- pgmig:txmode none\n{PARTIAL}")), (B, "CREATE TABLE b (id int);\n")]);
    let err = apply(&db, &m, &with_mode(TxMode::All)).expect_err("all");
    assert!(matches!(&err, Error::TxModeConflict { .. }), "{err}");
    assert!(!has_table(&db, "x"));

    let err = apply(&db, &m, &opts()).expect_err("fails");
    assert!(matches!(&err, Error::MigrationFailed { outcome: MigrationOutcome::Partial, .. }), "{err}");
    assert!(has_table(&db, "x") && has_table(&db, "y"), "statements before the failure stay applied");
    assert_eq!(rows(&db)[0].applied, 2);

    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n-- pgmig:txmode none\nCREATE TABLE b (id int);\n")]);
    let err = apply(&db, &m, &opts()).expect_err("misplaced");
    assert!(matches!(&err, Error::Directive { pos, .. } if pos.line == 2), "{err}");
    let m = Migrations::new(&[(A, "-- pgmig:txmode off\n\nCREATE TABLE a (id int);\n")]);
    assert!(matches!(apply(&db, &m, &opts()), Err(Error::Directive { .. })));
}

#[test]
fn all_mode_rolls_back_everything() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\nINSERT INTO nope VALUES (1);\n")]);
    let err = apply(&db, &m, &with_mode(TxMode::All)).expect_err("fails");
    assert!(matches!(&err, Error::MigrationFailed { outcome: MigrationOutcome::AllRolledBack, applied_files: 0, .. }), "{err}");
    assert!(!has_table(&db, "a") && !has_table(&db, "b"));
    let stored = rows(&db);
    assert_eq!(stored.len(), 1, "only the failing file's error is recorded: {stored:?}");
    assert!(stored[0].version == "2" && stored[0].error.is_some());

    m.write(B, "CREATE TABLE b (id int);\n");
    let r = apply(&db, &m, &with_mode(TxMode::All)).expect("all");
    assert_eq!(r.applied, 2);
    assert!(has_table(&db, "a") && has_table(&db, "b"));
}

#[test]
fn execution_orders() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (C, "CREATE TABLE c (id int);\n")]);
    apply(&db, &m, &opts()).expect("apply");
    m.write(B, "CREATE TABLE b (id int);\n");
    let order = |exec_order| ApplyOptions { exec_order, ..opts() };

    let err = apply(&db, &m, &order(ExecOrder::Linear)).expect_err("linear");
    assert!(matches!(&err, pgmig::Error::History(history::HistoryError::OutOfOrder { last, .. }) if last == "3"), "{err}");
    let r = apply(&db, &m, &order(ExecOrder::LinearSkip)).expect("skip");
    assert_eq!((r.applied, statuses(&r)[1]), (0, (B, FileStatus::Skipped)));
    assert!(!has_table(&db, "b"));
    let r = apply(&db, &m, &order(ExecOrder::NonLinear)).expect("non-linear");
    assert_eq!((r.applied, statuses(&r)[1]), (1, (B, FileStatus::Applied)));
    assert!(has_table(&db, "b"));
    // Inserting it did not invalidate the files applied after it.
    let r = apply(&db, &m, &order(ExecOrder::Linear)).expect("linear again");
    assert_eq!(r.applied, 0);
}

#[test]
fn history_must_match_the_directory() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\n")]);
    apply(&db, &m, &opts()).expect("apply");

    m.write(A, "CREATE TABLE a (id bigint);\n");
    let err = apply(&db, &m, &opts()).expect_err("edited");
    assert!(matches!(&err, Error::History(history::HistoryError::Changed { file, .. }) if file.ends_with(A)), "{err}");
    m.write(A, "CREATE TABLE a (id int);\n");

    m.remove(B);
    let err = apply(&db, &m, &opts()).expect_err("deleted");
    assert!(matches!(&err, Error::History(history::HistoryError::Missing { version, .. }) if version == "2"), "{err}");
    m.write(B, "CREATE TABLE b (id int);\n");

    // An unhashed edit is the sum file's to catch.
    std::fs::write(m.path().join(B), "CREATE TABLE b (id text);\n").expect("write");
    assert!(matches!(apply(&db, &m, &opts()), Err(Error::Checksum(_))));
}

#[test]
fn hand_edited_rows_are_refused() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\nINSERT INTO nope VALUES (1);\n")]);
    let _ = apply(&db, &m, &opts()).expect_err("fails");
    // Marking the failed file applied by hand.
    db.connect().batch("UPDATE pgmig.pgmig_revisions SET applied = total, error = NULL WHERE version = '2'").expect("update");
    let err = apply(&db, &m, &opts()).expect_err("edited row");
    assert!(matches!(&err, Error::Revisions(pgmig::revisions::Error::Edited { version, .. }) if version == "2"), "{err}");
}

#[test]
fn hooks_run_inside_each_transaction() {
    let db = TestDb::new();
    let m = Migrations::new(&[
        (A, "CREATE TABLE h (v text);\nINSERT INTO h VALUES (current_setting('pgmig.hook', true));\n"),
        (B, "INSERT INTO h VALUES (current_setting('pgmig.hook', true));\n"),
    ]);
    let hooks = ApplyOptions {
        before_sql: Some("SET LOCAL pgmig.hook = 'on'".into()),
        after_sql: Some("INSERT INTO h VALUES ('after')".into()),
        ..opts()
    };
    apply(&db, &m, &hooks).expect("apply");
    let got: Vec<Option<String>> = db.connect().query("SELECT v FROM h").expect("select").into_iter().map(|mut r| r.remove(0)).collect();
    let want = ["on", "after", "on", "after"].map(|s| Some(s.to_owned()));
    assert_eq!(got, want);

    // A failing hook rolls back its file.
    m.write(C, "CREATE TABLE c (id int);\n");
    let err = apply(&db, &m, &ApplyOptions { before_sql: Some("SELECT * FROM nope".into()), ..opts() }).expect_err("hook");
    assert!(
        matches!(&err, Error::MigrationStep { step, source, .. } if step == "before_sql" && source.code.as_deref() == Some("42P01")),
        "{err}"
    );
    assert!(!has_table(&db, "c"));
}

#[test]
fn a_held_lock_times_out_or_can_be_skipped() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n")]);
    let mut holder = db.connect();
    let key = history::lock_key(&history::default_lock_name(DEFAULT_SCHEMA));
    holder.query(&format!("SELECT pg_advisory_lock({key})")).expect("lock");

    let started = std::time::Instant::now();
    let err = apply(&db, &m, &ApplyOptions { lock_timeout_ms: 300, ..opts() }).expect_err("held");
    assert!(matches!(&err, Error::LockTimeout { timeout_ms: 300, .. }), "{err}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(300));
    assert!(err.to_string().contains("pgmig:pgmig"), "{err}");
    assert!(!has_table(&db, "a"));

    // Another lock name does not conflict; neither does no lock.
    let r = apply(&db, &m, &ApplyOptions { lock_name: Some("elsewhere".into()), dry_run: true, ..opts() }).expect("other name");
    assert_eq!(r.files[0].status, FileStatus::Pending);
    apply(&db, &m, &ApplyOptions { lock: false, ..opts() }).expect("no lock");
    assert!(has_table(&db, "a"));
}

#[test]
fn a_run_waits_for_the_lock() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n")]);
    let mut holder = db.connect();
    let key = history::lock_key(&history::default_lock_name(DEFAULT_SCHEMA));
    holder.query(&format!("SELECT pg_advisory_lock({key})")).expect("lock");
    std::thread::scope(|s| {
        let run = s.spawn(|| apply(&db, &m, &ApplyOptions { lock_timeout_ms: 30_000, ..opts() }));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!run.is_finished(), "the run waits while the lock is held");
        assert!(!has_table(&db, "a"));
        holder.query(&format!("SELECT pg_advisory_unlock({key})")).expect("unlock");
        let r = run.join().expect("thread").expect("apply");
        assert_eq!(r.applied, 1);
    });
    // Released at the end: the lock can be taken again at once.
    assert_eq!(holder.query(&format!("SELECT pg_try_advisory_lock({key})")).expect("try")[0][0].as_deref(), Some("t"));
}

#[test]
fn concurrent_applies_apply_each_file_once() {
    let db = TestDb::new();
    let files: Vec<(String, String)> =
        (1..=6).map(|i| (format!("{i}_t{i}.sql"), format!("CREATE TABLE t{i} (id int);\nINSERT INTO t{i} VALUES ({i});\n"))).collect();
    let refs: Vec<(&str, &str)> = files.iter().map(|(n, s)| (n.as_str(), s.as_str())).collect();
    let m = Migrations::new(&refs);
    let reports = std::thread::scope(|s| {
        let runs: Vec<_> = (0..2).map(|_| s.spawn(|| apply(&db, &m, &opts()))).collect();
        runs.into_iter().map(|h| h.join().expect("thread")).collect::<Vec<_>>()
    });
    let applied: usize = reports.iter().map(|r| r.as_ref().map(|r| r.applied).unwrap_or_else(|e| panic!("{e}"))).sum();
    assert_eq!(applied, 6, "each file is applied by exactly one run");
    assert_eq!(rows(&db).len(), 6);
    for i in 1..=6 {
        assert_eq!(db.connect().query(&format!("SELECT count(*) FROM t{i}")).expect("count")[0][0].as_deref(), Some("1"));
    }
}

const MOOD: &str = "CREATE TYPE mood AS ENUM ('sad', 'happy');\nCREATE TABLE people (id int, mood mood DEFAULT 'sad');\n";

/// A file as `migrate diff` writes it ([`pgmig::Plan::to_sql`]): a new enum value, its commit
/// point, then statements using the value. In one transaction Postgres refuses the use (55P04).
fn add_value_and_use(then: &str) -> String {
    let plan = pgmig::Plan {
        statements: vec![
            "ALTER TYPE public.mood ADD VALUE 'ok' AFTER 'sad'".into(),
            "ALTER TABLE public.people ALTER COLUMN mood SET DEFAULT 'ok'::public.mood".into(),
            then.into(),
        ],
        commit_after: Some(0),
        ..pgmig::Plan::default()
    };
    let sql = plan.to_sql();
    assert!(sql.contains("\n-- pgmig:commit\nALTER TABLE"), "{sql}");
    sql
}

fn moods(db: &TestDb) -> Vec<Option<String>> {
    let rows = db.connect().query("SELECT mood::text FROM people ORDER BY id").expect("select");
    rows.into_iter().map(|mut r| r.remove(0)).collect()
}

#[test]
fn file_mode_commits_at_commit_points() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, MOOD), (B, &add_value_and_use("INSERT INTO people (id) VALUES (1)"))]);
    let hooks = ApplyOptions { after_sql: Some("SELECT 1".into()), ..opts() };
    let r = apply(&db, &m, &hooks).expect("apply");
    assert_eq!((r.applied, r.statements), (2, 5));
    assert_eq!(moods(&db), [Some("ok".to_owned())]);
    let row = rows(&db).remove(1);
    assert!(row.is_complete() && row.partial_hashes.len() == 3, "{row:?}");

    // Without a transaction the commit point changes nothing.
    let db = TestDb::new();
    apply(&db, &m, &with_mode(TxMode::None)).expect("none");
    assert_eq!(moods(&db), [Some("ok".to_owned())]);
}

#[test]
fn a_failure_after_a_commit_point_keeps_what_was_committed() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, MOOD), (B, &add_value_and_use("INSERT INTO nope VALUES (1)"))]);
    let err = apply(&db, &m, &opts()).expect_err("fails");
    assert!(matches!(&err, Error::MigrationFailed { outcome: MigrationOutcome::PartlyCommitted, statement_index: 2, .. }), "{err}");
    assert!(err.to_string().contains("since its last commit point"), "{err}");
    let row = rows(&db).remove(1);
    assert_eq!((row.applied, row.total, row.partial_hashes.len()), (1, 3, 1), "{row:?}");
    assert_eq!(row.error_stmt.as_deref(), Some("INSERT INTO nope VALUES (1)"));
    let values = db.connect().query("SELECT count(*) FROM pg_catalog.pg_enum WHERE enumlabel = 'ok'").expect("enum");
    assert_eq!(values[0][0].as_deref(), Some("1"), "the value before the commit point stays");

    // Fixed: the run resumes at the commit point.
    m.write(B, &add_value_and_use("INSERT INTO people (id) VALUES (1)"));
    let r = apply(&db, &m, &opts()).expect("resume");
    assert_eq!((r.files[1].resumed_after, r.files[1].statements), (1, 2));
    assert_eq!(moods(&db), [Some("ok".to_owned())]);
    assert!(rows(&db).iter().all(Revision::is_complete));
}

#[test]
fn all_mode_refuses_commit_points() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, MOOD), (B, &add_value_and_use("INSERT INTO people (id) VALUES (1)"))]);
    let err = apply(&db, &m, &with_mode(TxMode::All)).expect_err("all");
    assert!(matches!(&err, Error::TxModeConflict { path, .. } if path.ends_with(B)), "{err}");
    assert!(err.to_string().contains("commit point (-- pgmig:commit) at line 3"), "{err}");
    assert!(!has_table(&db, "people"), "nothing runs");
}

#[test]
fn misplaced_commit_points_are_refused() {
    let db = TestDb::new();
    for sql in
        ["CREATE TABLE a (id int);\n-- pgmig:commit now\nCREATE TABLE b (id int);\n", "CREATE TABLE a (\n-- pgmig:commit\nid int);\n"]
    {
        let m = Migrations::new(&[(A, sql)]);
        let err = apply(&db, &m, &opts()).expect_err("misplaced");
        assert!(matches!(&err, Error::Directive { pos, .. } if pos.line == 2), "{err}");
    }
    // Before the first or after the last statement it commits nothing.
    let m = Migrations::new(&[(A, "-- pgmig:commit\nCREATE TABLE a (id int);\n\n-- pgmig:commit\n")]);
    apply(&db, &m, &with_mode(TxMode::All)).expect("no commit point");
}

/// A run that stopped between a statement without a transaction and its record leaves the row
/// marking the statement in flight: the next run refuses to guess whether it took effect.
#[test]
fn a_statement_left_in_flight_refuses_the_resume() {
    let db = TestDb::new();
    let m = Migrations::new(&[(A, PARTIAL)]);
    let _ = apply(&db, &m, &with_mode(TxMode::None)).expect_err("fails");
    // What a run that died while running statement 3 leaves behind.
    let mut conn = db.connect();
    let table = Table::open(&mut conn, DEFAULT_SCHEMA).expect("open").expect("table");
    let mut row = rows(&db).remove(0);
    row.start_statement(2, "INSERT INTO nope VALUES (1)");
    table.update(&mut conn, &row).expect("mark");

    m.write(A, &PARTIAL.replace("INSERT INTO nope", "INSERT INTO x"));
    let err = apply(&db, &m, &opts()).expect_err("in flight");
    assert!(matches!(&err, Error::Resume { .. }), "{err}");
    let text = err.to_string();
    assert!(text.contains("statement 3 (line 3) was started without a transaction") && text.contains("migrate set"), "{text}");
    assert!(!has_table(&db, "z"));

    // A failure clears the mark: the error replaces it.
    let db = TestDb::new();
    let _ = apply(&db, &Migrations::new(&[(A, PARTIAL)]), &with_mode(TxMode::None)).expect_err("fails");
    assert_eq!(rows(&db)[0].in_flight(), None);
}
