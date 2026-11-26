//! `migrate ls`, `show`, `edit`, `rm` and `rebase`: each leaves `migrate validate` passing, and the
//! database checks run on the test backend.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::revisions::{DEFAULT_SCHEMA, Kind, Revision, Table};
use pgmig::{
    ApplyOptions, Error, Guard, RevisionStatus, migrate_apply, migrate_edit, migrate_ls, migrate_rebase, migrate_rm, migrate_show,
};

use crate::support::TestDb;

/// A migration directory in a temp directory; removed on drop.
struct Migrations(PathBuf);

impl Migrations {
    /// The files, hashed.
    fn new(files: &[(&str, &str)]) -> Migrations {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig_migrate_maintain_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        for (name, sql) in files {
            std::fs::write(path.join(name), sql).expect("write migration");
        }
        pgmig::migrate_hash(&path).expect("hash");
        Migrations(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        v.sort();
        v
    }

    fn valid(&self) {
        pgmig::migrate_validate(&self.0, None).expect("migrate validate passes");
    }
}

impl Drop for Migrations {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const A: &str = "20261101000000_create_a.sql";
const B: &str = "20261102000000_create_b.sql";
const C: &str = "20261103000000_create_c.sql";

fn three() -> Migrations {
    Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (B, "CREATE TABLE b (id int);\n"), (C, "CREATE TABLE c (id int);\n")])
}

fn guard(db: &TestDb, force: bool) -> Guard {
    Guard { target: Some(db.conninfo.clone()), force, ..Guard::default() }
}

#[test]
fn ls_lists_files_in_order() {
    let m = three();
    let ls = migrate_ls(m.path(), None, DEFAULT_SCHEMA).expect("ls");
    let rows: Vec<_> = ls.files.iter().map(|f| (f.version.as_str(), f.description.as_str(), f.status)).collect();
    assert_eq!(rows, [("20261101000000", "create_a", None), ("20261102000000", "create_b", None), ("20261103000000", "create_c", None)]);
    assert!(ls.missing.is_empty());
}

#[test]
fn ls_reads_the_status_from_the_target() {
    let db = TestDb::new();
    let m = three();
    // No revisions table yet: everything is pending, and ls does not create one.
    let ls = migrate_ls(m.path(), Some(&db.conninfo), DEFAULT_SCHEMA).expect("ls");
    assert!(ls.files.iter().all(|f| f.status == Some(RevisionStatus::Pending)));
    let mut conn = db.connect();
    assert!(Table::open(&mut conn, DEFAULT_SCHEMA).expect("open").is_none(), "ls is read-only");

    migrate_apply(&db.conninfo, m.path(), &ApplyOptions { limit: Some(2), ..ApplyOptions::default() }).expect("apply");
    let ls = migrate_ls(m.path(), Some(&db.conninfo), DEFAULT_SCHEMA).expect("ls");
    let statuses: Vec<_> = ls.files.iter().map(|f| f.status).collect();
    assert_eq!(statuses, [Some(RevisionStatus::Applied), Some(RevisionStatus::Applied), Some(RevisionStatus::Pending)]);

    // A started row is partial; an applied file edited since is changed.
    let table = Table::open(&mut conn, DEFAULT_SCHEMA).expect("open").expect("table");
    let c = pgmig_migrate_file(m.path(), C);
    table.insert(&mut conn, &Revision::start("20261103000000", "create_c", Kind::Normal, 1, &c)).expect("insert");
    std::fs::write(m.path().join(A), "CREATE TABLE a (id bigint);\n").expect("edit");
    let ls = migrate_ls(m.path(), Some(&db.conninfo), DEFAULT_SCHEMA).expect("ls");
    let statuses: Vec<_> = ls.files.iter().map(|f| f.status).collect();
    assert_eq!(statuses, [Some(RevisionStatus::Changed), Some(RevisionStatus::Applied), Some(RevisionStatus::Partial)]);

    // An applied version without its file is missing.
    std::fs::remove_file(m.path().join(B)).expect("remove");
    let ls = migrate_ls(m.path(), Some(&db.conninfo), DEFAULT_SCHEMA).expect("ls");
    assert_eq!(ls.files.len(), 2);
    assert_eq!(
        ls.missing.iter().map(|r| (r.version.as_str(), r.description.as_str())).collect::<Vec<_>>(),
        [("20261102000000", "create_b")]
    );
}

/// The hash the revisions table records for a file.
fn pgmig_migrate_file(dir: &Path, name: &str) -> String {
    let d = pgmig::MigrationDir::open(dir).expect("open");
    d.find(name).expect("file").hash()
}

