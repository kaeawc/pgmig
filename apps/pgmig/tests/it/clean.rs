//! `pgmig schema clean`.

use serde_json::Value;

use crate::support::{TestDb, pgmig};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn tables(db: &TestDb) -> String {
    let rows =
        db.connect().query("SELECT count(*) FROM pg_tables WHERE schemaname NOT IN ('pg_catalog', 'information_schema')").expect("query");
    rows[0][0].clone().unwrap_or_default()
}

fn populated() -> TestDb {
    let db = TestDb::new();
    db.connect()
        .batch("CREATE SCHEMA app; CREATE TYPE app.e AS ENUM ('x'); CREATE TABLE app.t (id serial PRIMARY KEY, e app.e); CREATE TABLE u (id int);")
        .expect("load target");
    db
}

#[test]
fn dry_run_prints_the_drops_and_changes_nothing() {
    let db = populated();
    let out = pgmig(&["schema", "clean", "--url", &db.conninfo, "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let sql = text(&out.stdout);
    for s in ["DROP TABLE app.t;", "DROP TABLE public.u;", "DROP TYPE app.e;", "DROP SCHEMA app;"] {
        assert!(sql.contains(s), "{s} not in {sql}");
    }
    assert!(text(&out.stderr).contains("Target: "), "{}", text(&out.stderr));
    assert_eq!(tables(&db), "2");
}

#[test]
fn refuses_without_a_terminal_or_auto_approve() {
    let db = populated();
    let out = pgmig(&["schema", "clean", "--url", &db.conninfo]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("--auto-approve"), "{}", text(&out.stderr));

    let out = pgmig(&["schema", "clean", "--url", &db.conninfo, "--format", "json"]);
    assert_eq!(out.status.code(), Some(2));
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["code"], "usage");
    assert_eq!(tables(&db), "2");
}

#[test]
fn auto_approve_cleans_and_a_second_run_has_nothing_to_do() {
    let db = populated();
    let args = ["schema", "clean", "--url", &db.conninfo, "--auto-approve"];
    let out = pgmig(&args);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("cleaned: "), "{}", text(&out.stderr));
    assert_eq!(tables(&db), "0");

    let again = pgmig(&args);
    assert!(again.status.success());
    assert_eq!(text(&again.stdout), "");
    assert_eq!(text(&again.stderr), "nothing to clean\n");
}

#[test]
fn excluded_objects_and_what_they_need_are_kept_and_reported() {
    let db = TestDb::new();
    db.connect().batch("CREATE TYPE e AS ENUM ('x'); CREATE TABLE keep (e e); CREATE TABLE gone (id int);").expect("load target");
    let out = pgmig(&["schema", "clean", "--url", &db.conninfo, "--exclude", "table:public.keep", "--auto-approve"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("kept enum public.e: column public.keep.e uses it"), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "DROP TABLE public.gone;\n");
    assert_eq!(tables(&db), "1");
}

#[test]
fn json_reports_the_target_plan_and_what_was_applied() {
    let db = populated();
    let out = pgmig(&["schema", "clean", "--url", &db.conninfo, "--format", "json", "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(out.stderr.is_empty());
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["applied"], false);
    assert_eq!(json["target"]["source"], "--url");
    assert!(json["database"].as_str().is_some_and(|d| !d.is_empty()));
    assert!(json["sql"].as_str().is_some_and(|s| s.contains("DROP SCHEMA app;")));
    assert_eq!(tables(&db), "2");

    let out = pgmig(&["schema", "clean", "--url", &db.conninfo, "--format", "json", "--auto-approve"]);
    assert!(out.status.success());
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["applied"], true);
    assert!(json["report"]["statements"].as_u64().is_some_and(|n| n > 0));
    assert_eq!(tables(&db), "0");
}

/// Runs `pgmig` in a terminal (under `script`) and types `input` at it.
#[cfg(unix)]
fn pgmig_typing(args: &[&str], input: &str) -> (Option<i32>, String) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let bin = env!("CARGO_BIN_EXE_pgmig");
    let mut cmd = Command::new("script");
    if cfg!(target_os = "linux") {
        let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
        let line = std::iter::once(bin).chain(args.iter().copied()).map(quote).collect::<Vec<_>>().join(" ");
        cmd.args(["-qec", &line, "/dev/null"]);
    } else {
        cmd.args(["-q", "/dev/null", bin]).args(args);
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("run pgmig under `script`");
    // Kept open until pgmig exits: `script` ends the terminal's input at EOF, which can come
    // before pgmig reads its line.
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(input.as_bytes()).expect("type");
    let out = child.wait_with_output().expect("wait");
    drop(stdin);
    (out.status.code(), format!("{}{}", text(&out.stdout), text(&out.stderr)))
}

#[cfg(unix)]
#[test]
fn the_prompt_needs_the_database_name() {
    let db = populated();
    let database = db.connect().query("SELECT current_database()").expect("query")[0][0].clone().expect("name");
    let args = ["schema", "clean", "--url", &db.conninfo];
    let (code, out) = pgmig_typing(&args, "yes\n");
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("Type its name to confirm") && out.contains("not cleaned"), "{out}");
    assert_eq!(tables(&db), "2");

    let (code, out) = pgmig_typing(&args, &format!("{database}\n"));
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(tables(&db), "0");
}
