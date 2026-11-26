//! `migrate hash` and `migrate validate`: the sum file, and replaying a migration directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, ErrorCode};
use pgmig::{ChecksumError, Error, SUM_FILE, ShadowSpec};

use crate::support::{TestDb, postgres_dsn};

/// A temporary migration directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-migrate-it-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        let d = Dir(path);
        for (name, text) in files {
            d.write(name, text);
        }
        d
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.0.join(name), text).expect("write file");
    }

    fn hash(&self) {
        pgmig::migrate_hash(&self.0).expect("hash");
    }

    fn validate(&self) -> pgmig::Result<()> {
        pgmig::migrate_validate(&self.0, None)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shadow() -> (ShadowSpec, Option<TestDb>) {
    match postgres_dsn() {
        None => (ShadowSpec::Pgmem, None),
        Some(_) => {
            let db = TestDb::new();
            (ShadowSpec::Postgres(db.conninfo.clone()), Some(db))
        }
    }
}

const USERS: &str = "CREATE TABLE users (id int PRIMARY KEY);\n";
const ORDERS: &str = "CREATE TABLE orders (\n    id int PRIMARY KEY,\n    user_id int REFERENCES users\n);\n";
const NOTE: &str = "ALTER TABLE orders ADD COLUMN note text;\n";

fn three() -> Dir {
    let d = Dir::new(&[
        ("20261125093000_create_users.sql", USERS),
        ("20261126101500_add_orders.sql", ORDERS),
        ("20261127080000_order_note.sql", NOTE),
    ]);
    d.hash();
    d
}

fn checksum(r: pgmig::Result<()>) -> ChecksumError {
    match r {
        Err(Error::Checksum(e)) => e,
        Err(e) => panic!("expected a checksum error, got {e}"),
        Ok(()) => panic!("expected a checksum error"),
    }
}

#[test]
fn hash_writes_the_sum_file_and_validate_accepts_it() {
    let d = three();
    let text = std::fs::read_to_string(d.0.join(SUM_FILE)).expect("sum file");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert!(lines[0].starts_with("h1:"));
    assert!(lines[2].starts_with("20261126101500_add_orders.sql h1:"), "{text}");
    d.validate().expect("valid");
    let (_, written) = pgmig::migrate_hash(&d.0).expect("rehash");
    assert!(!written);
}

#[test]
fn missing_sum_file_is_reported() {
    let d = Dir::new(&[("1_a.sql", USERS)]);
    let e = checksum(d.validate());
    assert!(matches!(e, ChecksumError::MissingSum { .. }), "{e}");
    assert!(e.to_string().contains("run `pgmig migrate hash`"), "{e}");
}

#[test]
fn edited_file_is_reported() {
    let d = three();
    d.write("20261126101500_add_orders.sql", &ORDERS.replace("id int", "id bigint"));
    let e = checksum(d.validate());
    assert!(matches!(&e, ChecksumError::Edited { file } if file == &d.0.join("20261126101500_add_orders.sql")), "{e}");
}

#[test]
fn inserted_file_is_reported() {
    let d = three();
    d.write("20261126000000_add_email.sql", "ALTER TABLE users ADD COLUMN email text;\n");
    let e = checksum(d.validate());
    let ChecksumError::Inserted { file, before } = &e else { panic!("expected an insertion, got {e}") };
    assert_eq!((file, before.as_str()), (&d.0.join("20261126000000_add_email.sql"), "20261126101500_add_orders.sql"));
}

#[test]
fn deleted_file_is_reported() {
    let d = three();
    std::fs::remove_file(d.0.join("20261126101500_add_orders.sql")).expect("delete");
    let e = checksum(d.validate());
    assert!(matches!(&e, ChecksumError::Deleted { file } if file.ends_with("20261126101500_add_orders.sql")), "{e}");
}

#[test]
fn appended_file_without_rehash_is_reported() {
    let d = three();
    d.write("20261128000000_more.sql", "CREATE TABLE more (id int);\n");
    let e = checksum(d.validate());
    assert!(matches!(&e, ChecksumError::NotHashed { file } if file.ends_with("20261128000000_more.sql")), "{e}");
    d.hash();
    d.validate().expect("valid after rehash");
}

#[test]
fn hand_edited_sum_file_is_reported() {
    let d = three();
    let path = d.0.join(SUM_FILE);
    let text = std::fs::read_to_string(&path).expect("sum file");
    // Drop the last line, as a hand-resolved merge might.
    let kept: String = text.lines().take(3).map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, kept).expect("edit sum");
    let e = checksum(d.validate());
    assert!(matches!(e, ChecksumError::SumEdited { .. }), "{e}");
}

