//! `migrate diff` and `migrate new`, and a migration directory as a source.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, ErrorCode};
use pgmig::{ChecksumError, Error, Options, SUM_FILE, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory, removed on drop. Migrations go in `migrations/` under it (created by the
/// commands), desired-state files next to it.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig-migrate-diff-it-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tmp(path)
    }

    fn dir(&self) -> PathBuf {
        self.0.join("migrations")
    }

    /// Write the desired state.
    fn desired(&self, sql: &str) -> Source {
        let path = self.0.join("schema.sql");
        std::fs::write(&path, sql).expect("write schema.sql");
        Source::Sql(path)
    }

    fn migration(&self, name: &str, sql: &str) {
        std::fs::create_dir_all(self.dir()).expect("create migrations");
        std::fs::write(self.dir().join(name), sql).expect("write migration");
    }

    /// The migration file names, in name order.
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
        std::fs::read_to_string(self.dir().join(name)).expect("read")
    }
}

impl Drop for Tmp {
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

fn diff(t: &Tmp, name: &str, to: &Source, spec: &ShadowSpec) -> pgmig::Result<pgmig::MigrateDiff> {
    pgmig::migrate_diff(&t.dir(), name, to, spec, &Options::default())
}

/// Replaying the directory gives the desired state: nothing to change either way, and the same
/// recreating SQL.
fn assert_replay_matches(t: &Tmp, to: &Source, spec: &ShadowSpec) {
    let dir = Source::Migrations(t.dir());
    let opts = Options::default();
    let forward = pgmig::schema_diff(&dir, to, spec, &opts).expect("diff");
    assert!(forward.is_empty(), "left to change:\n{}", forward.to_sql());
    let back = pgmig::schema_diff(to, &dir, spec, &opts).expect("diff back");
    assert!(back.is_empty(), "left to change back:\n{}", back.to_sql());
    let a = pgmig::schema_inspect(&dir, spec, &opts).expect("inspect dir");
    let b = pgmig::schema_inspect(to, spec, &opts).expect("inspect desired");
    assert_eq!(a.to_sql(), b.to_sql());
}

const SCHEMA: &str = "CREATE TABLE users (id int PRIMARY KEY, email text NOT NULL);\n\
                      CREATE TABLE orders (id int PRIMARY KEY, user_id int REFERENCES users);\n";

#[test]
fn diff_writes_the_next_migration_then_reports_no_changes() {
    let t = Tmp::new();
    let (spec, _db) = shadow();
    let to = t.desired(SCHEMA);

    // The directory does not exist yet: an empty state, created when the file is written.
    let res = diff(&t, "Create Tables", &to, &spec).expect("diff");
    let written = res.written.expect("a file");
    let name = written.path.file_name().and_then(|n| n.to_str()).expect("name").to_owned();
    assert_eq!(name, format!("{}_create_tables.sql", written.version));
    assert_eq!(written.version.len(), 14, "{}", written.version);
    assert!(written.version.bytes().all(|b| b.is_ascii_digit()));
    assert_eq!(t.read(&name), res.plan.to_sql());
    assert!(t.read(&name).contains("CREATE TABLE public.users"), "{}", t.read(&name));
    pgmig::migrate_validate(&t.dir(), Some(&spec)).expect("valid and replays");
    assert_replay_matches(&t, &to, &spec);

    // A second run finds nothing to do and writes nothing.
    let sum = std::fs::read(t.dir().join(SUM_FILE)).expect("sum");
    let again = diff(&t, "again", &to, &spec).expect("diff again");
    assert!(again.written.is_none() && again.plan.is_empty() && again.changes.is_empty(), "{again:?}");
    assert_eq!(t.files(), std::slice::from_ref(&name));
    assert_eq!(std::fs::read(t.dir().join(SUM_FILE)).expect("sum"), sum);

    // A change to the desired state is the next file.
    let to = t.desired(&format!("{SCHEMA}ALTER TABLE orders ADD COLUMN note text;\nCREATE INDEX orders_user ON orders (user_id);\n"));
    let next = diff(&t, "order_note", &to, &spec).expect("diff").written.expect("a second file");
    assert!(pgmig_version_gt(&next.version, &written.version), "{} after {}", next.version, written.version);
    assert_eq!(t.files().len(), 2);
    pgmig::migrate_validate(&t.dir(), Some(&spec)).expect("valid and replays");
    assert_replay_matches(&t, &to, &spec);
    assert!(diff(&t, "x", &to, &spec).expect("diff").written.is_none());
}

fn pgmig_version_gt(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    (a.len(), a) > (b.len(), b)
}

/// The commit point the planner puts after new enum values is kept in the file, and the file
/// replays: migrations run statement by statement.
#[test]
fn commit_points_are_written_and_replay() {
    let t = Tmp::new();
    let (spec, _db) = shadow();
    let to = t.desired("CREATE TYPE mood AS ENUM ('sad', 'happy');\nCREATE TABLE people (id int, mood mood DEFAULT 'sad');\n");
    diff(&t, "mood", &to, &spec).expect("diff").written.expect("first");
    let to = t.desired("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');\nCREATE TABLE people (id int, mood mood DEFAULT 'ok');\n");
    let w = diff(&t, "ok mood", &to, &spec).expect("diff").written.expect("second");
    let name = w.path.file_name().and_then(|n| n.to_str()).expect("name").to_owned();
    assert!(t.read(&name).contains("\n-- pgmig:commit\n"), "{}", t.read(&name));
    pgmig::migrate_validate(&t.dir(), Some(&spec)).expect("replays");
    assert_replay_matches(&t, &to, &spec);
}

#[test]
fn versions_follow_files_from_the_future() {
    let t = Tmp::new();
    let (spec, _db) = shadow();
    t.migration("99990101000000_users.sql", "CREATE TABLE users (id int PRIMARY KEY);\n");
    pgmig::migrate_hash(&t.dir()).expect("hash");
    let to = t.desired("CREATE TABLE users (id int PRIMARY KEY, name text);\n");
    let w = diff(&t, "name", &to, &spec).expect("diff").written.expect("a file");
    assert_eq!(w.version, "99990101000001");
    assert_eq!(t.files(), ["99990101000000_users.sql", "99990101000001_name.sql"]);
    // `migrate new` right after, in the same second or not, still sorts last.
    let n = pgmig::migrate_new(&t.dir(), "fill in").expect("new");
    assert_eq!(n.version, "99990101000002");
    pgmig::migrate_validate(&t.dir(), Some(&spec)).expect("valid");
}

#[test]
fn new_writes_an_empty_migration_that_validates() {
    let t = Tmp::new();
    let (spec, _db) = shadow();
    let a = pgmig::migrate_new(&t.dir(), "backfill emails").expect("new");
    assert!(a.path.ends_with(format!("{}_backfill_emails.sql", a.version)), "{}", a.path.display());
    assert_eq!(std::fs::read(&a.path).expect("read"), b"");
    pgmig::migrate_validate(&t.dir(), Some(&spec)).expect("valid");
    let b = pgmig::migrate_new(&t.dir(), "second").expect("new");
    assert!(pgmig_version_gt(&b.version, &a.version), "{} after {}", b.version, a.version);
    pgmig::migrate_validate(&t.dir(), None).expect("rehashed");
    // Filled in by hand and rehashed, it replays.
    std::fs::write(&b.path, "CREATE TABLE t (id int);\n").expect("fill in");
    pgmig::migrate_hash(&t.dir()).expect("hash");
    pgmig::migrate_validate(&t.dir(), Some(&spec)).expect("replays");
}

#[test]
fn a_migration_directory_is_a_source() {
    let t = Tmp::new();
    let (spec, _db) = shadow();
    t.migration("1_users.sql", "CREATE TABLE users (id int PRIMARY KEY);\n");
    t.migration("2_email.sql", "ALTER TABLE users ADD COLUMN email text;\n");
    pgmig::migrate_hash(&t.dir()).expect("hash");
    let dir = t.dir().to_string_lossy().into_owned();
    assert_eq!(Source::parse(&dir), Source::Migrations(t.dir()));

    let to = t.desired("CREATE TABLE users (id int PRIMARY KEY, email text, name text);\n");
    let plan = pgmig::schema_diff(&Source::parse(&dir), &to, &spec, &Options::default()).expect("diff");
    assert_eq!(plan.statements, ["ALTER TABLE public.users ADD COLUMN name text"]);
    let back = pgmig::schema_diff(&to, &Source::parse(&dir), &spec, &Options::default()).expect("diff");
    assert_eq!(back.statements, ["ALTER TABLE public.users DROP COLUMN name"]);
    pgmig::schema_validate(&Source::Migrations(t.dir()), &spec).expect("loads");

    // Loading it checks the sum file.
    t.migration("2_email.sql", "ALTER TABLE users ADD COLUMN email varchar;\n");
    let err = pgmig::schema_inspect(&Source::Migrations(t.dir()), &spec, &Options::default()).expect_err("edited");
    assert!(matches!(err, Error::Checksum(ChecksumError::Edited { .. })), "{err}");
}

#[test]
fn diff_refuses_a_directory_that_does_not_match_its_sum_file() {
    let t = Tmp::new();
    let (spec, _db) = shadow();
    let to = t.desired(SCHEMA);
    t.migration("1_users.sql", "CREATE TABLE users (id int PRIMARY KEY);\n");
    // No sum file.
    let err = diff(&t, "x", &to, &spec).expect_err("no sum");
    assert!(matches!(err, Error::Checksum(ChecksumError::MissingSum { .. })), "{err}");
    pgmig::migrate_hash(&t.dir()).expect("hash");
    t.migration("1_users.sql", "CREATE TABLE users (id bigint PRIMARY KEY);\n");
    let err = diff(&t, "x", &to, &spec).expect_err("edited");
    assert!(matches!(err, Error::Checksum(ChecksumError::Edited { .. })), "{err}");
    let err = pgmig::migrate_new(&t.dir(), "x").expect_err("edited");
    assert!(matches!(err, Error::Checksum(ChecksumError::Edited { .. })), "{err}");
    assert_eq!(t.files(), ["1_users.sql"]);
}

#[test]
fn names_without_letters_or_digits_are_refused() {
    let t = Tmp::new();
    let err = pgmig::migrate_new(&t.dir(), " -- ").expect_err("empty name");
    assert!(matches!(err, Error::MigrationDescription { .. }), "{err}");
    assert_eq!(api::ApiError::from(&err).code, ErrorCode::Usage);
    assert!(!Path::new(&t.dir()).exists());
}

#[test]
fn api_reports_the_file_written() {
    let t = Tmp::new();
    let (spec, _db) = shadow();
    let to = t.desired(SCHEMA);
    let req = api::MigrateDiffRequest { dir: t.dir(), name: "init".into(), to, shadow: spec, options: Options::default(), editor: None };
    let res = api::migrate_diff(&req).expect("diff");
    let file = res.file.clone().expect("written");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), res.sql);
    assert!(res.version.is_some() && !res.changes.is_empty());
    let res = api::migrate_diff(&req).expect("diff again");
    assert_eq!((res.file, res.version, res.sql.as_str()), (None, None, ""));
    let new = api::migrate_new(&api::MigrateNewRequest { dir: t.dir(), name: "next".into() }).expect("new");
    assert!(new.file.ends_with(format!("{}_next.sql", new.version)));
}
