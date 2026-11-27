//! `-- pgmig:rename`: a rename planned as `RENAME`, the data kept, the hints that did not apply, and
//! the hints the diff suggests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, DiffRequest, ErrorCode, PlanApplyRequest};
use pgmig::{ChangeKind, Error, Options, ShadowSpec, Source, apply};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig-rename-hints-it-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tmp(path)
    }

    fn sql(&self, name: &str, sql: &str) -> Source {
        let path = self.0.join(name);
        std::fs::write(&path, sql).expect("write sql");
        Source::Sql(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
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

fn skipping(kinds: &[ChangeKind]) -> Options {
    Options { skip: kinds.to_vec(), ..Options::default() }
}

fn diff(from: &str, to: &str) -> api::DiffResponse {
    diff_with(from, to, Options::default())
}

fn diff_with(from: &str, to: &str, options: Options) -> api::DiffResponse {
    let t = Tmp::new();
    let (shadow, _keep) = shadow();
    api::schema_diff(&DiffRequest { from: t.sql("from.sql", from), to: t.sql("to.sql", to), shadow, options }).expect("diff")
}

const USERS: &str = "CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL);\n";

#[test]
fn a_column_hint_plans_rename_column_and_keeps_the_data() {
    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch(USERS).expect("load target");
    conn.batch("INSERT INTO users VALUES (1, 'ada'), (2, 'grace')").expect("insert");
    let t = Tmp::new();
    let desired = t.sql(
        "schema.sql",
        "-- pgmig:rename column public.users.name full_name\n\
         CREATE TABLE users (id integer PRIMARY KEY, full_name text NOT NULL);\n",
    );
    let (shadow, _keep) = shadow();

    let plan = pgmig::schema_plan_apply(&db.conninfo, &desired, &shadow, &Options::default()).expect("plan");
    assert_eq!(plan.statements, ["ALTER TABLE public.users RENAME COLUMN name TO full_name"]);
    pgmig::schema_plan_apply(&db.conninfo, &desired, &shadow, &Options::default()).expect("planning again works");
    apply(&db.conninfo, &plan).expect("apply");

    let rows = conn.query("SELECT id, full_name FROM users ORDER BY id").expect("select");
    let rows: Vec<Vec<Option<String>>> = rows;
    assert_eq!(
        rows,
        [vec![Some("1".to_owned()), Some("ada".to_owned())], vec![Some("2".to_owned()), Some("grace".to_owned())]],
        "the data survives the rename"
    );
    // The hint stays in the file after it ran: nothing to plan, and it is reported as unused.
    let res = api::schema_plan_apply(&PlanApplyRequest {
        target: db.conninfo.clone(),
        desired,
        shadow,
        options: Options::default(),
        verify: true,
    })
    .expect("plan after");
    assert!(res.plan.is_empty(), "{}", res.sql);
    assert_eq!(res.unused_hints.len(), 1, "{:?}", res.unused_hints);
}

#[test]
fn a_table_hint_with_a_type_change_renames_then_alters_and_verifies() {
    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch("CREATE TABLE users (id integer PRIMARY KEY, score integer NOT NULL DEFAULT 0, name text); CREATE INDEX users_name_idx ON users (name);")
        .expect("load target");
    conn.batch("INSERT INTO users (id, score, name) VALUES (1, 7, 'ada')").expect("insert");
    let t = Tmp::new();
    let desired = t.sql(
        "schema.sql",
        "-- pgmig:rename table public.users accounts\n\
         -- pgmig:rename column public.accounts.score points\n\
         CREATE TABLE accounts (id integer, points bigint NOT NULL DEFAULT 0, name text, CONSTRAINT users_pkey PRIMARY KEY (id));\n\
         CREATE INDEX users_name_idx ON accounts (name);\n",
    );
    let (shadow, _keep) = shadow();
    let res = api::schema_plan_apply(&PlanApplyRequest {
        target: db.conninfo.clone(),
        desired,
        shadow,
        options: Options::default(),
        verify: true,
    })
    .expect("plan, verified");
    assert_eq!(
        res.plan.statements,
        [
            "ALTER TABLE public.users RENAME TO accounts",
            "ALTER TABLE public.accounts RENAME COLUMN score TO points",
            "ALTER TABLE public.accounts ALTER COLUMN points TYPE bigint",
        ]
    );
    assert!(res.hints.is_empty() && res.unused_hints.is_empty());
    apply(&db.conninfo, &res.plan).expect("apply");
    let rows = conn.query("SELECT id, points, name FROM accounts").expect("select");
    assert_eq!(rows, [vec![Some("1".to_owned()), Some("7".to_owned()), Some("ada".to_owned())]]);
}

#[test]
fn hints_can_stand_anywhere_in_a_file_and_in_any_file_of_a_directory() {
    let t = Tmp::new();
    std::fs::create_dir_all(t.path("schema")).expect("dir");
    std::fs::write(
        t.path("schema/a_users.sql"),
        "CREATE TABLE users (id integer, full_name text);\n-- pgmig:rename column public.users.name full_name\n",
    )
    .expect("write");
    std::fs::write(t.path("schema/b_more.sql"), "-- pgmig:rename table public.things stuff\n\nCREATE TABLE stuff (x integer);\n")
        .expect("write");
    let (shadow, _keep) = shadow();
    let from = t.sql("from.sql", "CREATE TABLE users (id integer, name text); CREATE TABLE things (x integer);");
    let res =
        api::schema_diff(&DiffRequest { from, to: Source::Sql(t.path("schema")), shadow, options: Options::default() }).expect("diff");
    assert_eq!(
        res.plan.statements,
        ["ALTER TABLE public.things RENAME TO stuff", "ALTER TABLE public.users RENAME COLUMN name TO full_name"]
    );
}

#[test]
fn a_hint_that_does_not_apply_is_reported_with_its_place_and_nothing_is_renamed() {
    let res = diff(
        "CREATE TABLE users (id integer, name text);\n",
        "-- pgmig:rename table public.people accounts\n\n-- pgmig:rename column public.accounts.nick name\nCREATE TABLE accounts (id integer, name text);\n",
    );
    // Nothing to rename from: the diff is a drop and a create, and says so.
    assert_eq!(res.plan.statements.len(), 2, "{}", res.sql);
    assert!(res.plan.statements.iter().any(|s| s.starts_with("DROP TABLE")), "{}", res.sql);
    let unused: Vec<(String, u32, &str)> = res.unused_hints.iter().map(|u| (u.directive.clone(), u.line, u.reason.as_str())).collect();
    assert_eq!(
        unused,
        [
            ("-- pgmig:rename table public.people accounts".to_owned(), 1, "table public.people does not exist in the current schema"),
            (
                "-- pgmig:rename column public.accounts.nick name".to_owned(),
                3,
                "column public.accounts.nick does not exist in the current schema"
            ),
        ]
    );
    assert!(res.unused_hints[0].file.ends_with("to.sql"));
    let json = serde_json::to_value(&res).expect("json");
    assert_eq!(json["unused_hints"][0]["line"], 1);
    assert_eq!(json["unused_hints"][0]["column"], 1);
    assert_eq!(
        res.unused_hints[0].to_string().replace(res.unused_hints[0].file.to_str().expect("utf8"), "to.sql"),
        "to.sql:1:1: unused rename hint `-- pgmig:rename table public.people accounts`: table public.people does not exist in the current schema"
    );
}

#[test]
fn a_hint_that_already_ran_is_unused_and_not_an_error() {
    let res =
        diff("CREATE TABLE accounts (id integer);\n", "-- pgmig:rename table public.users accounts\nCREATE TABLE accounts (id integer);\n");
    assert!(res.plan.is_empty(), "{}", res.sql);
    assert_eq!(res.unused_hints.len(), 1);
    assert_eq!(res.unused_hints[0].reason, "table public.users does not exist in the current schema (renamed already?)");
}

#[test]
fn without_a_hint_a_rename_is_a_drop_and_an_add_with_a_suggestion() {
    let res = diff(USERS, "CREATE TABLE users (id integer PRIMARY KEY, full_name text NOT NULL);\n");
    let sql: Vec<&str> = res.plan.statements.iter().map(String::as_str).collect();
    assert!(sql.iter().any(|s| s.contains("DROP COLUMN name")) && sql.iter().any(|s| s.contains("ADD COLUMN full_name")), "{sql:?}");
    assert_eq!(res.hints.len(), 1, "{:?}", res.hints);
    assert_eq!(res.hints[0].directive, "-- pgmig:rename column public.users.name full_name");
    assert!(res.hints[0].message.contains("column public.users.name is dropped and column full_name of the same type (text) is added"));
    assert!(res.unused_hints.is_empty());
    let json = serde_json::to_value(&res).expect("json");
    assert_eq!(json["hints"][0]["directive"], "-- pgmig:rename column public.users.name full_name");

    let res = diff("CREATE TABLE users (id integer, name text);\n", "CREATE TABLE accounts (name text, id integer);\n");
    assert_eq!(res.hints.len(), 1, "{:?}", res.hints);
    assert_eq!(res.hints[0].directive, "-- pgmig:rename table public.users accounts");
    assert!(res.plan.statements.iter().any(|s| s.starts_with("DROP TABLE")), "never renamed without the hint");
}

#[test]
fn nothing_is_suggested_when_the_types_or_the_columns_differ() {
    let res = diff("CREATE TABLE t (id integer, a text);\n", "CREATE TABLE t (id integer, b integer);\n");
    assert!(res.hints.is_empty(), "{:?}", res.hints);
    let res = diff("CREATE TABLE users (id integer);\n", "CREATE TABLE accounts (id integer, extra text);\n");
    assert!(res.hints.is_empty(), "{:?}", res.hints);
}

#[test]
fn a_malformed_hint_is_an_error_naming_the_file_and_line() {
    let t = Tmp::new();
    let (shadow, _keep) = shadow();
    let bad = t.sql("bad.sql", "CREATE TABLE t (id integer);\n\n-- pgmig:rename table users\n");
    let err = pgmig::schema_validate(&bad, &shadow).expect_err("malformed");
    assert!(matches!(&err, Error::Directive { pos, .. } if (pos.line, pos.col) == (3, 1)), "{err}");
    assert_eq!(
        err.to_string().replace(t.path("bad.sql").to_str().expect("utf8"), "bad.sql"),
        "bad.sql:3:1: pgmig:rename: rename table: expected the new name after the old one"
    );
    // A diagnostic of `schema validate`, with its position.
    let res = api::schema_validate(&api::ValidateRequest { source: bad, shadow: shadow.clone() }).expect("validate");
    assert_eq!(res.diagnostics.len(), 1);
    assert_eq!((res.diagnostics[0].code, res.diagnostics[0].line, res.diagnostics[0].col), (ErrorCode::Directive, Some(3), Some(1)));

    for (args, why) in [
        ("view public.v w", "unknown kind `view` to rename"),
        ("table users accounts", "the old name must be <schema>.<name>"),
        ("table public.users other.accounts", "moving it to another schema is not planned"),
        ("table public.users accounts extra", "unexpected `extra` after the new name"),
    ] {
        let bad = t.sql("bad.sql", &format!("-- pgmig:rename {args}\nCREATE TABLE t (id integer);\n"));
        let err = pgmig::schema_validate(&bad, &shadow).expect_err(args);
        assert!(matches!(&err, Error::Directive { message, .. } if message.contains(why)), "{args}: {err}");
    }
    // The hint is checked before any statement runs: a bad statement after it does not hide it.
    let both = t.sql("both.sql", "CREATE TABLE nope (;\n-- pgmig:rename table x y\n");
    assert!(matches!(pgmig::schema_validate(&both, &shadow), Err(Error::Directive { .. })));
}

#[test]
fn skipping_a_rename_kind_leaves_out_the_rename_and_what_depends_on_it() {
    let from = "CREATE TABLE users (id integer PRIMARY KEY, score integer NOT NULL, name text);\n";
    let to = "-- pgmig:rename column public.users.score points\n\
              CREATE TABLE users (id integer PRIMARY KEY, points bigint NOT NULL, name text, extra text);\n";
    let res = diff_with(from, to, skipping(&[ChangeKind::RenameColumn]));
    // The new column is a change of its own; the type change is of the renamed column.
    assert_eq!(res.plan.statements, ["ALTER TABLE public.users ADD COLUMN extra text"]);
    let skipped: Vec<String> = res.skipped.iter().map(ToString::to_string).collect();
    assert_eq!(
        skipped,
        [
            "rename_column public.users.score (policy)",
            "alter_column_type public.users.points (because rename_column public.users.score is skipped)",
        ]
    );
}

#[test]
fn a_skipped_rename_is_expected_to_remain_when_verifying() {
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE users (id integer PRIMARY KEY, name text);").expect("load target");
    let t = Tmp::new();
    let desired = t.sql(
        "schema.sql",
        "-- pgmig:rename column public.users.name full_name\nCREATE TABLE users (id integer PRIMARY KEY, full_name text);\n",
    );
    let (shadow, _keep) = shadow();
    let res = api::schema_plan_apply(&PlanApplyRequest {
        target: db.conninfo.clone(),
        desired,
        shadow,
        options: skipping(&[ChangeKind::RenameColumn]),
        verify: true,
    })
    .expect("verified");
    assert!(res.plan.is_empty(), "{}", res.sql);
    assert_eq!(res.skipped.len(), 1);
    assert_eq!(res.skipped[0].kind, ChangeKind::RenameColumn);
}

#[test]
fn a_hint_in_schema_mode_prints_unqualified_names() {
    let opts = Options { schema: Some("app".to_owned()), ..Options::default() };
    let res = diff_with(
        "CREATE SCHEMA app; CREATE TABLE app.users (id integer, name text);\n",
        "CREATE SCHEMA app;\n-- pgmig:rename table app.users accounts\nCREATE TABLE app.accounts (id integer, name text);\n",
        opts,
    );
    assert_eq!(res.plan.statements, ["ALTER TABLE users RENAME TO accounts"]);
}

#[test]
fn a_hint_whose_copy_cannot_be_made_says_so() {
    // The copy is rebuilt from what pgmig models, which has no functions: a default that calls one
    // cannot be copied, so the rename cannot be checked and nothing is guessed.
    let t = Tmp::new();
    let (shadow, _keep) = shadow();
    let from = t.sql(
        "from.sql",
        "CREATE FUNCTION f() RETURNS integer LANGUAGE sql AS 'SELECT 1';\nCREATE TABLE t (id integer DEFAULT f(), a text);\n",
    );
    let to = t.sql(
        "to.sql",
        "CREATE FUNCTION f() RETURNS integer LANGUAGE sql AS 'SELECT 1';\n-- pgmig:rename column public.t.a b\nCREATE TABLE t (id integer DEFAULT f(), b text);\n",
    );
    let err = pgmig::schema_diff(&from, &to, &shadow, &Options::default()).expect_err("no copy");
    assert!(matches!(&err, Error::RenameSetup { statement, .. } if statement.contains("CREATE TABLE")), "{err}");
    assert_eq!(api::ApiError::from(&err).code, ErrorCode::RenameHint);
}

#[test]
fn migrate_diff_writes_the_renames_and_reports_the_hints() {
    let t = Tmp::new();
    let dir = t.path("migrations");
    std::fs::create_dir_all(&dir).expect("migrations");
    std::fs::write(dir.join("20240101000000_init.sql"), USERS).expect("write");
    pgmig::migrate_hash(&dir).expect("hash");
    let desired = t.sql(
        "schema.sql",
        "-- pgmig:rename column public.users.name full_name
\
         -- pgmig:rename table public.gone elsewhere
\
         CREATE TABLE users (id integer PRIMARY KEY, full_name text NOT NULL);
",
    );
    let (shadow, _keep) = shadow();
    let res = pgmig::migrate_diff(&dir, "rename_name", &desired, &shadow, &Options::default()).expect("migrate diff");
    assert_eq!(res.sql, "ALTER TABLE public.users RENAME COLUMN name TO full_name;\n");
    assert_eq!(res.written.len(), 1);
    let written = std::fs::read_to_string(&res.written[0].path).expect("read");
    assert_eq!(written, res.sql);
    assert_eq!(res.unused_hints.len(), 1);
    assert!(res.unused_hints[0].reason.contains("public.gone"), "{:?}", res.unused_hints);
    // The rename is in the history now: a second diff finds nothing, and the hints are unused.
    let again = pgmig::migrate_diff(&dir, "again", &desired, &shadow, &Options::default()).expect("migrate diff again");
    assert!(again.written.is_empty() && again.plan.is_empty(), "{}", again.sql);
    assert_eq!(again.unused_hints.len(), 2);
}

#[test]
fn a_rename_with_a_type_change_that_needs_using_still_asks_the_server() {
    // The cast probe needs the desired state's connection, which a real Postgres shadow gives up
    // while the renames are made on a copy and takes again afterwards.
    let res = diff(
        "CREATE TABLE t (id integer, qty text);\n",
        "-- pgmig:rename column public.t.qty amount\nCREATE TABLE t (id integer, amount integer);\n",
    );
    assert_eq!(
        res.plan.statements,
        ["ALTER TABLE public.t RENAME COLUMN qty TO amount", "ALTER TABLE public.t ALTER COLUMN amount TYPE integer USING amount::integer"]
    );
}

#[test]
fn a_table_hint_renames_its_serial_sequence_so_the_next_value_carries_on() {
    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch("CREATE TABLE users (id serial PRIMARY KEY, name text);").expect("load target");
    conn.batch("INSERT INTO users (name) VALUES ('ada'), ('grace')").expect("insert");
    let t = Tmp::new();
    let desired =
        t.sql("schema.sql", "-- pgmig:rename table public.users accounts\nCREATE TABLE accounts (id serial PRIMARY KEY, name text);\n");
    let (shadow, _keep) = shadow();
    let res = api::schema_plan_apply(&PlanApplyRequest {
        target: db.conninfo.clone(),
        desired: desired.clone(),
        shadow: shadow.clone(),
        options: Options::default(),
        verify: true,
    })
    .expect("plan, verified");
    assert_eq!(
        res.plan.statements,
        [
            "ALTER TABLE public.users RENAME TO accounts",
            "ALTER SEQUENCE public.users_id_seq RENAME TO accounts_id_seq",
            "ALTER TABLE public.accounts RENAME CONSTRAINT users_pkey TO accounts_pkey",
        ]
    );
    apply(&db.conninfo, &res.plan).expect("apply");
    conn.batch("INSERT INTO accounts (name) VALUES ('edsger')").expect("the next insert takes a new id");
    let rows = conn.query("SELECT id, name FROM accounts ORDER BY id").expect("select");
    let ids: Vec<Option<String>> = rows.into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(ids, [Some("1".to_owned()), Some("2".to_owned()), Some("3".to_owned())]);
    let again = pgmig::schema_plan_apply(&db.conninfo, &desired, &shadow, &Options::default()).expect("plan again");
    assert!(again.is_empty(), "{:?}", again.statements);
}

#[test]
fn a_skipped_table_rename_leaves_out_what_names_the_new_table() {
    let db = TestDb::new();
    db.connect()
        .batch("CREATE TABLE users (id integer, name text, CONSTRAINT users_name_check CHECK (length(name) > 0));")
        .expect("load target");
    let t = Tmp::new();
    let desired = t.sql("schema.sql", "-- pgmig:rename table public.users accounts\nCREATE TABLE accounts (id integer, name text);\n");
    let (shadow, _keep) = shadow();
    let res = api::schema_plan_apply(&PlanApplyRequest {
        target: db.conninfo.clone(),
        desired,
        shadow,
        options: skipping(&[ChangeKind::RenameTable]),
        verify: true,
    })
    .expect("verified");
    assert!(res.plan.statements.iter().all(|s| !s.contains("accounts")), "{}", res.sql);
    let skipped: Vec<String> = res.skipped.iter().map(ToString::to_string).collect();
    assert_eq!(
        skipped,
        [
            "rename_table public.users (policy)",
            "drop_constraint public.accounts.users_name_check (because rename_table public.users is skipped)",
        ]
    );
    apply(&db.conninfo, &res.plan).expect("apply");
}

#[test]
fn a_skipped_enum_rename_leaves_out_what_casts_to_the_new_name() {
    let db = TestDb::new();
    db.connect().batch("CREATE TYPE mood AS ENUM ('sad', 'ok'); CREATE TABLE p (m mood DEFAULT 'ok');").expect("load target");
    let t = Tmp::new();
    let desired = t.sql(
        "schema.sql",
        "-- pgmig:rename enum public.mood feeling\nCREATE TYPE feeling AS ENUM ('sad', 'ok');\nCREATE TABLE p (m feeling DEFAULT 'sad');\n",
    );
    let (shadow, _keep) = shadow();
    let res = api::schema_plan_apply(&PlanApplyRequest {
        target: db.conninfo.clone(),
        desired,
        shadow,
        options: skipping(&[ChangeKind::RenameEnum]),
        verify: false,
    })
    .expect("plan");
    assert!(res.plan.statements.iter().all(|s| !s.contains("feeling")), "{}", res.sql);
    assert_eq!(res.skipped[0].kind, ChangeKind::RenameEnum);
    assert!(
        res.skipped
            .iter()
            .any(|s| s.kind == ChangeKind::SetDefault && s.because.as_ref().is_some_and(|c| c.kind == ChangeKind::RenameEnum)),
        "{:?}",
        res.skipped
    );
    apply(&db.conninfo, &res.plan).expect("apply");
}