#[test]
fn malformed_names_are_reported() {
    let d = Dir::new(&[("1_a.sql", USERS), ("orders.sql", ORDERS)]);
    let err = pgmig::migrate_hash(&d.0).expect_err("bad name");
    assert!(matches!(&err, Error::Migrate(e) if e.to_string().contains("orders.sql")), "{err}");
}

#[test]
fn replay_runs_every_migration_in_order() {
    let d = three();
    let (spec, _db) = shadow();
    pgmig::migrate_validate(&d.0, Some(&spec)).expect("replays");
}

#[test]
fn failing_migration_is_reported_with_file_and_line() {
    let d = Dir::new(&[
        ("20261125093000_create_users.sql", USERS),
        ("20261126101500_add_orders.sql", "-- orders\n\nCREATE TABLE orders (\n    id int,\n    user_id nosuchtype\n);\n"),
        ("20261127080000_order_note.sql", NOTE),
    ]);
    d.hash();
    let (spec, _db) = shadow();
    let err = pgmig::migrate_validate(&d.0, Some(&spec)).expect_err("unknown type");
    let Error::Load { path, pos, .. } = &err else { panic!("expected a load error, got {err}") };
    assert_eq!(path, &d.0.join("20261126101500_add_orders.sql"));
    // pgmem reports no error position (kaeawc/pgmem#166): the statement start stands in.
    let want = if postgres_dsn().is_some() { (5, 13) } else { (3, 1) };
    assert_eq!((pos.line, pos.col), want, "{err}");
    assert!(err.to_string().ends_with("type \"nosuchtype\" does not exist (SQLSTATE 42704)"), "{err}");
}

/// Replay depends on order: a later migration sees the earlier ones' objects.
#[test]
fn replay_uses_version_order_not_name_order() {
    let d = Dir::new(&[("9_users.sql", USERS), ("10_orders.sql", ORDERS)]);
    d.hash();
    let (spec, _db) = shadow();
    pgmig::migrate_validate(&d.0, Some(&spec)).expect("9 runs before 10");
}

#[test]
fn checksums_are_checked_before_replay() {
    let d = three();
    d.write("20261127080000_order_note.sql", "this is not sql;\n");
    let (spec, _db) = shadow();
    let e = checksum(pgmig::migrate_validate(&d.0, Some(&spec)));
    assert!(matches!(e, ChecksumError::Edited { .. }), "{e}");
}

#[test]
fn api_reports_diagnostics() {
    let d = three();
    let (spec, _db) = shadow();
    let ok = api::migrate_validate(&api::MigrateValidateRequest { dir: d.0.clone(), shadow: Some(spec) }).expect("checked");
    assert!(ok.is_valid() && ok.replayed, "{ok:?}");
    d.write("20261125093000_create_users.sql", "CREATE TABLE users (id bigint PRIMARY KEY);\n");
    let res = api::migrate_validate(&api::MigrateValidateRequest { dir: d.0.clone(), shadow: None }).expect("checked");
    assert!(!res.replayed);
    let [diag] = res.diagnostics.as_slice() else { panic!("one diagnostic: {res:?}") };
    assert_eq!(diag.code, ErrorCode::FileEdited);
    assert_eq!(diag.file.as_deref(), Some(d.0.join("20261125093000_create_users.sql").as_path()));
    let hashed = api::migrate_hash(&api::MigrateHashRequest { dir: d.0.clone() }).expect("hash");
    assert!(hashed.written);
    assert_eq!(hashed.sum_file, d.0.join(SUM_FILE));
    assert_eq!(hashed.files.len(), 3);
    assert!(hashed.dir_hash.starts_with("h1:") && hashed.files[0].hash.starts_with("h1:"));
    let missing = api::migrate_validate(&api::MigrateValidateRequest { dir: Path::new(&d.0).join("nope"), shadow: None }).expect("checked");
    assert_eq!(missing.diagnostics[0].code, ErrorCode::Io);
}

#[test]
fn an_import_in_a_migration_is_rejected_even_without_a_shadow() {
    for (text, line) in [
        ("-- pgmig:import ../schema\nCREATE TABLE a (id int);\n", 1),
        ("CREATE TABLE a (id int);\n-- pgmig:import ../schema\nCREATE TABLE b (id int);\n", 2),
    ] {
        let d = Dir::new(&[("20261125093000_a.sql", text)]);
        d.hash();
        let err = d.validate().expect_err("import");
        assert!(matches!(err, Error::MigrationImport { .. }), "{err}");
        let api = api::ApiError::from(&err);
        assert_eq!(api.code, ErrorCode::MigrationImport);
        assert_eq!(api.line, Some(line));
        assert!(api.file.as_deref().is_some_and(|f| f.ends_with("20261125093000_a.sql")));
    }
}
