//! `drift check`: a live database against the migration directory at its applied version, against
//! desired-state SQL and against another database, on the test backend.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, DriftCheckRequest};
use pgmig::{
    ApplyOptions, ChangeKind, DriftCheck, Expected, Filter, IssueKind, Options, ShadowSpec, Source, TxMode, drift_check, migrate_apply,
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

/// A temp directory of files, removed on drop; `hash` rehashes it as a migration directory.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)], hash: bool) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig_drift_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        let d = Dir(path);
        for (name, sql) in files {
            d.write(name, sql, hash);
        }
        d
    }

    fn write(&self, name: &str, sql: &str, hash: bool) {
        std::fs::write(self.0.join(name), sql).expect("write file");
        if hash {
            pgmig::migrate_hash(&self.0).expect("hash");
        }
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

const A: &str = "1_users.sql";
const B: &str = "2_posts.sql";
const USERS: &str = "CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL, active boolean DEFAULT true);\n\
                     CREATE INDEX users_name ON users (name);\n";
const POSTS: &str = "CREATE TABLE posts (id int PRIMARY KEY, title text);\n";

fn applied(db: &TestDb, m: &Dir, shadow: &ShadowSpec, opts: &Options) -> DriftCheck {
    drift_check(&db.conninfo, &Expected::Applied(m.path().to_owned()), shadow, opts).expect("drift check")
}

fn issue_kinds(d: &DriftCheck) -> Vec<IssueKind> {
    d.history.as_ref().expect("history").issues.iter().map(|i| i.kind).collect()
}

/// Done when: no drift after `migrate apply`.
#[test]
fn applied_directory_has_no_drift() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    let m = Dir::new(&[(A, USERS), (B, POSTS)], true);
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions::default()).expect("apply");
    let d = applied(&db, &m, &shadow, &Options::default());
    assert!(!d.drifted && d.changes.is_empty() && d.plan.is_empty() && d.sql.is_empty(), "{d:?}");
    let h = d.history.expect("history");
    assert_eq!((h.replayed, h.pending, h.current.as_deref()), (vec![A.to_owned(), B.to_owned()], vec![], Some("2")));
    assert!(h.issues.is_empty() && h.partial.is_empty(), "{:?}", h.issues);
}

/// Done when: drift introduced by hand is reported, and its plan brings the database back.
#[test]
fn drift_by_hand_is_reported_and_its_plan_brings_the_database_back() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    let m = Dir::new(&[(A, USERS), (B, POSTS)], true);
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions::default()).expect("apply");
    db.connect()
        .batch("ALTER TABLE users ADD COLUMN email text; DROP INDEX users_name; ALTER TABLE users ALTER COLUMN active SET DEFAULT false")
        .expect("drift by hand");
    let d = applied(&db, &m, &shadow, &Options::default());
    assert!(d.drifted, "{d:?}");
    for wanted in ["DROP COLUMN email", "CREATE INDEX users_name", "SET DEFAULT true"] {
        assert!(d.sql.contains(wanted), "{wanted:?} in:\n{}", d.sql);
    }
    assert!(d.sql.contains("public.users"), "schema-qualified: {}", d.sql);
    assert!(d.history.as_ref().is_some_and(|h| h.issues.is_empty()), "{:?}", d.history);
    pgmig::apply(&db.conninfo, &d.plan).expect("apply the drift plan");
    let again = applied(&db, &m, &shadow, &Options::default());
    assert!(!again.drifted && again.changes.is_empty(), "{again:?}");
}

#[test]
fn pending_files_are_listed_and_are_not_drift() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    let m = Dir::new(&[(A, USERS)], true);
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions::default()).expect("apply");
    m.write(B, POSTS, true);
    let d = applied(&db, &m, &shadow, &Options::default());
    assert!(!d.drifted && d.changes.is_empty(), "{d:?}");
    let h = d.history.expect("history");
    assert_eq!((h.replayed, h.pending), (vec![A.to_owned()], vec![B.to_owned()]));

    // Nothing applied at all: the expected state is an empty database, so the objects are drift.
    let fresh = TestDb::new();
    fresh.connect().batch("CREATE TABLE stray (id int)").expect("stray");
    let d = applied(&fresh, &m, &shadow, &Options::default());
    assert!(d.drifted && d.sql.contains("DROP TABLE public.stray"), "{d:?}");
    let h = d.history.expect("history");
    assert_eq!((h.revisions_table, h.replayed.len(), h.pending.len()), (None, 0, 2));
}

#[test]
fn an_applied_file_edited_afterwards_is_a_history_problem() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    let m = Dir::new(&[(A, USERS)], true);
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions::default()).expect("apply");
    m.write(A, &format!("{USERS}-- a comment\n"), true);
    let d = applied(&db, &m, &shadow, &Options::default());
    assert!(d.drifted && d.changes.is_empty(), "the schema agrees, the history does not: {d:?}");
    assert_eq!(issue_kinds(&d), [IssueKind::Changed]);
    let issue = &d.history.as_ref().expect("history").issues[0];
    assert!(issue.hint.contains("pgmig migrate set 1 --force"), "{}", issue.hint);
}

