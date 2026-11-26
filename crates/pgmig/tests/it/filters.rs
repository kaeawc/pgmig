//! `--include` / `--exclude` filters and schema mode.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{Filter, Options, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// A SQL file in the temp directory, removed with the value.
struct SqlFile(PathBuf);

impl SqlFile {
    fn new(sql: &str) -> SqlFile {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-filters-{}-{}.sql", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, sql).expect("write sql");
        SqlFile(path)
    }

    fn source(&self) -> Source {
        Source::Sql(self.0.clone())
    }
}

impl Drop for SqlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The shadow on the test backend; keep the database alive as long as the spec is used.
fn shadow_spec() -> (ShadowSpec, Option<TestDb>) {
    let db = postgres_dsn().map(|_| TestDb::new());
    (db.as_ref().map_or(ShadowSpec::Pgmem, |d| ShadowSpec::Postgres(d.conninfo.clone())), db)
}

fn excluding(patterns: &[&str]) -> Options {
    Options { filter: Filter::parse::<&str, _>(&[], patterns).expect("patterns"), schema: None }
}

fn diff(from: &str, to: &str, opts: &Options) -> Vec<String> {
    let (spec, _db) = shadow_spec();
    let (from, to) = (SqlFile::new(from), SqlFile::new(to));
    pgmig::schema_diff(&from.source(), &to.source(), &spec, opts).expect("diff").statements
}

#[test]
fn excluded_tables_are_hidden_from_both_sides() {
    let from = "CREATE TABLE users (id int); CREATE TABLE tmp_a (id int);";
    let to = "CREATE TABLE users (id int); CREATE TABLE tmp_b (x int);";
    assert_eq!(diff(from, to, &Options::default()).len(), 2, "unfiltered, tmp_b is created and tmp_a dropped");
    assert!(diff(from, to, &excluding(&["public.tmp_*"])).is_empty());
    // Includes: only `users` is managed, so the tmp tables are left alone too.
    let only_users = Options { filter: Filter::parse(&["table:public.users"], &[] as &[&str]).expect("patterns"), schema: None };
    assert!(diff(from, to, &only_users).is_empty());
}

#[test]
fn a_kind_prefix_limits_a_pattern_to_that_kind() {
    let from = "CREATE TYPE tmp_mood AS ENUM ('a'); CREATE TABLE tmp_t (id int);";
    assert_eq!(diff(from, "", &excluding(&["enum:public.tmp_*"])), ["DROP TABLE public.tmp_t"]);
    assert_eq!(diff(from, "", &excluding(&["table:public.tmp_*"])), ["DROP TYPE public.tmp_mood"]);
}

#[test]
fn an_excluded_column_is_left_alone() {
    let from = "CREATE TABLE t (id int, legacy text);";
    let to = "CREATE TABLE t (id int, added int);";
    assert_eq!(diff(from, to, &excluding(&["public.t.legacy"])), ["ALTER TABLE public.t ADD COLUMN added integer"]);
}

#[test]
fn a_foreign_key_to_an_excluded_table_plans_and_applies() {
    let parent = "CREATE TABLE p (id int PRIMARY KEY);";
    let desired = SqlFile::new("CREATE TABLE p (id int PRIMARY KEY, extra int); CREATE TABLE c (pid int REFERENCES p);");
    let opts = excluding(&["public.p"]);
    let target = TestDb::new();
    target.connect().batch(parent).expect("load target");
    let (spec, _db) = shadow_spec();
    let plan = pgmig::schema_plan_apply(&target.conninfo, &desired.source(), &spec, &opts).expect("plan");
    assert_eq!(
        plan.statements,
        [
            "CREATE TABLE public.c (\n    pid integer\n)",
            "ALTER TABLE public.c ADD CONSTRAINT c_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)"
        ]
    );
    pgmig::apply(&target.conninfo, &plan).expect("apply");
    let again = pgmig::schema_plan_apply(&target.conninfo, &desired.source(), &spec, &opts).expect("plan again");
    assert!(again.is_empty(), "{:?}", again.statements);
}

