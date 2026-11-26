//! The revisions table: created on first use, read back as written, hand edits caught, and never
//! part of the managed schema.

use pgmig::revisions::{self, DEFAULT_SCHEMA, Error, FORMAT, Kind, Revision, Table};
use pgmig::{Options, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

fn count(conn: &mut pgmig::Conn, sql: &str) -> String {
    conn.query(sql).expect("query").remove(0).remove(0).expect("not null")
}

fn tables_named(conn: &mut pgmig::Conn, schema: &str) -> String {
    count(
        conn,
        &format!(
            "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = '{schema}' AND c.relname = 'pgmig_revisions'"
        ),
    )
}

#[test]
fn ensure_creates_the_table_once() {
    let db = TestDb::new();
    let mut conn = db.connect();
    assert_eq!(Table::open(&mut conn, DEFAULT_SCHEMA).expect("open"), None);
    let t = Table::ensure(&mut conn, DEFAULT_SCHEMA).expect("ensure");
    assert_eq!(t.name(), "pgmig.pgmig_revisions");
    assert_eq!(tables_named(&mut conn, "pgmig"), "1");
    let comment =
        count(&mut conn, "SELECT obj_description(c.oid, 'pg_class') FROM pg_catalog.pg_class c WHERE c.relname = 'pgmig_revisions'");
    assert_eq!(comment, format!("pgmig revisions table, format {FORMAT}"));
    assert!(t.read(&mut conn).expect("read").is_empty());

    // Again: a no-op that keeps the rows.
    t.insert(&mut conn, &Revision::start("1", "a", Kind::Normal, 0, "h")).expect("insert");
    let again = Table::ensure(&mut conn, DEFAULT_SCHEMA).expect("ensure again");
    assert_eq!(again, t);
    assert_eq!(again.read(&mut conn).expect("read").len(), 1);
    assert_eq!(Table::open(&mut conn, DEFAULT_SCHEMA).expect("open"), Some(t));
}

#[test]
fn ensure_uses_an_existing_schema_and_quotes_its_name() {
    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch("CREATE SCHEMA \"Ops\"").expect("schema");
    let t = Table::ensure(&mut conn, "Ops").expect("ensure");
    assert_eq!(t.name(), "\"Ops\".pgmig_revisions");
    assert_eq!(tables_named(&mut conn, "Ops"), "1");
    t.insert(&mut conn, &Revision::start("1", "a", Kind::Normal, 0, "h")).expect("insert");
    assert_eq!(t.read(&mut conn).expect("read").len(), 1);
}

#[test]
fn rows_round_trip_through_start_progress_failure_and_finish() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let t = Table::ensure(&mut conn, DEFAULT_SCHEMA).expect("ensure");

    let mut rev = Revision::start("20261126103000", "it's \"quoted\"\n\\ and more", Kind::Normal, 3, "abc+/=");
    t.insert(&mut conn, &rev).expect("insert");
    let stored = t.read(&mut conn).expect("read");
    assert_eq!(stored.len(), 1);
    assert!(stored[0].executed_at.as_deref().is_some_and(|s| s.starts_with("20")), "{:?}", stored[0].executed_at);
    rev.executed_at.clone_from(&stored[0].executed_at);
    assert_eq!(stored[0], rev);
    assert_eq!(rev.pgmig_version, env!("CARGO_PKG_VERSION"));

    rev.progress("s1+/=");
    t.update(&mut conn, &rev).expect("progress");
    rev.fail("relation \"nope\" does not exist (SQLSTATE 42P01)", "SELECT * FROM nope WHERE a = 'x'");
    rev.execution_ms = 12;
    t.update(&mut conn, &rev).expect("error");
    assert_eq!(t.read(&mut conn).expect("read"), [rev.clone()]);

    rev.progress("s2");
    rev.progress("s3");
    rev.execution_ms = 40;
    t.update(&mut conn, &rev).expect("finish");
    let got = t.read(&mut conn).expect("read");
    assert_eq!(got, [rev.clone()]);
    assert!(got[0].is_complete());
    assert_eq!(got[0].partial_hashes, ["s1+/=", "s2", "s3"]);

    // A baseline with a given start time; rows come back in version order.
    let mut base = Revision::start("9", "baseline", Kind::Baseline, 0, "h");
    base.executed_at = Some("2026-01-02 03:04:05+00".into());
    t.insert(&mut conn, &base).expect("insert baseline");
    let got = t.read(&mut conn).expect("read");
    assert_eq!(got.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(), ["9", "20261126103000"]);
    assert_eq!(got[0].kind, Kind::Baseline);

    // One row per version.
    let dup = t.insert(&mut conn, &base).expect_err("duplicate");
    assert!(matches!(&dup, Error::Conn(e) if e.code.as_deref() == Some("23505")), "{dup}");
    let missing = t.update(&mut conn, &Revision::start("77", "x", Kind::Normal, 0, "h")).expect_err("missing");
    assert!(matches!(&missing, Error::NotFound { version, .. } if version == "77"), "{missing}");

    assert!(t.delete(&mut conn, "9").expect("delete"));
    assert!(!t.delete(&mut conn, "9").expect("delete again"));
    assert_eq!(t.read(&mut conn).expect("read"), [rev]);
}

