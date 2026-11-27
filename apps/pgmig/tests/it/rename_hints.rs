//! `-- pgmig:rename` on the command line: the plan, the warnings for hints that did not apply and
//! the notes suggesting one, on stderr or in `hints` and `unused_hints`.

use serde_json::Value;

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

const FROM: &str = "CREATE TABLE users (id int PRIMARY KEY, name text);";

#[test]
fn a_hint_makes_the_diff_a_rename_and_a_hint_that_ran_is_a_warning() {
    let (shadow, _keep) = shadow_arg();
    let from = SqlFile::new(FROM);
    let to = SqlFile::new("-- pgmig:rename column public.users.name full_name\nCREATE TABLE users (id int PRIMARY KEY, full_name text);");
    let out = pgmig(&["schema", "diff", "--from", &from.path(), "--to", &to.path(), "--shadow", &shadow, "--exit-code"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "ALTER TABLE public.users RENAME COLUMN name TO full_name;\n");
    assert_eq!(text(&out.stderr), "");

    // Once it ran, the same file plans nothing and the hint is reported as unused.
    let ran = SqlFile::new("CREATE TABLE users (id int PRIMARY KEY, full_name text);");
    let out = pgmig(&["schema", "diff", "--from", &ran.path(), "--to", &to.path(), "--shadow", &shadow, "--exit-code"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    let err = text(&out.stderr).replace(&to.path(), "to.sql");
    assert_eq!(
        err,
        "warning: to.sql:1:1: unused rename hint `-- pgmig:rename column public.users.name full_name`: \
         column public.users.name does not exist in the current schema (renamed already?)\n"
    );
}

#[test]
fn a_drop_and_an_add_that_look_like_a_rename_say_which_hint_to_write() {
    let (shadow, _keep) = shadow_arg();
    let from = SqlFile::new(FROM);
    let to = SqlFile::new("CREATE TABLE users (id int PRIMARY KEY, full_name text);");
    let out = pgmig(&["schema", "diff", "--from", &from.path(), "--to", &to.path(), "--shadow", &shadow]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("DROP COLUMN name"), "{}", text(&out.stdout));
    assert_eq!(
        text(&out.stderr),
        "note: column public.users.name is dropped and column full_name of the same type (text) is added; \
         if it is a rename, the hint keeps the data; add `-- pgmig:rename column public.users.name full_name` to the desired state\n"
    );

    let out = pgmig(&["schema", "diff", "--from", &from.path(), "--to", &to.path(), "--shadow", &shadow, "--format", "json"]);
    assert!(out.status.success() && out.stderr.is_empty(), "{}", text(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["hints"][0]["directive"], "-- pgmig:rename column public.users.name full_name");
    assert_eq!(json["unused_hints"], serde_json::json!([]));
}

#[test]
fn schema_apply_keeps_the_data_of_a_renamed_column() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch(FROM).expect("load target");
    conn.batch("INSERT INTO users VALUES (1, 'ada')").expect("insert");
    let to = SqlFile::new("-- pgmig:rename column public.users.name full_name\nCREATE TABLE users (id int PRIMARY KEY, full_name text);");
    let out = pgmig(&["schema", "apply", "--url", &db.conninfo, "--to", &to.path(), "--shadow", &shadow, "--auto-approve", "--verify"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let rows = conn.query("SELECT id, full_name FROM users").expect("select");
    assert_eq!(rows, [vec![Some("1".to_owned()), Some("ada".to_owned())]]);
}

#[test]
fn a_malformed_hint_fails_with_its_file_and_line() {
    let (shadow, _keep) = shadow_arg();
    let to = SqlFile::new("CREATE TABLE t (id int);\n-- pgmig:rename table public.t\n");
    let out = pgmig(&["schema", "validate", &to.path(), "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr).replace(&to.path(), "schema.sql");
    assert!(err.contains("schema.sql:2:1: pgmig:rename: rename table: expected the new name after the old one"), "{err}");
}
