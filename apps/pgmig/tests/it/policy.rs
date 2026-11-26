//! `--skip`: skipped changes leave the plan and are reported, on stderr or in `skipped`.

use serde_json::Value;

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn schema_diff_reports_each_skipped_change_on_stderr() {
    let (shadow, _keep) = shadow_arg();
    let from = SqlFile::new("CREATE SCHEMA app; CREATE TABLE app.t (id int); CREATE TABLE old (id int); CREATE TABLE keep (id int);");
    let to = SqlFile::new("CREATE TABLE keep (id int, name text);");
    let out = pgmig(&["schema", "diff", "--from", &from.path(), "--to", &to.path(), "--shadow", &shadow, "--skip", "drop_table"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "ALTER TABLE public.keep ADD COLUMN name text;\n");
    assert_eq!(
        text(&out.stderr),
        "skipped drop_schema app (because drop_table app.t is skipped)\n\
         skipped drop_table app.t (policy)\n\
         skipped drop_table public.old (policy)\n"
    );

    // JSON: the list, and nothing on stderr.
    let out = pgmig(&[
        "schema",
        "diff",
        "--from",
        &from.path(),
        "--to",
        &to.path(),
        "--shadow",
        &shadow,
        "--skip",
        "drop_table",
        "--skip",
        "add_column",
        "--format",
        "json",
    ]);
    assert!(out.status.success() && out.stderr.is_empty(), "{}", text(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["plan"]["statements"], serde_json::json!([]));
    let skipped = json["skipped"].as_array().expect("skipped");
    let mut kinds: Vec<&str> = skipped.iter().map(|s| s["kind"].as_str().unwrap_or("")).collect();
    kinds.sort();
    assert_eq!(kinds, ["add_column", "drop_schema", "drop_table", "drop_table"]);
    let add = skipped.iter().find(|s| s["kind"] == "add_column").expect("add_column");
    assert_eq!(add["statement"], "ALTER TABLE public.keep ADD COLUMN name text");
    assert_eq!(add["because"], Value::Null);

    // Only skipped changes: not a change for --exit-code.
    let out = pgmig(&[
        "schema",
        "diff",
        "--from",
        &from.path(),
        "--to",
        &to.path(),
        "--shadow",
        &shadow,
        "--skip",
        "drop_table",
        "--skip",
        "add_column",
        "--exit-code",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

#[test]
fn an_unknown_kind_is_a_usage_error() {
    let out = pgmig(&["schema", "diff", "--from", "a.sql", "--to", "b.sql", "--skip", "drop_tables"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(err.contains("unknown kind of change \"drop_tables\" (expected one of create_schema, drop_schema,"), "{err}");
}

#[test]
fn schema_apply_keeps_skipped_objects() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE keep (id int, gone int); CREATE TABLE old (id int);").expect("load target");
    let to = SqlFile::new("CREATE TABLE keep (id int); CREATE TABLE new (id int);");
    let args = [
        "schema",
        "apply",
        "--url",
        &db.conninfo,
        "--to",
        &to.path(),
        "--shadow",
        &shadow,
        "--skip",
        "drop_table",
        "--skip",
        "drop_column",
    ];
    let out = pgmig(&[&args[..], &["--auto-approve"]].concat());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "CREATE TABLE public.new (\n    id integer\n);\n");
    let err = text(&out.stderr);
    assert!(err.starts_with("skipped drop_column public.keep.gone (policy)\nskipped drop_table public.old (policy)\n"), "{err}");
    // Applied again: nothing to do, and the skipped changes are reported again.
    let out = pgmig(&[&args[..], &["--auto-approve"]].concat());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stderr), "skipped drop_column public.keep.gone (policy)\nskipped drop_table public.old (policy)\nno changes\n");
    let rows = db.connect().query("SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name");
    let tables: Vec<String> = rows.expect("query").into_iter().filter_map(|r| r[0].clone()).collect();
    assert_eq!(tables, ["keep", "new", "old"]);
}
