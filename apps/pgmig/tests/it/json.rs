//! `--format json` output and the exit codes.

use std::process::Output;

use serde_json::{Value, json};

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

const A: &str = "CREATE TABLE t (id int NOT NULL);";
const B: &str = "CREATE TABLE t (id int NOT NULL, n text);";

/// Stdout with `file`'s path replaced by `<FILE>`.
fn stdout(out: &Output, file: &SqlFile) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf-8").replace(&file.path(), "<FILE>")
}

fn code(out: &Output) -> Option<i32> {
    out.status.code()
}

fn parse(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn inspect_prints_the_model() {
    let (shadow, _keep) = shadow_arg();
    let a = SqlFile::new(A);
    let out = pgmig(&["schema", "inspect", &a.path(), "--shadow", &shadow, "--format", "json"]);
    assert_eq!(code(&out), Some(0));
    assert!(out.stderr.is_empty());
    let expected = r#"{
  "plan": {
    "statements": [
      "CREATE TABLE public.t (\n    id integer NOT NULL\n)"
    ],
    "commit_after": null
  },
  "model": {
    "schemas": [
      {
        "name": "public",
        "comment": "standard public schema"
      }
    ],
    "tables": [
      {
        "name": {
          "schema": "public",
          "name": "t"
        },
        "columns": [
          {
            "name": "id",
            "type_name": "integer",
            "type_ref": null,
            "not_null": true,
            "default": null,
            "comment": null,
            "default_sequences": [],
            "identity": null
          }
        ],
        "comment": null,
        "constraints": [],
        "indexes": []
      }
    ],
    "enums": [],
    "sequences": [],
    "domains": [],
    "composites": []
  },
  "sql": "CREATE TABLE public.t (\n    id integer NOT NULL\n);\n"
}
"#;
    assert_eq!(stdout(&out, &a), expected);
}

#[test]
fn fingerprint_prints_one_value_for_a_database_and_its_sql() {
    let (shadow, _keep) = shadow_arg();
    let a = SqlFile::new(A);
    let text = pgmig(&["schema", "fingerprint", &a.path(), "--shadow", &shadow]);
    assert_eq!(code(&text), Some(0));
    assert!(text.stderr.is_empty());
    let printed = String::from_utf8(text.stdout).expect("utf-8");
    let fp = printed.strip_suffix('\n').expect("one line");
    assert!(fp.starts_with("h1:") && !fp.contains('\n'), "{printed}");

    let db = TestDb::new();
    db.connect().batch(A).expect("load");
    let out = pgmig(&["schema", "fingerprint", &db.conninfo, "--format", "json"]);
    assert_eq!(code(&out), Some(0));
    assert_eq!(parse(&out), json!({ "fingerprint": fp }));

    // Scope flags apply: excluding the only table leaves an empty database's fingerprint.
    let empty = SqlFile::new("");
    let none = pgmig(&["schema", "fingerprint", &empty.path(), "--shadow", &shadow]);
    let excluded = pgmig(&["schema", "fingerprint", &a.path(), "--shadow", &shadow, "--exclude", "public.t"]);
    assert_eq!(String::from_utf8_lossy(&excluded.stdout), String::from_utf8_lossy(&none.stdout));
    assert_ne!(String::from_utf8_lossy(&none.stdout), printed);
}

#[test]
fn diff_prints_changes_and_statements() {
    let (shadow, _keep) = shadow_arg();
    let (a, b) = (SqlFile::new(A), SqlFile::new(B));
    let out = pgmig(&["schema", "diff", "--from", &a.path(), "--to", &b.path(), "--shadow", &shadow, "--format", "json"]);
    assert_eq!(code(&out), Some(0));
    assert!(out.stderr.is_empty());
    let expected = r#"{
  "plan": {
    "statements": [
      "ALTER TABLE public.t ADD COLUMN n text"
    ],
    "commit_after": null
  },
  "changes": [
    {
      "kind": "add_column",
      "data": {
        "table": {
          "schema": "public",
          "name": "t"
        },
        "column": {
          "name": "n",
          "type_name": "text",
          "type_ref": null,
          "not_null": false,
          "default": null,
          "comment": null,
          "default_sequences": [],
          "identity": null
        }
      }
    }
  ],
  "skipped": [],
  "sql": "ALTER TABLE public.t ADD COLUMN n text;\n"
}
"#;
    assert_eq!(stdout(&out, &a), expected);
}