#[test]
fn a_hand_edited_row_is_reported_by_version() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let t = Table::ensure(&mut conn, DEFAULT_SCHEMA).expect("ensure");
    let mut a = Revision::start("1", "a", Kind::Normal, 2, "h1");
    a.progress("x");
    t.insert(&mut conn, &a).expect("insert");
    t.insert(&mut conn, &Revision::start("2", "b", Kind::Normal, 1, "h2")).expect("insert");
    t.read(&mut conn).expect("intact");

    // Marking a file applied by hand.
    conn.batch("UPDATE pgmig.pgmig_revisions SET applied = 1, partial_hashes = ARRAY['y'] WHERE version = '2'").expect("edit");
    let err = t.read(&mut conn).expect_err("edited");
    assert!(matches!(&err, Error::Edited { version, .. } if version == "2"), "{err}");
    assert!(err.to_string().contains("version 2 was edited by hand"), "{err}");

    // Rewriting the row through the API makes it valid again; a changed start time is not an edit.
    t.update(&mut conn, &Revision::start("2", "b", Kind::Normal, 1, "h2")).expect("rewrite");
    conn.batch("UPDATE pgmig.pgmig_revisions SET executed_at = executed_at - interval '1 hour'").expect("move time");
    t.read(&mut conn).expect("intact again");

    // A row inserted by hand has no valid hash.
    conn.batch("INSERT INTO pgmig.pgmig_revisions VALUES ('3', 'c', 0, 0, 0, now(), 0, NULL, NULL, 'h3', '{}', '0.1.0', 'forged')")
        .expect("insert by hand");
    let err = t.read(&mut conn).expect_err("forged");
    assert!(matches!(&err, Error::Edited { version, .. } if version == "3"), "{err}");

    // An unknown kind is reported as such.
    conn.batch("DELETE FROM pgmig.pgmig_revisions WHERE version = '3'; UPDATE pgmig.pgmig_revisions SET kind = 7 WHERE version = '1'")
        .expect("bad kind");
    let err = t.read(&mut conn).expect_err("kind");
    assert!(matches!(&err, Error::Malformed { version, column: "kind", .. } if version == "1"), "{err}");
}

#[test]
fn a_newer_or_foreign_table_is_refused() {
    let db = TestDb::new();
    let mut conn = db.connect();
    Table::ensure(&mut conn, DEFAULT_SCHEMA).expect("ensure");
    conn.batch(&format!("COMMENT ON TABLE pgmig.pgmig_revisions IS 'pgmig revisions table, format {}'", FORMAT + 1)).expect("comment");
    for err in [Table::ensure(&mut conn, DEFAULT_SCHEMA).expect_err("newer"), Table::open(&mut conn, DEFAULT_SCHEMA).expect_err("newer")] {
        assert!(matches!(err, Error::NewerFormat { found, supported: FORMAT, .. } if found == FORMAT + 1), "{err}");
    }

    conn.batch("CREATE SCHEMA other; CREATE TABLE other.pgmig_revisions (id int)").expect("foreign table");
    let err = Table::ensure(&mut conn, "other").expect_err("foreign");
    assert!(matches!(&err, Error::NotRevisionsTable { table } if table == "other.pgmig_revisions"), "{err}");
}

/// A target holding the revisions table, and desired-state SQL that knows nothing of it.
struct Target {
    db: TestDb,
    sql: std::path::PathBuf,
    _shadow: Option<TestDb>,
    shadow: ShadowSpec,
}

