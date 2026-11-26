//! `schema fingerprint`: the same schema gives the same fingerprint from a live database, SQL
//! files and a migration replay, on pgmem and every Postgres version.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{Filter, Options, ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

/// One of everything the model holds: schemas, an enum, a `serial` and its sequence, a standalone
/// sequence, identity columns, every kind of constraint but exclusion, defaults and comments.
const SCHEMA: &str = "\
CREATE SCHEMA app;
COMMENT ON SCHEMA app IS 'the app';
CREATE TYPE app.mood AS ENUM ('sad', 'ok', 'happy');
CREATE SEQUENCE app.tickets START 100 INCREMENT 5 CACHE 10;
CREATE TABLE app.teams (
    id bigint GENERATED ALWAYS AS IDENTITY (START WITH 10) CONSTRAINT teams_pkey PRIMARY KEY,
    name text NOT NULL CONSTRAINT teams_name_key UNIQUE
);
CREATE TABLE app.users (
    id serial CONSTRAINT users_pkey PRIMARY KEY,
    team bigint CONSTRAINT users_team_fkey REFERENCES app.teams DEFERRABLE,
    mood app.mood DEFAULT 'ok',
    age integer CONSTRAINT users_age_check CHECK (age >= 0),
    created timestamptz NOT NULL DEFAULT now()
);
COMMENT ON TABLE app.users IS 'people';
COMMENT ON COLUMN app.users.mood IS 'how they feel';
COMMENT ON CONSTRAINT users_pkey ON app.users IS 'the key';
CREATE TABLE public.notes (body text);
";

/// The fingerprint of [`SCHEMA`]. Every backend must print exactly this: pgmem, and each Postgres
/// version CI runs. A change here changes every saved fingerprint, so it must be deliberate.
const SCHEMA_FINGERPRINT: &str = "h1:v+6Oq4XWM9PqlBqYT1TQ3s3e1yJJQRlQWaJK0ZhMRNw=";

/// The canonical text [`SCHEMA_FINGERPRINT`] hashes.
const SCHEMA_CANONICAL: &str = "pgmig-fingerprint-v1
schema 3:app + 7:the app
schema 6:public + 22:standard public schema
enum 3:app 4:mood 3 3:sad 2:ok 5:happy -
sequence 3:app 7:tickets 6:bigint 100 5 1 9223372036854775807 10 f - -
sequence 3:app 12:users_id_seq 7:integer 1 1 1 2147483647 1 f + 3:app 5:users 2:id -
table 3:app 5:teams -
column 2:id 6:bigint t - - + always 10 1 1 9223372036854775807 1 f
column 4:name 4:text t - - -
constraint 14:teams_name_key unique 13:UNIQUE (name) -
constraint 10:teams_pkey primary_key 16:PRIMARY KEY (id) -
table 3:app 5:users + 6:people
column 3:age 7:integer f - - -
column 7:created 24:timestamp with time zone t + 5:now() - -
column 2:id 7:integer t + 37:nextval('app.users_id_seq'::regclass) - -
column 4:mood 8:app.mood f + 14:'ok'::app.mood + 13:how they feel -
column 4:team 6:bigint f - - -
constraint 15:users_age_check check 18:CHECK ((age >= 0)) -
constraint 10:users_pkey primary_key 16:PRIMARY KEY (id) + 7:the key
constraint 15:users_team_fkey foreign_key 54:FOREIGN KEY (team) REFERENCES app.teams(id) DEFERRABLE -
table 6:public 5:notes -
column 4:body 4:text f - - -
";