#[test]
fn validate_prints_diagnostics() {
    let (shadow, _keep) = shadow_arg();
    let ok = SqlFile::new(A);
    let out = pgmig(&["schema", "validate", &ok.path(), "--shadow", &shadow, "--format", "json"]);
    assert_eq!(code(&out), Some(0));
    assert_eq!(stdout(&out, &ok), "{\n  \"diagnostics\": []\n}\n");

    let bad = SqlFile::new("CREATE TABLE t (id nosuch);");
    let out = pgmig(&["schema", "validate", &bad.path(), "--shadow", &shadow, "--format", "json"]);
    assert_eq!(code(&out), Some(1));
    assert!(out.stderr.is_empty());
    // Postgres points at the type; pgmem reports no position (kaeawc/pgmem#166), so the statement
    // start is used.
    let col = if std::env::var("PGMIG_TEST_DSN").is_ok_and(|s| !s.is_empty()) { 20 } else { 1 };
    let expected = format!(
        r#"{{
  "diagnostics": [
    {{
      "code": "load",
      "message": "<FILE>:1:{col}: type \"nosuch\" does not exist (SQLSTATE 42704)",
      "sqlstate": "42704",
      "file": "<FILE>",
      "line": 1,
      "col": {col},
      "apply": null
    }}
  ]
}}
"#
    );
    assert_eq!(stdout(&out, &bad), expected);
}

#[test]
fn errors_are_json_on_stdout() {
    let (shadow, _keep) = shadow_arg();
    let file = SqlFile::new(A);
    let path = format!("{}.missing", file.path());
    let out = pgmig(&["schema", "inspect", &path, "--shadow", &shadow, "--format", "json"]);
    assert_eq!(code(&out), Some(1));
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    let json = parse(&out);
    assert_eq!(json["code"], "io");
    assert_eq!(json["file"], path.as_str());
    assert!(json["message"].as_str().expect("message").starts_with("reading "));
    assert_eq!(json["sqlstate"], Value::Null);

    // Applying without approval, and without a terminal to ask on, is the same object.
    let out = pgmig(&["schema", "apply", "--url", "unused", "--to", &file.path(), "--shadow", &shadow, "--format", "json"]);
    assert_eq!(code(&out), Some(1));
    assert!(out.stderr.is_empty());
    assert_eq!(parse(&out)["code"], "conn");
}

#[test]
fn apply_json_reports_the_plan_and_what_ran() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    let to = SqlFile::new(A);
    let to_path = to.path();
    let run = |extra: &[&str]| {
        let mut args = vec!["schema", "apply", "--url", &db.conninfo, "--to", &to_path, "--shadow", &shadow, "--format", "json"];
        args.extend(extra);
        pgmig(&args)
    };
    let statements = json!(["CREATE TABLE public.t (\n    id integer NOT NULL\n)"]);

    let out = run(&["--dry-run", "--exit-code"]);
    assert_eq!(code(&out), Some(3));
    let json = parse(&out);
    assert_eq!((&json["applied"], &json["report"], &json["plan"]["statements"]), (&json!(false), &Value::Null, &statements));
    assert_eq!(json["sql"], "CREATE TABLE public.t (\n    id integer NOT NULL\n);\n");

    // stdin is not a terminal and nothing approves the plan: JSON mode never asks.
    let out = run(&[]);
    assert_eq!(code(&out), Some(1));
    assert!(out.stderr.is_empty());
    assert_eq!(parse(&out)["code"], "usage");

    let out = run(&["--auto-approve"]);
    assert_eq!(code(&out), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let json = parse(&out);
    assert_eq!(json["applied"], true);
    assert_eq!(json["report"], json!({"batches": 1, "statements": 1}));
    assert_eq!(json["plan"]["statements"], statements);

    let out = run(&["--auto-approve"]);
    assert_eq!(code(&out), Some(0));
    let json = parse(&out);
    assert_eq!((&json["applied"], &json["plan"]["statements"]), (&json!(false), &json!([])));
}

#[test]
fn exit_codes() {
    let (shadow, _keep) = shadow_arg();
    let (a, b, bad) = (SqlFile::new(A), SqlFile::new(B), SqlFile::new("CREATE TABLE t (id nosuch);"));
    let diff = |from: &SqlFile, to: &SqlFile, extra: &[&str]| {
        let (f, t) = (from.path(), to.path());
        let mut args = vec!["schema", "diff", "--from", &f, "--to", &t, "--shadow", &shadow];
        args.extend(extra);
        code(&pgmig(&args))
    };
    assert_eq!(diff(&a, &b, &[]), Some(0), "changes without --exit-code");
    assert_eq!(diff(&a, &a, &["--exit-code"]), Some(0), "--exit-code, nothing to change");
    assert_eq!(diff(&a, &b, &["--exit-code"]), Some(3));
    assert_eq!(diff(&a, &bad, &[]), Some(1), "invalid SQL");
    assert_eq!(diff(&a, &b, &["--no-such-flag"]), Some(2), "usage");
    assert_eq!(code(&pgmig(&["schema", "validate", &bad.path(), "--shadow", &shadow])), Some(1));
    assert_eq!(code(&pgmig(&["schema", "validate", &a.path(), "--shadow", &shadow])), Some(0));
}