impl Target {
    fn new(revisions_schema: &str) -> Target {
        let db = TestDb::new();
        let mut conn = db.connect();
        let t = Table::ensure(&mut conn, revisions_schema).expect("ensure");
        t.insert(&mut conn, &Revision::start("1", "a", Kind::Normal, 0, "h")).expect("insert");
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sql = std::env::temp_dir().join(format!("pgmig-revisions-it-{}-{n}.sql", std::process::id()));
        std::fs::write(&sql, "CREATE TABLE users (id int PRIMARY KEY);\n").expect("write sql");
        let (shadow, keep) = match postgres_dsn() {
            None => (ShadowSpec::Pgmem, None),
            Some(_) => {
                let s = TestDb::new();
                (ShadowSpec::Postgres(s.conninfo.clone()), Some(s))
            }
        };
        Target { db, sql, _shadow: keep, shadow }
    }

    fn plan(&self, opts: &Options) -> Vec<String> {
        pgmig::schema_plan_apply(&self.db.conninfo, &Source::Sql(self.sql.clone()), &self.shadow, opts).expect("plan").statements
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sql);
    }
}

#[test]
fn schema_apply_and_diff_leave_the_revisions_schema_alone() {
    let target = Target::new(DEFAULT_SCHEMA);
    let opts = Options::default();
    let plan = target.plan(&opts);
    assert_eq!(plan, ["CREATE TABLE public.users (\n    id integer NOT NULL,\n    CONSTRAINT users_pkey PRIMARY KEY (id)\n)"], "{plan:#?}");
    pgmig::apply(&target.db.conninfo, &pgmig::Plan { statements: plan, commit_after: None }).expect("apply");
    assert!(target.plan(&opts).is_empty());
    let mut conn = target.db.connect();
    let t = Table::open(&mut conn, DEFAULT_SCHEMA).expect("open").expect("still there");
    assert_eq!(t.read(&mut conn).expect("read").len(), 1);

    // `schema diff` and `schema inspect` of the database do not see it either.
    let db = Source::Database(target.db.conninfo.clone());
    let diff = pgmig::schema_diff(&db, &Source::Sql(target.sql.clone()), &target.shadow, &opts).expect("diff");
    assert!(diff.statements.is_empty(), "{:#?}", diff.statements);
    let inspected = pgmig::schema_inspect(&db, &target.shadow, &opts).expect("inspect").statements;
    assert!(inspected.iter().all(|s| !s.contains("pgmig")), "{inspected:#?}");

    // Filters cannot bring it back.
    let include = Options { filter: pgmig::Filter::parse(&["pgmig", "public"], &[] as &[&str]).expect("filter"), ..Options::default() };
    assert!(target.plan(&include).is_empty());
}

#[test]
fn a_configured_revisions_schema_is_the_one_left_alone() {
    let target = Target::new("ops");
    let ops = Options { revisions_schema: "ops".into(), ..Options::default() };
    assert_eq!(target.plan(&ops).len(), 1, "only users is created");
    // With the default, `ops` is an ordinary schema the desired state does not have.
    let plan = target.plan(&Options::default());
    assert!(plan.iter().any(|s| s == "DROP TABLE ops.pgmig_revisions"), "{plan:#?}");
    assert_eq!(revisions::DEFAULT_SCHEMA, "pgmig");
}

#[test]
fn options_serialize_the_revisions_schema_only_when_it_is_not_the_default() {
    assert_eq!(serde_json::to_string(&Options::default()).expect("json"), r#"{"filter":{"include":[],"exclude":[]},"schema":null}"#);
    let ops = Options { revisions_schema: "ops".into(), ..Options::default() };
    let json = serde_json::to_string(&ops).expect("json");
    assert!(json.ends_with(r#""revisions_schema":"ops"}"#), "{json}");
    assert_eq!(serde_json::from_str::<Options>(&json).expect("parse"), ops);
    assert_eq!(serde_json::from_str::<Options>("{}").expect("parse").revisions_schema, "pgmig");
}

#[test]
fn the_revisions_schema_cannot_be_empty_or_the_managed_schema() {
    let err = serde_json::from_str::<Options>(r#"{"revisions_schema":""}"#).expect_err("empty").to_string();
    assert!(err.contains("not an empty string"), "{err}");
    let target = Target::new(DEFAULT_SCHEMA);
    let db = Source::Database(target.db.conninfo.clone());
    let sql = Source::Sql(target.sql.clone());
    let empty = Options { revisions_schema: String::new(), ..Options::default() };
    let err = pgmig::schema_diff(&db, &sql, &target.shadow, &empty).expect_err("empty").to_string();
    assert!(err.contains("not an empty string"), "{err}");
    let same = Options { schema: Some("pgmig".into()), ..Options::default() };
    let err = pgmig::schema_diff(&db, &sql, &target.shadow, &same).expect_err("same").to_string();
    assert!(err.contains("is the revisions schema") && err.contains("revisions_schema"), "{err}");
}
