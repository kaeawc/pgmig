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

const SCHEMA: &str = "CREATE TABLE users (id int PRIMARY KEY);\nCREATE TABLE orders (id int PRIMARY KEY, user_id int REFERENCES users);\n";

/// The `.sql` files of a directory, in name order.
fn sql_files(d: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(d)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".sql")).collect())
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn diff_writes_a_migration_then_reports_no_changes() {
    let t = Dir::new(&[("schema.sql", SCHEMA)]);
    let dir = t.0.join("migrations");
    let (dir_arg, to) = (dir.to_string_lossy().into_owned(), t.0.join("schema.sql").to_string_lossy().into_owned());
    let (shadow, _keep) = shadow_arg();
    let out = pgmig(&["migrate", "diff", "Create Tables", "--dir", &dir_arg, "--to", &to, "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    let files = sql_files(&dir);
    let [file] = files.as_slice() else { panic!("one file: {files:?}") };
    assert!(file.ends_with("_create_tables.sql") && file.len() == "20261126103000_create_tables.sql".len(), "{file}");
    assert!(stderr(&out).starts_with(&format!("wrote {}", dir.join(file).display())), "{}", stderr(&out));
    let text = std::fs::read_to_string(dir.join(file)).expect("read");
    assert!(text.contains("CREATE TABLE public.orders"), "{text}");

    // The directory validates and replays, and matches the desired state.
    let out = pgmig(&["migrate", "validate", "--dir", &dir_arg, "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = pgmig(&["schema", "diff", "--from", &dir_arg, "--to", &to, "--shadow", &shadow, "--exit-code"]);
    assert_eq!(out.status.code(), Some(0), "{}{}", stderr(&out), String::from_utf8_lossy(&out.stdout));
    assert!(out.stdout.is_empty());

    let sum = std::fs::read(dir.join("pgmig.sum")).expect("sum");
    let out = pgmig(&["migrate", "diff", "again", "--dir", &dir_arg, "--to", &to, "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "no changes\n");
    assert_eq!(sql_files(&dir).len(), 1);
    assert_eq!(std::fs::read(dir.join("pgmig.sum")).expect("sum"), sum);

    let out = pgmig(&["migrate", "diff", "again", "--dir", &dir_arg, "--to", &to, "--shadow", &shadow, "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v = parse(&out);
    assert_eq!((v["file"].is_null(), v["version"].is_null(), v["sql"].as_str()), (true, true, Some("")));
}

#[test]
fn diff_and_new_read_the_config_file_and_print_json() {
    let t = Dir::new(&[("schema.sql", SCHEMA)]);
    let (shadow, _keep) = shadow_arg();
    let config = t.0.join("pgmig.toml");
    let toml = format!(
        "to = {:?}\nshadow = {:?}\n[migrations]\ndir = {:?}\n",
        t.0.join("schema.sql").to_string_lossy(),
        shadow,
        t.0.join("m").to_string_lossy()
    );
    std::fs::write(&config, toml).expect("config");
    let config = config.to_string_lossy().into_owned();
    let out = pgmig(&["--config", &config, "migrate", "diff", "init", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let v = parse(&out);
    let file = v["file"].as_str().expect("file").to_owned();
    assert_eq!(std::fs::read_to_string(&file).expect("read"), v["sql"].as_str().expect("sql"));
    assert!(v["changes"].as_array().is_some_and(|c| !c.is_empty()));

    let out = pgmig(&["--config", &config, "migrate", "new", "Backfill!", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let v = parse(&out);
    let new = v["file"].as_str().expect("file");
    assert!(new.ends_with(&format!("{}_backfill.sql", v["version"].as_str().expect("version"))), "{new}");
    assert_eq!(std::fs::read(new).expect("read"), b"");
    let out = pgmig(&["--config", &config, "migrate", "validate"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn diff_refuses_an_edited_directory() {
    let d = Dir::new(&[("1_users.sql", USERS)]);
    assert_eq!(pgmig(&["migrate", "hash", "--dir", &d.arg()]).status.code(), Some(0));
    std::fs::write(d.0.join("1_users.sql"), "CREATE TABLE users (id bigint PRIMARY KEY);\n").expect("edit");
    let schema = Dir::new(&[("schema.sql", SCHEMA)]);
    let to = schema.0.join("schema.sql").to_string_lossy().into_owned();
    let out = pgmig(&["migrate", "diff", "x", "--dir", &d.arg(), "--to", &to, "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(parse(&out)["code"], "file_edited");
    assert_eq!(sql_files(&d.0), ["1_users.sql"]);
    let out = pgmig(&["migrate", "new", "!!", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}
