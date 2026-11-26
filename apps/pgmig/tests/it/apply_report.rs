//! The `migrate apply --format json` report: snapshots with times, the directory and the target
//! replaced by placeholders.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

use crate::support::{TestDb, pgmig};

/// A temporary migration directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig_cli_report_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        for (name, text) in files {
            std::fs::write(path.join(name), text).expect("write file");
        }
        let d = Dir(path);
        assert_eq!(pgmig(&["migrate", "hash", "--dir", &d.arg()]).status.code(), Some(0));
        d
    }

    fn arg(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn normalize(v: &mut Value, dir: &str) {
    match v {
        Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                if k == "execution_ms" {
                    *v = Value::from(0);
                } else {
                    normalize(v, dir);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| normalize(v, dir)),
        Value::String(s) => *s = s.replace(dir, "<dir>"),
        _ => {}
    }
}

/// Runs `migrate apply --format json` and returns the exit code and the normalized output.
fn apply(db: &TestDb, d: &Dir, extra: &[&str]) -> (Option<i32>, String) {
    let mut args = vec!["migrate", "apply", "--url", &db.conninfo, "--dir"];
    let dir = d.arg();
    args.push(&dir);
    args.extend_from_slice(&["--format", "json"]);
    args.extend_from_slice(extra);
    let out = pgmig(&args);
    let mut v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    normalize(&mut v, &dir);
    if let Some(t) = v.get_mut("target") {
        *t = Value::from("<target>");
    }
    let text = serde_json::to_string_pretty(&v).expect("json");
    (out.status.code(), text)
}

/// The column of the error: Postgres points at `nosuchtype` in `BAD`; pgmem reports no position
/// (kaeawc/pgmem#166), so the CLI falls back to column 1.
fn col() -> usize {
    if std::env::var("PGMIG_TEST_DSN").is_ok_and(|s| !s.is_empty()) {
        let line = BAD.lines().find(|l| l.contains("nosuchtype")).expect("offending line");
        line[..line.find("nosuchtype").expect("token")].chars().count() + 1
    } else {
        1
    }
}

/// The snapshot with `{COL}` replaced by [`col`].
fn expected(snapshot: &str) -> String {
    snapshot.trim_end().replace("{COL}", &col().to_string())
}

const USERS: &str = "CREATE TABLE users (id int PRIMARY KEY);\nCREATE INDEX users_id ON users (id);\n";
const ORDERS: &str = "CREATE TABLE orders (id int);\n";
const BAD: &str = "CREATE TABLE refs (id int);\n\nCREATE TABLE broken (id nosuchtype);\nCREATE TABLE after_it (id int);\n";

#[test]
fn a_successful_run() {
    let db = TestDb::new();
    let d = Dir::new(&[("1_users.sql", USERS), ("2_orders.sql", ORDERS), ("3_later.sql", "CREATE TABLE later (id int);\n")]);
    let (code, json) = apply(&db, &d, &["1"]);
    assert_eq!(code, Some(0));
    assert_eq!(json, include_str!("apply_report/success.json").trim_end());
}

#[test]
fn a_dry_run_lists_what_is_pending() {
    let db = TestDb::new();
    let d = Dir::new(&[("1_users.sql", USERS), ("2_orders.sql", ORDERS)]);
    let (code, json) = apply(&db, &d, &["--dry-run"]);
    assert_eq!(code, Some(0));
    assert_eq!(json, include_str!("apply_report/dry_run.json").trim_end());
}

#[test]
fn a_failure_in_the_second_file_rolls_back_that_file_only() {
    let db = TestDb::new();
    let d = Dir::new(&[("1_users.sql", USERS), ("2_bad.sql", BAD), ("3_orders.sql", ORDERS)]);
    let (code, json) = apply(&db, &d, &[]);
    assert_eq!(code, Some(1));
    assert_eq!(json, expected(include_str!("apply_report/failure.json")));
}

#[test]
fn a_failure_without_a_transaction_is_partial() {
    let db = TestDb::new();
    let d = Dir::new(&[("1_users.sql", USERS), ("2_bad.sql", &format!("-- pgmig:txmode none\n{BAD}"))]);
    let (code, json) = apply(&db, &d, &[]);
    assert_eq!(code, Some(1));
    assert_eq!(json, expected(include_str!("apply_report/partial.json")));
}

#[test]
fn a_failure_in_the_single_transaction_rolls_back_every_file() {
    let db = TestDb::new();
    let d = Dir::new(&[("1_users.sql", USERS), ("2_bad.sql", BAD), ("3_orders.sql", ORDERS)]);
    let (code, json) = apply(&db, &d, &["--tx-mode", "all"]);
    assert_eq!(code, Some(1));
    assert_eq!(json, expected(include_str!("apply_report/all_rolled_back.json")));
}
