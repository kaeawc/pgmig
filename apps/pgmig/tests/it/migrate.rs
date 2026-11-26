//! `migrate hash` and `migrate validate`: text and JSON output, exit codes.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

use crate::support::{TestDb, pgmig, shadow_arg};

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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn apply_lists_applied_files_then_nothing() {
    let db = TestDb::new();
    let d = Dir::new(&[("1_users.sql", USERS), ("2_orders.sql", "CREATE TABLE orders (id int);\n")]);
    assert_eq!(pgmig(&["migrate", "hash", "--dir", &d.arg()]).status.code(), Some(0));

    let out = pgmig(&["migrate", "apply", "--url", &db.conninfo, "--dir", &d.arg(), "--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "1_users.sql\n2_orders.sql\n");
    assert!(stderr(&out).contains("2 migrations pending (dry run"), "{}", stderr(&out));

    let out = pgmig(&["migrate", "apply", "1", "--url", &db.conninfo, "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).starts_with("1_users.sql (1 statement, "), "{}", stdout(&out));
    assert!(stderr(&out).contains("applied 1 migration (1 statement); 1 left pending"), "{}", stderr(&out));

    let out = pgmig(&["migrate", "apply", "--url", &db.conninfo, "--dir", &d.arg(), "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let json = parse(&out);
    assert_eq!((json["applied"].as_u64(), json["dry_run"].as_bool()), (Some(1), Some(false)));
    assert_eq!(json["files"][0]["status"], "already_applied");
    assert_eq!((json["files"][1]["status"].as_str(), json["files"][1]["tx_mode"].as_str()), (Some("applied"), Some("file")));
    assert_eq!(json["target"]["source"], "--url");

    let out = pgmig(&["migrate", "apply", "--url", &db.conninfo, "--dir", &d.arg()]);
    assert_eq!((out.status.code(), stdout(&out).as_str()), (Some(0), ""));
    assert!(stderr(&out).contains("nothing to apply"), "{}", stderr(&out));
}

#[test]
fn apply_reports_the_failing_statement_and_reads_the_config_file() {
    let db = TestDb::new();
    let d = Dir::new(&[("1_orders.sql", BAD)]);
    assert_eq!(pgmig(&["migrate", "hash", "--dir", &d.arg()]).status.code(), Some(0));
    let config = d.0.join("pgmig.toml");
    std::fs::write(
        &config,
        format!("url = {:?}\n[migrations]\ndir = {:?}\ntx_mode = \"none\"\nlock_timeout = \"2s\"\n", db.conninfo, d.arg()),
    )
    .expect("config");
    let cfg = config.to_string_lossy().into_owned();
    let out = pgmig(&["migrate", "apply", "--config", &cfg]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("1_orders.sql:2:") && stderr(&out).contains("42704"), "{}", stderr(&out));
    assert!(stderr(&out).contains("no transaction"), "{}", stderr(&out));

    let out = pgmig(&["migrate", "apply", "--config", &cfg, "--format", "json", "--tx-mode", "file"]);
    assert_eq!(out.status.code(), Some(1));
    let json = parse(&out);
    assert_eq!((json["code"].as_str(), json["sqlstate"].as_str(), json["line"].as_u64()), (Some("migration"), Some("42704"), Some(2)));
    assert_eq!(json["migration"]["outcome"], "file_rolled_back");

    let out = pgmig(&["migrate", "apply", "--config", &cfg, "--no-lock", "--lock-name", "x"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let out = pgmig(&["migrate", "apply", "--config", &cfg, "--lock-timeout", "soon"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

const M1: &str = "20261101000000_create_a.sql";
const M2: &str = "20261102000000_create_b.sql";
const M3: &str = "20261103000000_create_c.sql";

fn hashed_three() -> Dir {
    let d = Dir::new(&[(M1, "CREATE TABLE a (id int);\n"), (M2, "CREATE TABLE b (id int);\n"), (M3, "CREATE TABLE c (id int);\n")]);
    assert_eq!(pgmig(&["migrate", "hash", "--dir", &d.arg()]).status.code(), Some(0));
    d
}

fn validates(d: &Dir) {
    let out = pgmig(&["migrate", "validate", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn ls_lists_versions_and_with_url_their_status() {
    let d = hashed_three();
    let out = pgmig(&["migrate", "ls", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "20261101000000  create_a\n20261102000000  create_b\n20261103000000  create_c\n");

    let db = TestDb::new();
    let out = pgmig(&["migrate", "apply", "1", "--url", &db.conninfo, "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = pgmig(&["migrate", "ls", "--dir", &d.arg(), "--url", &db.conninfo]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "applied  20261101000000  create_a\npending  20261102000000  create_b\npending  20261103000000  create_c\n");
    let v = parse(&pgmig(&["migrate", "ls", "--dir", &d.arg(), "--url", &db.conninfo, "--format", "json"]));
    assert_eq!(v["files"][0]["status"], "applied");
    assert_eq!(v["files"][1]["file"], M2);
    assert_eq!(v["missing"], serde_json::json!([]));
}

#[test]
fn show_prints_the_file() {
    let d = hashed_three();
    let out = pgmig(&["migrate", "show", "20261102000000", "--dir", &d.arg()]);
    assert_eq!((out.status.code(), stdout(&out).as_str()), (Some(0), "CREATE TABLE b (id int);\n"));
    let v = parse(&pgmig(&["migrate", "show", M3, "--dir", &d.arg(), "--format", "json"]));
    assert_eq!((v["version"].as_str(), v["sql"].as_str()), (Some("20261103000000"), Some("CREATE TABLE c (id int);\n")));
    let out = pgmig(&["migrate", "show", "2026110", "--dir", &d.arg(), "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(parse(&out)["code"], "migration_ambiguous");
    let out = pgmig(&["migrate", "show", "42", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no migration has the version or file name \"42\""), "{}", stderr(&out));
}

#[test]
fn edit_refuses_without_a_terminal() {
    let d = hashed_three();
    let out = pgmig(&["migrate", "edit", M1, "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("stdin is not a terminal"), "{}", stderr(&out));
    validates(&d);
}

#[test]
fn rm_removes_and_refuses_applied_files() {
    let d = hashed_three();
    let db = TestDb::new();
    assert_eq!(pgmig(&["migrate", "apply", "1", "--url", &db.conninfo, "--dir", &d.arg()]).status.code(), Some(0));
    let out = pgmig(&["migrate", "rm", M1, "--dir", &d.arg(), "--url", &db.conninfo, "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(parse(&out)["code"], "migration_applied");
    assert!(d.0.join(M1).is_file());

    let out = pgmig(&["migrate", "rm", "20261103000000", "--dir", &d.arg(), "--url", &db.conninfo]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("removed"), "{}", stderr(&out));
    assert!(!d.0.join(M3).exists());
    validates(&d);

    let out = pgmig(&["migrate", "rm", M1, "--dir", &d.arg(), "--url", &db.conninfo, "--force", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(parse(&out)["recorded"], true);
    validates(&d);
}

#[test]
fn rebase_moves_a_branch_file_after_the_others() {
    let d = Dir::new(&[(M1, "CREATE TABLE a (id int);\n"), (M3, "CREATE TABLE theirs (id int);\n")]);
    assert_eq!(pgmig(&["migrate", "hash", "--dir", &d.arg()]).status.code(), Some(0));
    std::fs::write(d.0.join(M2), "CREATE TABLE ours (id int);\n").expect("ours");
    assert_eq!(pgmig(&["migrate", "validate", "--dir", &d.arg()]).status.code(), Some(1));
    let out = pgmig(&["migrate", "rebase", "20261102000000", "--dir", &d.arg(), "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let v = parse(&out);
    let to = v["moved"][0]["to"].as_str().expect("to").to_owned();
    assert!(to.ends_with("_create_b.sql") && Path::new(&to).is_file(), "{v}");
    assert!(!d.0.join(M2).exists(), "the old name is gone");
    let files = sql_files(&d.0);
    assert_eq!(files.last().map(String::as_str), Path::new(&to).file_name().and_then(|n| n.to_str()));
    validates(&d);
    let out = pgmig(&["migrate", "rebase", &to, "--dir", &d.arg()]);
    assert!(stderr(&out).contains("nothing to move"), "{}", stderr(&out));
}
