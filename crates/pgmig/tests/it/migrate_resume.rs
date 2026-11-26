//! Resume after a partial failure without a transaction: fail statement k, fix the file after k,
//! apply again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::revisions::{DEFAULT_SCHEMA, Revision, Table};
use pgmig::{ApplyOptions, Error, IssueKind, MigrationOutcome, migrate_apply, migrate_status};

use crate::support::TestDb;

/// A one-file migration directory in a temp directory, rehashed after every write; removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig_resume_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Dir(path)
    }

    fn write(&self, sql: &str) {
        std::fs::write(self.0.join("1_a.sql"), sql).expect("write migration");
        pgmig::migrate_hash(&self.0).expect("hash");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn apply(db: &TestDb, d: &Dir) -> pgmig::Result<pgmig::MigrateApplyReport> {
    migrate_apply(&db.conninfo, d.path(), &ApplyOptions::default())
}

fn row(db: &TestDb) -> Revision {
    let mut conn = db.connect();
    Table::open(&mut conn, DEFAULT_SCHEMA).expect("open").expect("table").read(&mut conn).expect("read").remove(0)
}

fn count(db: &TestDb, table: &str) -> String {
    db.connect().query(&format!("SELECT count(*) FROM {table}")).expect("count")[0][0].clone().expect("value")
}

/// Statement 1 seeds a unique-keyed table, so running it twice fails with 23505.
const FILE: &str = "-- pgmig:txmode none\n\
CREATE TABLE seed (id int PRIMARY KEY);\n\
INSERT INTO seed VALUES (1);\n\
CREATE TABLE t (id int);\n\
INSERT INTO missing VALUES (1);\n\
INSERT INTO t VALUES (7);\n";

#[test]
fn fail_statement_k_fix_after_k_and_resume() {
    let db = TestDb::new();
    let d = Dir::new();
    d.write(FILE);

    let err = apply(&db, &d).expect_err("statement 4 fails");
    let Error::MigrationFailed { statement_index, outcome, source, .. } = &err else { panic!("{err}") };
    assert_eq!((*statement_index, *outcome, source.code.as_deref()), (3, MigrationOutcome::Partial, Some("42P01")));
    let text = err.to_string();
    assert!(text.contains("pgmig migrate hash") && text.contains("--force"), "{text}");
    let r = row(&db);
    assert_eq!((r.applied, r.total), (3, 5));

    // The status explains the partial state and how to recover.
    let s = migrate_status(&db.conninfo, d.path(), DEFAULT_SCHEMA).expect("status");
    let issue = s.issues.iter().find(|i| i.kind == IssueKind::Failed).expect("failed issue");
    assert!(issue.message.contains("3 of 5"), "{}", issue.message);
    assert!(issue.hint.contains("migrate hash") && issue.hint.contains("--force"), "{}", issue.hint);

    // Fix statement 4 and later; statements 1..3 are untouched.
    d.write(&FILE.replace("INSERT INTO missing", "INSERT INTO t"));
    let r = apply(&db, &d).expect("resume");
    assert_eq!((r.files[0].resumed_after, r.files[0].statements), (3, 2));
    // A re-run of statement 2 would have failed with 23505 (or doubled the row).
    assert_eq!(count(&db, "seed"), "1");
    assert_eq!(count(&db, "t"), "2");
    assert!(row(&db).is_complete());
}

#[test]
fn changing_a_statement_before_k_refuses_the_resume() {
    let db = TestDb::new();
    let d = Dir::new();
    d.write(FILE);
    apply(&db, &d).expect_err("fails");

    d.write(&FILE.replace("VALUES (1);\nCREATE TABLE t", "VALUES (2);\nCREATE TABLE t").replace("INSERT INTO missing", "INSERT INTO t"));
    let err = apply(&db, &d).expect_err("changed applied statement");
    assert!(matches!(&err, Error::Resume { .. }), "{err}");
    let text = err.to_string();
    assert!(text.contains("statement 2") && text.contains("has changed") && text.contains("--force"), "{text}");
    assert_eq!(count(&db, "seed"), "1");
    assert_eq!(row(&db).applied, 3, "nothing ran");
}
