//! `migrate hash` and `migrate validate`: text and JSON output, exit codes.

use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

use crate::support::{pgmig, shadow_arg};

/// A temporary migration directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig_cli_migrate_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        for (name, text) in files {
            std::fs::write(path.join(name), text).expect("write file");
        }
        Dir(path)
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

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn parse(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

const USERS: &str = "CREATE TABLE users (id int PRIMARY KEY);\n";
const BAD: &str = "-- refers to a type that does not exist\nCREATE TABLE orders (id nosuchtype);\n";

#[test]
fn hash_then_validate() {
    let d = Dir::new(&[("20261125093000_create_users.sql", USERS)]);
    let out = pgmig(&["migrate", "validate", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no sum file"), "{}", stderr(&out));

    let out = pgmig(&["migrate", "hash", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("(1 file)"), "{}", stderr(&out));
    assert!(d.0.join("pgmig.sum").is_file());
    let out = pgmig(&["migrate", "hash", "--dir", &d.arg()]);
    assert!(stderr(&out).contains("is up to date"), "{}", stderr(&out));

    let (shadow, _keep) = shadow_arg();
    let out = pgmig(&["migrate", "validate", "--dir", &d.arg(), "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

#[test]
fn validate_reports_an_edited_file_as_json() {
    let d = Dir::new(&[("1_users.sql", USERS)]);
    assert_eq!(pgmig(&["migrate", "hash", "--dir", &d.arg()]).status.code(), Some(0));
    std::fs::write(d.0.join("1_users.sql"), "CREATE TABLE users (id bigint PRIMARY KEY);\n").expect("edit");
    let out = pgmig(&["migrate", "validate", "--dir", &d.arg(), "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let v = parse(&out);
    assert_eq!(v["replayed"], false);
    assert_eq!(v["diagnostics"][0]["code"], "file_edited");
    assert_eq!(v["diagnostics"][0]["file"], d.0.join("1_users.sql").to_string_lossy().as_ref());
}

#[test]
fn validate_replays_and_reports_the_failing_file_and_line() {
    let d = Dir::new(&[("1_users.sql", USERS), ("2_orders.sql", BAD)]);
    let out = pgmig(&["migrate", "hash", "--dir", &d.arg(), "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let v = parse(&out);
    assert_eq!(v["written"], true);
    assert_eq!(v["files"].as_array().map(Vec::len), Some(2));

    // Checksums alone pass.
    assert_eq!(pgmig(&["migrate", "validate", "--dir", &d.arg()]).status.code(), Some(0));

    let (shadow, _keep) = shadow_arg();
    let out = pgmig(&["migrate", "validate", "--dir", &d.arg(), "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(1));
    let file = d.0.join("2_orders.sql").to_string_lossy().into_owned();
    // pgmem reports no error position (kaeawc/pgmem#166): the statement start stands in.
    let at = if crate::support::shadow_is_postgres() { "2:25" } else { "2:1" };
    assert!(stderr(&out).starts_with(&format!("error: {file}:{at}: type \"nosuchtype\" does not exist")), "{}", stderr(&out));

    let out = pgmig(&["migrate", "validate", "--dir", &d.arg(), "--shadow", &shadow, "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    let v = parse(&out);
    assert_eq!(v["diagnostics"][0]["code"], "load");
    assert_eq!(v["diagnostics"][0]["sqlstate"], "42704");
    assert_eq!(v["diagnostics"][0]["file"], file.as_str());
    assert_eq!(v["diagnostics"][0]["line"], 2);
}

#[test]
fn malformed_name_fails_hash() {
    let d = Dir::new(&[("users.sql", USERS)]);
    let out = pgmig(&["migrate", "hash", "--dir", &d.arg(), "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(parse(&out)["code"], "migration_name");
    assert!(!d.0.join("pgmig.sum").exists());
}
