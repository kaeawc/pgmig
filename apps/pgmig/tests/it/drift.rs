//! `drift check`: text and JSON output, exit codes, flags and the config file.

use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

/// A temporary directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig_cli_drift_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn parse(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

const USERS: &str = "CREATE TABLE users (id int PRIMARY KEY, name text);\nCREATE INDEX users_name ON users (name);\n";
const POSTS: &str = "CREATE TABLE posts (id int PRIMARY KEY);\n";

/// A hashed migration directory applied to a new database.
fn applied(files: &[(&str, &str)]) -> (Dir, TestDb) {
    let d = Dir::new(files);
    let out = pgmig(&["migrate", "hash", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let db = TestDb::new();
    let out = pgmig(&["migrate", "apply", "--url", &db.conninfo, "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    (d, db)
}

#[test]
fn no_drift_exits_0_and_drift_exits_3() {
    let (shadow, _keep) = shadow_arg();
    let (d, db) = applied(&[("1_users.sql", USERS)]);
    let dir = d.arg();
    let check = |extra: &[&str]| {
        let mut args = vec!["drift", "check", "--url", &db.conninfo, "--dir", &dir, "--shadow", &shadow];
        args.extend_from_slice(extra);
        pgmig(&args)
    };
    let out = check(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).is_empty() && stderr(&out).contains("no drift"), "{}", stderr(&out));

    db.connect().batch("ALTER TABLE users ADD COLUMN email text; DROP INDEX users_name").expect("drift by hand");
    let out = check(&[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let sql = stdout(&out);
    assert!(sql.contains("DROP COLUMN email") && sql.contains("CREATE INDEX users_name"), "{sql}");
    assert!(stderr(&out).contains("drift: 2 changes"), "{}", stderr(&out));

    let out = check(&["--format", "json"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let json = parse(&out);
    assert_eq!(json["drifted"], true);
    assert_eq!(json["target"]["source"], "--url");
    assert_eq!(json["history"]["replayed"][0], "1_users.sql");
    assert!(json["sql"].as_str().is_some_and(|s| s.contains("DROP COLUMN email")), "{json}");

    // `--skip` and `--exclude` as `schema diff` takes them.
    let out = check(&["--exclude", "public.users"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    // The printed plan brings the database back.
    db.connect().batch(&sql).expect("run the plan");
    let out = check(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn pending_files_and_history_problems() {
    let (shadow, _keep) = shadow_arg();
    let (d, db) = applied(&[("1_users.sql", USERS)]);
    std::fs::write(d.0.join("2_posts.sql"), POSTS).expect("write");
    let out = pgmig(&["migrate", "hash", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let check = || pgmig(&["drift", "check", "--url", &db.conninfo, "--dir", &d.arg(), "--shadow", &shadow]);
    let out = check();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("pending 2_posts.sql (not applied; not drift)"), "{}", stderr(&out));

    // An applied file edited afterwards.
    std::fs::write(d.0.join("1_users.sql"), format!("{USERS}-- edited\n")).expect("edit");
    let out = pgmig(&["migrate", "hash", "--dir", &d.arg()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let out = check();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("problem: 1_users.sql was edited after it was applied") && err.contains("drift: 1 history problem"), "{err}");
}

#[test]
fn to_sql_or_another_database_and_the_config_default() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    db.connect().batch(USERS).expect("users");
    let desired = SqlFile::new(USERS);
    let out = pgmig(&["drift", "check", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--format", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(parse(&out)["history"], Value::Null);

    let other = TestDb::new();
    other.connect().batch(&format!("{USERS}{POSTS}")).expect("other");
    let out = pgmig(&["drift", "check", "--url", &db.conninfo, "--to", &other.conninfo, "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stdout(&out).contains("CREATE TABLE public.posts"), "{}", stdout(&out));

    // --dir and --to together are a usage error; so is neither, without a config file.
    let out = pgmig(&["drift", "check", "--url", &db.conninfo, "--to", &desired.path(), "--dir", "m"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let empty = Dir::new(&[]);
    let toml = empty.0.join("none.toml");
    std::fs::write(&toml, "").expect("write");
    let out = pgmig(&["drift", "check", "--config", toml.to_str().expect("utf-8"), "--url", &db.conninfo]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("missing --dir or --to"), "{}", stderr(&out));

    // `[migrations] dir` from the config file: nothing applied, so the table is drift.
    let (m, _applied) = applied(&[("1_users.sql", USERS)]);
    let cfg = Dir::new(&[(
        "pgmig.toml",
        &format!("url = {:?}\nshadow = {:?}\nto = {:?}\n[migrations]\ndir = {:?}\n", db.conninfo, shadow, desired.path(), m.arg()),
    )]);
    let out = pgmig(&["drift", "check", "--config", cfg.0.join("pgmig.toml").to_str().expect("utf-8")]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("no revisions table"), "{}", stderr(&out));
    // A flag wins: `--to` is compared with instead.
    let out = pgmig(&["drift", "check", "--config", cfg.0.join("pgmig.toml").to_str().expect("utf-8"), "--to", &desired.path()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}