#[test]
fn schema_mode_manages_one_schema_with_unqualified_names() {
    let sql = SqlFile::new(
        "CREATE SCHEMA app; CREATE TABLE app.x (id int); \
         CREATE TYPE mood AS ENUM ('a'); CREATE TABLE t (id serial PRIMARY KEY, m mood); COMMENT ON TABLE t IS 'c';",
    );
    let (spec, _db) = shadow_spec();
    let public = Options { schema: Some("public".into()), ..Options::default() };
    let out = pgmig::schema_inspect(&sql.source(), &spec, &public).expect("inspect").to_sql();
    assert_eq!(
        out,
        "CREATE TYPE mood AS ENUM ('a');\n\
         CREATE SEQUENCE t_id_seq AS integer;\n\
         CREATE TABLE t (\n    id integer DEFAULT nextval('t_id_seq'::regclass) NOT NULL,\n    m mood,\n    CONSTRAINT t_pkey PRIMARY KEY (id)\n);\n\
         ALTER SEQUENCE t_id_seq OWNED BY t.id;\n\
         COMMENT ON TABLE t IS 'c';\n"
    );
    let app = Options { schema: Some("app".into()), ..Options::default() };
    let out = pgmig::schema_inspect(&sql.source(), &spec, &app).expect("inspect").to_sql();
    assert_eq!(out, "CREATE TABLE x (\n    id integer\n);\n");

    // Applying runs a qualified plan.
    let target = TestDb::new();
    let plan = pgmig::schema_plan_apply(&target.conninfo, &sql.source(), &spec, &public).expect("plan");
    assert!(plan.statements.iter().any(|s| s.starts_with("CREATE TABLE public.t ")), "{:?}", plan.statements);
    pgmig::apply(&target.conninfo, &plan).expect("apply");
    let again = pgmig::schema_plan_apply(&target.conninfo, &sql.source(), &spec, &public).expect("plan again");
    assert!(again.is_empty(), "{:?}", again.statements);
}

#[test]
fn schema_mode_apply_never_touches_another_schemas_namesake() {
    // Under the default search_path, an unqualified `DROP TABLE x` would drop public.x.
    let target = TestDb::new();
    target.connect().batch("CREATE SCHEMA app; CREATE TABLE app.x (id int); CREATE TABLE public.x (id int);").expect("load target");
    let desired = SqlFile::new("CREATE SCHEMA app;");
    let (spec, _db) = shadow_spec();
    let app = Options { schema: Some("app".into()), ..Options::default() };
    let plan = pgmig::schema_plan_apply(&target.conninfo, &desired.source(), &spec, &app).expect("plan");
    assert_eq!(plan.statements, ["DROP TABLE app.x"]);
    pgmig::apply(&target.conninfo, &plan).expect("apply");
    let left = pgmig::inspect(&mut target.connect()).expect("inspect");
    let names: Vec<String> = left.tables.keys().map(|n| format!("{}.{}", n.schema, n.name)).collect();
    assert_eq!(names, ["public.x"]);
}

#[test]
fn a_serial_sequence_follows_its_excluded_table() {
    let opts = excluding(&["table:public.tmp_*"]);
    // Desired adds an excluded serial table: its sequence is not created either.
    assert!(diff("", "CREATE TABLE tmp_b (id serial);", &opts).is_empty());
    // The target has one: its sequence is not dropped either.
    let target = TestDb::new();
    target.connect().batch("CREATE TABLE tmp_a (id serial);").expect("load target");
    let desired = SqlFile::new("");
    let (spec, _db) = shadow_spec();
    let plan = pgmig::schema_plan_apply(&target.conninfo, &desired.source(), &spec, &opts).expect("plan");
    assert!(plan.is_empty(), "{:?}", plan.statements);
}

#[test]
fn a_serial_sequence_follows_its_excluded_column() {
    let opts = excluding(&["public.t.legacy"]);
    let from = "CREATE TABLE t (id int, legacy serial);";
    assert!(diff("CREATE TABLE t (id int);", from, &opts).is_empty());
    assert!(diff(from, "CREATE TABLE t (id int);", &opts).is_empty());
}

#[test]
fn an_included_serial_table_brings_its_sequence() {
    let opts = Options { filter: Filter::parse(&["table:public.users"], &[] as &[&str]).expect("patterns"), schema: None };
    let plan = diff("", "CREATE TABLE users (id serial); CREATE SEQUENCE other;", &opts);
    assert_eq!(plan.len(), 3, "{plan:?}");
    assert!(plan[0].starts_with("CREATE SEQUENCE public.users_id_seq"), "{plan:?}");
}