#[test]
fn show_by_version_name_or_prefix() {
    let m = three();
    let by_version = migrate_show(m.path(), "20261102000000").expect("by version");
    assert_eq!((by_version.file, by_version.sql.as_str()), (m.path().join(B), "CREATE TABLE b (id int);\n"));
    assert_eq!(migrate_show(m.path(), B).expect("by name").version, "20261102000000");
    assert_eq!(migrate_show(m.path(), &m.path().join(C).to_string_lossy()).expect("by path").description, "create_c");
    assert_eq!(migrate_show(m.path(), "2026110").map(|s| s.version).ok(), None, "a prefix of three files");
    assert_eq!(migrate_show(m.path(), "20261103").expect("unique prefix").version, "20261103000000");
}

#[test]
fn unknown_and_ambiguous_migrations_are_errors() {
    let m = three();
    let err = migrate_show(m.path(), "99").expect_err("unknown");
    assert!(matches!(err, Error::Migrate(pgmig_migrate::Error::NotFound { .. })), "{err}");
    let err = migrate_rm(m.path(), "202611", &Guard::default()).expect_err("ambiguous");
    assert!(matches!(&err, Error::Migrate(pgmig_migrate::Error::Ambiguous { candidates, .. }) if candidates.len() == 3), "{err}");
    let err = migrate_rebase(m.path(), &["nope.sql".into()], &Guard::default()).expect_err("unknown");
    assert!(matches!(err, Error::Migrate(pgmig_migrate::Error::NotFound { .. })), "{err}");
    assert_eq!(m.names(), [A, B, C]);
    m.valid();
}

#[test]
fn rm_removes_and_rehashes() {
    let m = three();
    let res = migrate_rm(m.path(), "20261102000000", &Guard::default()).expect("rm");
    assert_eq!((res.file, res.recorded), (m.path().join(B), false));
    assert_eq!(m.names(), [A, C]);
    m.valid();
    // The last file too.
    migrate_rm(m.path(), C, &Guard::default()).expect("rm last");
    migrate_rm(m.path(), A, &Guard::default()).expect("rm only");
    assert!(m.names().is_empty());
    m.valid();
}

#[test]
fn rm_refuses_a_directory_that_does_not_validate() {
    let m = three();
    std::fs::write(m.path().join(A), "CREATE TABLE a (id bigint);\n").expect("edit");
    let err = migrate_rm(m.path(), B, &Guard::default()).expect_err("edited");
    assert!(matches!(err, Error::Checksum(pgmig::ChecksumError::Edited { .. })), "{err}");
    assert_eq!(m.names(), [A, B, C], "nothing removed");
}

#[test]
fn rm_refuses_an_applied_file_unless_forced() {
    let db = TestDb::new();
    let m = three();
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions { limit: Some(1), ..ApplyOptions::default() }).expect("apply");
    let err = migrate_rm(m.path(), A, &guard(&db, false)).expect_err("applied");
    assert!(matches!(&err, Error::MigrationApplied { path, partial: false, .. } if path == &m.path().join(A)), "{err}");
    assert!(err.to_string().contains("--force"), "{err}");
    assert_eq!(m.names(), [A, B, C]);
    // A pending file goes.
    assert!(!migrate_rm(m.path(), C, &guard(&db, false)).expect("pending").recorded);
    // Forced: removed, and reported.
    assert!(migrate_rm(m.path(), A, &guard(&db, true)).expect("forced").recorded);
    assert_eq!(m.names(), [B]);
    m.valid();
}

/// Both branches added a migration: ours is older than theirs, and the sum file is theirs.
fn merged() -> (Migrations, &'static str) {
    let m = Migrations::new(&[(A, "CREATE TABLE a (id int);\n"), (C, "CREATE TABLE theirs (id int);\n")]);
    std::fs::write(m.path().join(B), "CREATE TABLE ours (id int);\n").expect("ours");
    (m, B)
}

