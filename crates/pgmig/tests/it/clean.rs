//! `schema_plan_clean` + `apply`: a cleaned database equals a new one.

use pgmig::{Error, Filter, Model, Options, ShadowSpec, Source, apply, inspect, schema_fingerprint, schema_plan_clean};

use crate::support::TestDb;

const POPULATED: &str = "
    CREATE SCHEMA app;
    COMMENT ON SCHEMA app IS 'the app';
    COMMENT ON SCHEMA public IS 'mine';
    CREATE TYPE app.mood AS ENUM ('happy', 'sad');
    COMMENT ON TYPE app.mood IS 'moods';
    CREATE SEQUENCE app.counter;
    CREATE TABLE app.a (id serial PRIMARY KEY, mood app.mood NOT NULL DEFAULT 'happy', n bigint DEFAULT nextval('app.counter'));
    CREATE TABLE public.b (id int PRIMARY KEY, a_id int REFERENCES app.a (id));
    ALTER TABLE app.a ADD COLUMN b_id int REFERENCES public.b (id);
    CREATE TABLE app.c (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, moods app.mood[]);
    COMMENT ON TABLE app.a IS 'a';
    COMMENT ON COLUMN public.b.a_id IS 'to a';
    INSERT INTO app.a (mood) VALUES ('sad');
";

fn target(sql: &str) -> TestDb {
    let db = TestDb::new();
    db.connect().batch(sql).expect("load target");
    db
}

fn managed(db: &TestDb) -> Model {
    Options::default().managed(&inspect(&mut db.connect()).expect("inspect"))
}

fn fingerprint(db: &TestDb) -> String {
    schema_fingerprint(&Source::Database(db.conninfo.clone()), &ShadowSpec::Pgmem, &Options::default()).expect("fingerprint")
}

fn clean(db: &TestDb, opts: &Options) -> pgmig::SchemaClean {
    let c = schema_plan_clean(&db.conninfo, opts).expect("plan clean");
    apply(&db.conninfo, &c.plan).unwrap_or_else(|e| panic!("{e}\n{}", c.plan.to_sql()));
    c
}

#[test]
fn a_cleaned_database_equals_a_new_one() {
    let db = target(POPULATED);
    let c = clean(&db, &Options::default());
    assert!(!c.database.is_empty());
    assert!(c.local, "the test database is on this machine (server address {:?})", c.server_address);
    assert!(c.kept.is_empty(), "{:?}", c.kept);
    assert_eq!(managed(&db), Model::empty_database());
    assert_eq!(fingerprint(&db), fingerprint(&TestDb::new()));
    // Nothing is left to clean.
    assert!(schema_plan_clean(&db.conninfo, &Options::default()).expect("plan again").plan.is_empty());
}

#[test]
fn the_revisions_schema_stays() {
    let db = target("CREATE SCHEMA pgmig; CREATE TABLE pgmig.history (id int); CREATE TABLE t (id int);");
    let c = clean(&db, &Options::default());
    assert!(c.plan.to_sql().contains("DROP TABLE public.t"), "{}", c.plan.to_sql());
    let full = inspect(&mut db.connect()).expect("inspect");
    assert!(full.schemas.contains_key("pgmig") && full.tables.keys().any(|t| t.schema == "pgmig" && t.name == "history"));
    assert_eq!(managed(&db), Model::empty_database());
}

#[test]
fn an_excluded_table_stays_with_what_it_needs() {
    let db = target(
        "CREATE SCHEMA app;
         CREATE TYPE app.mood AS ENUM ('a', 'b');
         CREATE SEQUENCE app.ids;
         CREATE TABLE app.parent (id int PRIMARY KEY, extra text);
         CREATE TABLE public.keep (id bigint DEFAULT nextval('app.ids'), m app.mood, p int REFERENCES app.parent (id));
         CREATE TABLE public.gone (id int);
         CREATE TYPE app.unused AS ENUM ('x');",
    );
    let opts = Options { filter: Filter::parse::<&str, _>(&[], &["table:public.keep"]).expect("filter"), ..Options::default() };
    let c = clean(&db, &opts);
    let objects: Vec<&str> = c.kept.iter().map(|k| k.object.as_str()).collect();
    for o in ["enum app.mood", "sequence app.ids", "table app.parent", "schema app"] {
        assert!(objects.contains(&o), "{o} not in {objects:?}");
    }
    let m = inspect(&mut db.connect()).expect("inspect");
    let names: Vec<String> = m.tables.keys().map(|n| n.to_string()).collect();
    assert_eq!(names, ["app.parent", "public.keep"]);
    assert_eq!(m.enums.keys().map(|n| n.to_string()).collect::<Vec<_>>(), ["app.mood"]);
    assert_eq!(m.sequences.keys().map(|n| n.to_string()).collect::<Vec<_>>(), ["app.ids"]);
    // Kept objects are kept whole: a second clean has nothing left to do.
    assert!(schema_plan_clean(&db.conninfo, &opts).expect("plan again").plan.is_empty());
}

#[test]
fn schema_mode_empties_the_schema_and_keeps_it() {
    let db = target("CREATE SCHEMA app; CREATE TABLE app.t (id serial); CREATE TYPE app.e AS ENUM ('x'); CREATE TABLE other (id int);");
    let opts = Options { schema: Some("app".into()), ..Options::default() };
    clean(&db, &opts);
    let m = inspect(&mut db.connect()).expect("inspect");
    assert!(m.schemas.contains_key("app"));
    assert!(m.enums.is_empty() && m.sequences.is_empty());
    assert_eq!(m.tables.keys().map(|n| n.to_string()).collect::<Vec<_>>(), ["public.other"]);
}

#[test]
fn an_empty_database_has_nothing_to_clean() {
    let db = TestDb::new();
    assert!(schema_plan_clean(&db.conninfo, &Options::default()).expect("plan").plan.is_empty());
}

#[test]
fn schema_mode_on_the_revisions_schema_is_refused() {
    let db = TestDb::new();
    let opts = Options { schema: Some("pgmig".into()), ..Options::default() };
    assert!(matches!(schema_plan_clean(&db.conninfo, &opts), Err(Error::Options(_))));
}