/// DRF-3: after a partial failure only the statements recorded as applied are expected, and the
/// failure is reported with how to recover.
#[test]
fn a_failed_file_is_replayed_as_far_as_it_was_applied() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    let partial = "CREATE TABLE x (id int);\nCREATE TABLE y (id int);\nINSERT INTO nope VALUES (1);\n";
    let m = Dir::new(&[(A, USERS), (B, partial)], true);
    let opts = ApplyOptions { tx_mode: TxMode::None, ..ApplyOptions::default() };
    let _ = migrate_apply(&db.conninfo, m.path(), &opts).expect_err("the third statement fails");
    let d = applied(&db, &m, &shadow, &Options::default());
    assert!(d.drifted && d.changes.is_empty(), "x and y are expected: {d:?}");
    assert_eq!(issue_kinds(&d), [IssueKind::Failed]);
    let h = d.history.as_ref().expect("history");
    assert_eq!((h.replayed.clone(), h.partial.clone(), h.pending.clone()), (vec![A.into(), B.into()], vec![B.into()], vec![B.into()]));
    assert!(h.issues[0].hint.contains("pgmig migrate apply"), "{}", h.issues[0].hint);

    // Undone by hand: now the schema drifted too.
    db.connect().batch("DROP TABLE y").expect("drop");
    let d = applied(&db, &m, &shadow, &Options::default());
    assert!(d.sql.contains("CREATE TABLE public.y"), "{}", d.sql);
}

#[test]
fn compares_with_desired_sql_and_with_another_database() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    db.connect().batch(USERS).expect("users");
    let desired = Dir::new(&[("schema.sql", USERS)], false);
    let to_sql = Expected::Source(Source::Sql(desired.path().to_owned()));
    let d = drift_check(&db.conninfo, &to_sql, &shadow, &Options::default()).expect("check");
    assert!(!d.drifted && d.history.is_none(), "{d:?}");

    // DRF-2: two live databases.
    let other = TestDb::new();
    other.connect().batch(&format!("{USERS}{POSTS}")).expect("other");
    let to_db = Expected::Source(Source::Database(other.conninfo.clone()));
    let d = drift_check(&db.conninfo, &to_db, &shadow, &Options::default()).expect("check");
    assert!(d.drifted && d.sql.contains("CREATE TABLE public.posts"), "{d:?}");
    pgmig::apply(&db.conninfo, &d.plan).expect("apply");
    let d = drift_check(&db.conninfo, &to_db, &shadow, &Options::default()).expect("check");
    assert!(!d.drifted, "{d:?}");
    let d = drift_check(&db.conninfo, &to_sql, &shadow, &Options::default()).expect("check");
    assert!(d.drifted && d.sql.contains("DROP TABLE public.posts"), "{d:?}");
}

#[test]
fn filters_schema_mode_and_skip_are_respected() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    let m = Dir::new(&[(A, "CREATE SCHEMA app;\nCREATE TABLE app.t (id int);\n")], true);
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions::default()).expect("apply");
    db.connect().batch("CREATE TABLE public.stray (id int); ALTER TABLE app.t ADD COLUMN extra int").expect("drift");

    let schema = Options { schema: Some("app".into()), ..Options::default() };
    let d = applied(&db, &m, &shadow, &schema);
    assert!(d.drifted && d.changes.len() == 1 && d.sql.contains("DROP COLUMN extra"), "{d:?}");

    let filtered = Options { filter: Filter::parse(&["app"], &["app.t"]).expect("patterns"), ..Options::default() };
    let d = applied(&db, &m, &shadow, &filtered);
    assert!(!d.drifted, "{d:?}");

    // Skipped changes are reported, and are not drift.
    let skip = Options { skip: vec![ChangeKind::DropColumn], ..schema };
    let d = applied(&db, &m, &shadow, &skip);
    assert!(!d.drifted && d.changes.is_empty() && d.skipped.len() == 1, "{d:?}");
}

#[test]
fn api_requests_and_responses_round_trip() {
    let (shadow, _keep) = shadow_spec();
    let db = TestDb::new();
    let m = Dir::new(&[(A, USERS)], true);
    migrate_apply(&db.conninfo, m.path(), &ApplyOptions::default()).expect("apply");
    db.connect().batch("ALTER TABLE users ADD COLUMN email text").expect("drift");
    let req = DriftCheckRequest {
        target: db.conninfo.clone(),
        dir: Some(m.path().to_owned()),
        to: None,
        shadow: shadow.clone(),
        options: Options::default(),
    };
    let json = serde_json::to_string(&req).expect("serialize");
    assert!(!json.contains("\"to\""), "{json}");
    assert_eq!(serde_json::from_str::<DriftCheckRequest>(&json).expect("request"), req);
    let res = api::drift_check(&req).expect("drift check");
    assert!(res.drifted && res.history.is_some(), "{res:?}");
    let json = serde_json::to_value(&res).expect("serialize");
    for key in ["drifted", "changes", "skipped", "plan", "sql", "unsupported", "history"] {
        assert!(json.get(key).is_some(), "{key} in {json}");
    }
    assert_eq!(serde_json::from_value::<pgmig::api::DriftCheckResponse>(json).expect("response"), res);

    let both = DriftCheckRequest { to: Some(Source::Sql("s.sql".into())), ..req.clone() };
    let neither = DriftCheckRequest { dir: None, ..req };
    for r in [both, neither] {
        let err = api::drift_check(&r).expect_err("needs exactly one");
        assert_eq!(err.code, api::ErrorCode::Usage, "{err:?}");
    }
    assert!(serde_json::from_str::<DriftCheckRequest>(r#"{"target":"t","dir":"m","extra":1}"#).is_err());
}