/// A temporary directory, removed with the value.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-fingerprint-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create dir");
        TempDir(path)
    }

    fn sql(&self, sql: &str) -> Source {
        let file = self.0.join("schema.sql");
        std::fs::write(&file, sql).expect("write sql");
        Source::Sql(file)
    }

    fn migrations(&self, sql: &str) -> Source {
        let dir = self.0.join("migrations");
        std::fs::create_dir_all(&dir).expect("create migrations");
        std::fs::write(dir.join("20261126000000_init.sql"), sql).expect("write migration");
        pgmig::migrate_hash(&dir).expect("hash");
        Source::Migrations(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The shadow on the test backend; keep the database alive as long as the spec is used.
fn shadow_spec() -> (ShadowSpec, Option<TestDb>) {
    let db = postgres_dsn().map(|_| TestDb::new());
    (db.as_ref().map_or(ShadowSpec::Pgmem, |d| ShadowSpec::Postgres(d.conninfo.clone())), db)
}

fn fingerprint(source: &Source, opts: &Options) -> String {
    let (spec, _db) = shadow_spec();
    pgmig::schema_fingerprint(source, &spec, opts).expect("fingerprint")
}

/// A database holding `sql`.
fn database(sql: &str) -> TestDb {
    let db = TestDb::new();
    db.connect().batch(sql).expect("load");
    db
}

#[test]
fn a_database_sql_files_and_a_replay_of_one_schema_agree() {
    let dir = TempDir::new();
    let opts = Options::default();
    let from_sql = fingerprint(&dir.sql(SCHEMA), &opts);
    let from_migrations = fingerprint(&dir.migrations(SCHEMA), &opts);
    let db = database(SCHEMA);
    let from_database = fingerprint(&Source::Database(db.conninfo.clone()), &opts);
    assert_eq!(from_sql, from_database);
    assert_eq!(from_migrations, from_database);
    // The library function and the model agree.
    let model = opts.managed(&pgmig::inspect(&mut db.connect()).expect("inspect"));
    assert_eq!(pgmig::fingerprint(&model), from_database);
}

#[test]
fn the_fingerprint_of_a_fixed_schema_is_the_same_on_every_backend() {
    let dir = TempDir::new();
    let source = dir.sql(SCHEMA);
    let (spec, _db) = shadow_spec();
    let model = Options::default().managed(&pgmig::load_model(&source, &spec).expect("load"));
    // The text first: when a backend differs, the assertion shows where.
    assert_eq!(pgmig_model::canonical(&model), SCHEMA_CANONICAL, "on {}", crate::support::backend_name());
    assert_eq!(fingerprint(&source, &Options::default()), SCHEMA_FINGERPRINT, "on {}", crate::support::backend_name());
}

#[test]
fn identity_sequence_names_and_column_order_do_not_count() {
    let dir = TempDir::new();
    let want = fingerprint(&dir.sql("CREATE TABLE t (id int GENERATED BY DEFAULT AS IDENTITY, a text, b text);"), &Options::default());
    // The identity's sequence gets another name (`t_id_seq1`) while `t_id_seq` is taken, and the
    // columns are created in another order.
    let db = database(
        "CREATE SEQUENCE t_id_seq; \
         CREATE TABLE t (b text, id int GENERATED BY DEFAULT AS IDENTITY, a text); \
         DROP SEQUENCE t_id_seq;",
    );
    assert_eq!(fingerprint(&Source::Database(db.conninfo.clone()), &Options::default()), want);
}

#[test]
fn a_change_changes_the_fingerprint() {
    let dir = TempDir::new();
    let base = fingerprint(&dir.sql(SCHEMA), &Options::default());
    for change in [
        "ALTER TABLE app.users ALTER COLUMN mood SET DEFAULT 'sad'",
        "COMMENT ON TABLE app.users IS 'humans'",
        "ALTER TABLE app.users DROP CONSTRAINT users_age_check",
        "ALTER TYPE app.mood ADD VALUE 'meh'",
        "ALTER SEQUENCE app.tickets INCREMENT 6",
        "ALTER TABLE app.teams ALTER COLUMN id RESTART WITH 50",
        "ALTER TABLE public.notes ADD COLUMN at date",
    ] {
        let changed = fingerprint(&dir.sql(&format!("{SCHEMA}{change};\n")), &Options::default());
        // RESTART changes the sequence's state, not its options: no difference.
        if change.contains("RESTART") {
            assert_eq!(changed, base, "{change}");
        } else {
            assert_ne!(changed, base, "{change}");
        }
    }
}

#[test]
fn filters_and_the_revisions_schema_apply() {
    let dir = TempDir::new();
    let opts = Options::default();
    let without_notes = fingerprint(&dir.sql(&SCHEMA.replace("CREATE TABLE public.notes (body text);\n", "")), &opts);
    let excluding = Options { filter: Filter::parse::<&str, _>(&[], &["public.notes"]).expect("pattern"), ..Options::default() };
    assert_eq!(fingerprint(&dir.sql(SCHEMA), &excluding), without_notes);
    // pgmig's revisions schema is never managed, so it never enters.
    let with_revisions = format!("{SCHEMA}CREATE SCHEMA pgmig; CREATE TABLE pgmig.revisions (version text);\n");
    assert_eq!(fingerprint(&dir.sql(&with_revisions), &opts), fingerprint(&dir.sql(SCHEMA), &opts));
    // Schema mode fingerprints one schema's objects.
    let app = Options { schema: Some("app".into()), ..Options::default() };
    let app_only = fingerprint(&dir.sql(SCHEMA), &app);
    assert_ne!(app_only, fingerprint(&dir.sql(SCHEMA), &opts));
    assert_eq!(fingerprint(&dir.sql(&format!("{SCHEMA}CREATE TABLE public.more (x int);\n")), &app), app_only);
}

#[test]
fn the_api_returns_the_fingerprint() {
    let dir = TempDir::new();
    let (spec, _db) = shadow_spec();
    let req = pgmig::api::FingerprintRequest { source: dir.sql(SCHEMA), shadow: spec.clone(), options: Options::default() };
    let res = pgmig::api::schema_fingerprint(&req).expect("fingerprint");
    assert_eq!(res.fingerprint, pgmig::schema_fingerprint(&req.source, &spec, &req.options).expect("fingerprint"));
    assert!(res.fingerprint.starts_with("h1:"), "{}", res.fingerprint);
}
