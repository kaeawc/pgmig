//! Inspection does not depend on the connection's `search_path`.

use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{Options, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn, shadow};

/// `other` holds namesakes of `public`'s `b` and `mood`: under `search_path = other, public` the
/// server would print `public.b` and `public.mood` qualified, under the default unqualified.
const SCHEMA: &str = "CREATE SCHEMA other; \
     CREATE TABLE other.b (id int PRIMARY KEY); \
     CREATE TYPE other.mood AS ENUM ('x'); \
     CREATE TYPE public.mood AS ENUM ('a', 'b'); \
     CREATE TABLE public.b (id int PRIMARY KEY); \
     CREATE TABLE public.t (id serial, b int REFERENCES public.b, m public.mood DEFAULT 'a', ms public.mood[]);";

/// A connection to `db` whose session `search_path` is `other, public`, as a role setting would
/// make it. pgmem ignores startup options and `ALTER ROLE ... SET` (kaeawc/pgmem#227), so the
/// session sets it itself.
fn connect_with_search_path(db: &TestDb) -> pgmig::Conn {
    let mut conn = db.connect();
    conn.batch("SET search_path = other, public").expect("set search_path");
    conn
}

#[test]
fn a_target_with_another_search_path_has_no_changes() {
    let target = TestDb::new();
    let mut conn = connect_with_search_path(&target);
    conn.batch(SCHEMA).expect("load target");
    let a = pgmig::inspect(&mut conn).expect("inspect target");

    let (mut sh, _shadow_db) = shadow();
    sh.load(SCHEMA).expect("load shadow");
    let b = pgmig::inspect(sh.conn()).expect("inspect shadow");
    let changes = pgmig::diff_with_casts(&a, &b, sh.conn());
    assert!(changes.is_empty(), "{changes:?}");
}

/// On Postgres, through the library with the setting in the connection string, as a role's would be.
#[test]
fn a_target_connection_with_another_search_path_plans_nothing() {
    if postgres_dsn().is_none() {
        return; // kaeawc/pgmem#227
    }
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let file = std::env::temp_dir().join(format!("pgmig-search-path-{}-{}.sql", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&file, SCHEMA).expect("write sql");
    let target = TestDb::new();
    let conninfo = format!("{} options=-csearch_path=other,public", target.conninfo);
    let mut conn = pgmig::Conn::connect(&conninfo).expect("connect");
    // Postgres echoes the startup option as given, without normalizing the spacing.
    let shown = conn.query("SHOW search_path").expect("show")[0][0].clone().unwrap_or_default();
    assert_eq!(shown.replace(' ', ""), "other,public");
    conn.batch(SCHEMA).expect("load target");

    let shadow_db = TestDb::new();
    let spec = ShadowSpec::Postgres(shadow_db.conninfo.clone());
    let plan = pgmig::schema_plan_apply(&conninfo, &Source::Sql(file.clone()), &spec, &Options::default());
    let _ = std::fs::remove_file(&file);
    let plan = plan.expect("plan");
    assert!(plan.is_empty(), "{:?}", plan.statements);
}

#[test]
fn inspect_prints_names_qualified_and_leaves_the_session_alone() {
    let target = TestDb::new();
    let mut conn = connect_with_search_path(&target);
    conn.batch(SCHEMA).expect("load");
    let model = pgmig::inspect(&mut conn).expect("inspect");
    // Postgres echoes the startup option as given, without normalizing the spacing.
    let shown = conn.query("SHOW search_path").expect("show")[0][0].clone().unwrap_or_default();
    assert_eq!(shown.replace(' ', ""), "other,public");

    let t = &model.tables[&pgmig_model::QualName::new("public", "t")];
    let fk = t.constraints.iter().find(|c| c.name == "t_b_fkey").expect("t_b_fkey");
    assert_eq!(fk.definition, "FOREIGN KEY (b) REFERENCES public.b(id)");
    let m = t.column("m").expect("m");
    assert_eq!((m.type_name.as_str(), m.default.as_deref()), ("public.mood", Some("'a'::public.mood")));
    assert_eq!(t.column("ms").expect("ms").type_name, "public.mood[]");
    let id = t.column("id").expect("id");
    assert_eq!(id.default_sequences, [pgmig_model::QualName::new("public", "t_id_seq")]);
    // pgmem prints `nextval('t_id_seq'::regclass)` (kaeawc/pgmem#226); inspection qualifies it.
    assert_eq!(id.default.as_deref(), Some("nextval('public.t_id_seq'::regclass)"));

    // Inspection opens no transaction: a statement that fails now does not leave one open.
    assert!(conn.batch("SELECT 1/0").is_err());
    conn.batch("CREATE TABLE after_inspect ()").expect("session not in a failed transaction");
}

/// A pgmem serial default matches a Postgres target's text, so a public serial column converges.
#[test]
fn a_public_serial_default_prints_qualified() {
    let target = TestDb::new();
    let mut conn = target.connect();
    conn.batch("CREATE TABLE t (id serial, q int DEFAULT 1); CREATE TABLE \"Odd\" (id serial)").expect("load");
    let model = pgmig::inspect(&mut conn).expect("inspect");
    let t = &model.tables[&pgmig_model::QualName::new("public", "t")];
    assert_eq!(t.column("id").expect("id").default.as_deref(), Some("nextval('public.t_id_seq'::regclass)"));
    assert_eq!(t.column("q").expect("q").default.as_deref(), Some("1"));
    let odd = &model.tables[&pgmig_model::QualName::new("public", "Odd")];
    assert_eq!(odd.column("id").expect("id").default.as_deref(), Some("nextval('public.\"Odd_id_seq\"'::regclass)"));
}

/// A desired-state file that leaves a transaction open: inspection keeps the transaction's work
/// and the cast probe after it sees the same schema, so a matching target has no changes.
#[test]
fn a_desired_state_left_in_a_transaction_is_inspected_whole() {
    let sql = "CREATE TABLE t (id serial, n int); BEGIN; CREATE TABLE u (v text);";
    let target = TestDb::new();
    target.connect().batch("CREATE TABLE t (id serial, n int); CREATE TABLE u (v text);").expect("load target");
    let (mut sh, _shadow_db) = shadow();
    sh.load(sql).expect("load shadow");
    let a = pgmig::inspect(&mut target.connect()).expect("inspect target");
    let b = pgmig::inspect(sh.conn()).expect("inspect shadow");
    assert!(b.tables.contains_key(&pgmig_model::QualName::new("public", "u")), "{:?}", b.tables.keys());
    let changes = pgmig::diff_with_casts(&a, &b, sh.conn());
    assert!(changes.is_empty(), "{changes:?}");
    assert!(sh.conn().query("SELECT 1 FROM u").is_ok(), "the transaction's table is still there");
}