#[test]
fn rebase_moves_a_conflicting_file_to_the_end() {
    let (m, ours) = merged();
    let err = pgmig::migrate_validate(m.path(), None).expect_err("inserted");
    assert!(matches!(err, Error::Checksum(pgmig::ChecksumError::Inserted { .. })), "{err}");
    let res = migrate_rebase(m.path(), &["20261102000000".into()], &Guard::default()).expect("rebase");
    assert_eq!(res.moved.len(), 1);
    let moved = &res.moved[0];
    assert_eq!(moved.from, m.path().join(ours));
    assert!(pgmig_migrate::compare_versions(&moved.to_version, "20261103000000").is_gt(), "{moved:?}");
    let new_name = format!("{}_create_b.sql", moved.to_version);
    assert_eq!(moved.to, m.path().join(&new_name));
    assert_eq!(m.names(), [A.to_owned(), C.to_owned(), new_name.clone()], "the old name is gone");
    assert_eq!(std::fs::read_to_string(m.path().join(&new_name)).expect("read"), "CREATE TABLE ours (id int);\n");
    m.valid();
    // Rebasing it again moves nothing: it is already last.
    assert!(migrate_rebase(m.path(), &[new_name], &Guard::default()).expect("again").moved.is_empty());
    m.valid();
}

#[test]
fn rebase_moves_a_hashed_file_too() {
    let m = three();
    let res = migrate_rebase(m.path(), &[A.into(), "20261102".into()], &Guard::default()).expect("rebase");
    let to: Vec<_> = res.moved.iter().map(|r| r.to.file_name().expect("name").to_string_lossy().into_owned()).collect();
    assert_eq!(to.len(), 2);
    assert!(to[0].ends_with("_create_a.sql") && to[1].ends_with("_create_b.sql"), "{to:?}");
    assert_eq!(m.names()[0], C);
    m.valid();
}

#[test]
fn rebase_refuses_when_the_rest_does_not_validate() {
    let (m, ours) = merged();
    std::fs::write(m.path().join(C), "CREATE TABLE theirs (id bigint);\n").expect("edit theirs");
    let err = migrate_rebase(m.path(), &[ours.into()], &Guard::default()).expect_err("edited");
    assert!(matches!(err, Error::Checksum(pgmig::ChecksumError::Edited { .. })), "{err}");
    assert!(m.path().join(ours).is_file(), "nothing moved");
}

#[test]
fn rebase_refuses_an_applied_file_unless_forced() {
    let db = TestDb::new();
    let m = three();
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions { limit: Some(1), ..ApplyOptions::default() }).expect("apply");
    let err = migrate_rebase(m.path(), &[A.into()], &guard(&db, false)).expect_err("applied");
    assert!(matches!(err, Error::MigrationApplied { .. }), "{err}");
    assert_eq!(m.names(), [A, B, C]);
    // Not applied: moves.
    assert_eq!(migrate_rebase(m.path(), &[B.into()], &guard(&db, false)).expect("pending").moved.len(), 1);
    m.valid();
}

#[cfg(unix)]
#[test]
fn edit_runs_the_editor_then_rehashes() {
    let m = three();
    // An editor that appends a statement to the file it is given ($0 of the script).
    let editor: Vec<String> = ["sh", "-c", "printf 'CREATE INDEX b_id ON b (id);\\n' >> \"$0\""].map(String::from).to_vec();
    let res = migrate_edit(m.path(), "20261102000000", &editor).expect("edit");
    assert!(res.changed);
    assert_eq!(std::fs::read_to_string(m.path().join(B)).expect("read"), "CREATE TABLE b (id int);\nCREATE INDEX b_id ON b (id);\n");
    m.valid();
    // An editor that changes nothing.
    assert!(!migrate_edit(m.path(), B, &["true".to_owned()]).expect("unchanged").changed);
    m.valid();
}

#[cfg(unix)]
#[test]
fn edit_failures_rehash_nothing() {
    let m = three();
    let editor: Vec<String> = ["sh", "-c", "echo broken >> \"$0\"; exit 3"].map(String::from).to_vec();
    let err = migrate_edit(m.path(), B, &editor).expect_err("exit 3");
    assert!(matches!(&err, Error::Editor { reason, .. } if reason.contains("was not rehashed")), "{err}");
    let err = pgmig::migrate_validate(m.path(), None).expect_err("not rehashed");
    assert!(matches!(err, Error::Checksum(pgmig::ChecksumError::Edited { .. })), "{err}");
    // An edited directory is refused before the editor runs.
    let err = migrate_edit(m.path(), C, &["true".to_owned()]).expect_err("invalid");
    assert!(matches!(err, Error::Checksum(_)), "{err}");
}

#[test]
fn edit_needs_an_editor_that_runs() {
    let m = three();
    assert!(matches!(migrate_edit(m.path(), B, &[]), Err(Error::Options(_))));
    let err = migrate_edit(m.path(), B, &["pgmig-no-such-editor-anywhere".to_owned()]).expect_err("missing");
    assert!(matches!(err, Error::Editor { .. }), "{err}");
    m.valid();
}
