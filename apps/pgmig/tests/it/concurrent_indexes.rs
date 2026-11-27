//! `--concurrent-indexes` and `[diff] concurrent_indexes`: indexes built and dropped without
//! blocking writes, as statements outside a transaction, and `migrate diff` writing one file per
//! unit.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

const FROM: &str = "CREATE TABLE t (id int, a int);\n";
const TO: &str = "CREATE TABLE t (id int, a int);\nCREATE INDEX t_a_idx ON t (a);\n";
const NOTX: &str = "-- Runs outside a transaction: Postgres refuses it inside one.\n-- pgmig:notx\n";

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A temporary project directory, removed on drop.
struct Project(PathBuf);

impl Project {
    fn new(files: &[(&str, &str)]) -> Project {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig_cli_concurrent_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        for (name, text) in files {
            std::fs::write(path.join(name), text).expect("write file");
        }
        Project(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pgmig")).args(args).current_dir(&self.0).stdin(Stdio::null()).output().expect("run pgmig")
    }

    fn arg(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }

    fn sql_files(&self, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.0.join(dir))
            .map(|rd| {
                rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".sql")).collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn indexes(db: &TestDb) -> Vec<(String, bool)> {
    let rows = db
        .connect()
        .query(
            "SELECT i.relname, x.indisvalid FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
             JOIN pg_namespace n ON n.oid = i.relnamespace WHERE n.nspname = 'public' ORDER BY 1",
        )
        .expect("query indexes");
    rows.into_iter().map(|r| (r[0].clone().expect("name"), r[1].as_deref() == Some("t"))).collect()
}

#[test]
fn schema_diff_prints_concurrent_statements_with_the_flag_only() {
    let (shadow, _keep) = shadow_arg();
    let (from, to) = (SqlFile::new(FROM), SqlFile::new(TO));
    let args = ["schema", "diff", "--from", &from.path(), "--to", &to.path(), "--shadow", &shadow];
    let out = pgmig(&args);
    assert_eq!(text(&out.stdout), "CREATE INDEX t_a_idx ON public.t USING btree (a);\n", "{}", text(&out.stderr));

    let out = pgmig(&[&args[..], &["--concurrent-indexes"]].concat());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), format!("{NOTX}CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a);\n"));

    let out = pgmig(&[&args[..], &["--concurrent-indexes", "--format", "json"]].concat());
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["plan"]["non_transactional"], serde_json::json!([0]));
    assert_eq!(json["plan"]["statements"][0], "CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a)");
    // Without it the plan JSON has no such field.
    let out = pgmig(&[&args[..], &["--format", "json"]].concat());
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(json["plan"].get("non_transactional").is_none(), "{json}");
}

#[test]
fn the_config_key_turns_it_on_and_the_flags_and_environments_override_it() {
    let (shadow, _keep) = shadow_arg();
    let shadow_line = format!("shadow = {shadow:?}\nfrom = \"a.sql\"\nto = \"b.sql\"\n");
    let p = Project::new(&[
        ("a.sql", FROM),
        ("b.sql", TO),
        ("pgmig.toml", &format!("{shadow_line}[diff]\nconcurrent_indexes = true\n[env.plain.diff]\nconcurrent_indexes = false\n")),
    ]);
    let concurrent = |out: &Output| text(&out.stdout).contains("CREATE INDEX CONCURRENTLY");
    let out = p.run(&["schema", "diff"]);
    assert!(out.status.success() && concurrent(&out), "{}", text(&out.stderr));
    // An environment overrides the top level, and --no-concurrent-indexes the config.
    assert!(!concurrent(&p.run(&["schema", "diff", "--env", "plain"])));
    assert!(!concurrent(&p.run(&["schema", "diff", "--no-concurrent-indexes"])));
    // A flag wins over the environment's value.
    assert!(concurrent(&p.run(&["schema", "diff", "--env", "plain", "--concurrent-indexes"])));

    // Wrong types name the key, at the top level and in an environment.
    std::fs::write(p.0.join("pgmig.toml"), "[diff]\nconcurrent_indexes = \"yes\"\n").expect("write");
    let out = p.run(&["schema", "diff"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("pgmig.toml: diff.concurrent_indexes: expected a boolean"), "{}", text(&out.stderr));
    std::fs::write(p.0.join("pgmig.toml"), "[env.prod.diff]\nconcurrent_indexes = 1\n").expect("write");
    let out = p.run(&["config", "validate", "--env", "prod"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("env.prod.diff.concurrent_indexes: expected a boolean"), "{}", text(&out.stderr));
}

#[test]
fn schema_apply_builds_the_index_outside_a_transaction() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    db.connect().batch(FROM).expect("load target");
    let to = SqlFile::new("CREATE TABLE t (id int, a int, b int);\nCREATE INDEX t_a_idx ON t (a);\n");
    let args = ["schema", "apply", "--url", &db.conninfo, "--to", &to.path(), "--shadow", &shadow, "--concurrent-indexes", "--verify"];
    let out = pgmig(&[&args[..], &["--auto-approve"]].concat());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(err.contains("applied 2 statements: 1 in 1 transaction(s), 1 outside a transaction"), "{err}");
    assert_eq!(indexes(&db), [("t_a_idx".into(), true)]);

    // Again: nothing to do.
    let out = pgmig(&[&args[..], &["--auto-approve"]].concat());
    assert!(text(&out.stderr).ends_with("no changes\n"), "{}", text(&out.stderr));

    // JSON reports how many statements ran outside a transaction.
    let db = TestDb::new();
    db.connect().batch(FROM).expect("load target");
    let to = SqlFile::new(TO);
    let args = ["schema", "apply", "--url", &db.conninfo, "--to", &to.path(), "--shadow", &shadow, "--concurrent-indexes"];
    let out = pgmig(&[&args[..], &["--auto-approve", "--format", "json"]].concat());
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["report"], serde_json::json!({"batches": 0, "statements": 1, "non_transactional": 1}), "{json}");
}

#[test]
fn schema_apply_failure_names_the_leftover_index() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE t (id int, a int); INSERT INTO t VALUES (1, 7), (2, 7);").expect("load target");
    let to = SqlFile::new("CREATE TABLE t (id int, a int);\nCREATE UNIQUE INDEX t_a_idx ON t (a);\n");
    let out = pgmig(&[
        "schema",
        "apply",
        "--url",
        &db.conninfo,
        "--to",
        &to.path(),
        "--shadow",
        &shadow,
        "--concurrent-indexes",
        "--auto-approve",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("SQLSTATE 23505") && err.contains("DROP INDEX CONCURRENTLY public.t_a_idx"), "{err}");
}

/// `migrate diff` of `to` in `dir` (under `p`), with the flag.
fn migrate_diff(p: &Project, shadow: &str, name: &str, to: &str, extra: &[&str]) -> Output {
    let (dir, to) = (p.arg("migrations"), p.arg(to));
    p.run(&[&["migrate", "diff", name, "--dir", &dir, "--to", &to, "--shadow", shadow][..], extra].concat())
}

#[test]
fn migrate_diff_writes_a_non_transactional_file_that_migrate_apply_runs() {
    let (shadow, _keep) = shadow_arg();
    let p = Project::new(&[("a.sql", FROM), ("b.sql", TO)]);
    let out = migrate_diff(&p, &shadow, "init", "a.sql", &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = migrate_diff(&p, &shadow, "index", "b.sql", &["--concurrent-indexes", "--verify"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    // One file: the plan is a single unit, so it is reported as before.
    assert!(text(&out.stderr).contains("verified:") && text(&out.stderr).contains("(1 statement)"), "{}", text(&out.stderr));
    let files = p.sql_files("migrations");
    assert_eq!(files.len(), 2, "{files:?}");
    let written = std::fs::read_to_string(p.0.join("migrations").join(&files[1])).expect("read");
    assert!(written.starts_with("-- pgmig:txmode none\n") && written.contains("CREATE INDEX CONCURRENTLY t_a_idx"), "{written}");

    let db = TestDb::new();
    let out = pgmig(&["migrate", "apply", "--url", &db.conninfo, "--dir", &p.arg("migrations")]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(indexes(&db), [("t_a_idx".into(), true)]);
}

#[test]
fn migrate_diff_lists_every_file_it_writes() {
    let (shadow, _keep) = shadow_arg();
    let p = Project::new(&[("a.sql", FROM), ("b.sql", "CREATE TABLE t (id int, a int, b int);\nCREATE INDEX t_b_idx ON t (b);\n")]);
    migrate_diff(&p, &shadow, "init", "a.sql", &[]);
    let out = migrate_diff(&p, &shadow, "more", "b.sql", &["--concurrent-indexes"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let files = p.sql_files("migrations");
    assert_eq!(files.len(), 3, "{files:?}");
    let err = text(&out.stderr);
    assert!(err.contains("wrote 2 files, one per unit of the plan"), "{err}");
    for f in &files[1..] {
        assert!(err.contains(&format!("migrations{}{f}", std::path::MAIN_SEPARATOR)), "{err}");
    }

    // JSON: `file` and `version` are the first file, `files` all of them.
    let p2 = Project::new(&[("a.sql", FROM), ("b.sql", "CREATE TABLE t (id int, a int, b int);\nCREATE INDEX t_b_idx ON t (b);\n")]);
    migrate_diff(&p2, &shadow, "init", "a.sql", &[]);
    let out = migrate_diff(&p2, &shadow, "more", "b.sql", &["--concurrent-indexes", "--format", "json"]);
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    let listed: Vec<&str> = json["files"].as_array().expect("files").iter().filter_map(|f| f["path"].as_str()).collect();
    assert_eq!(listed.len(), 2, "{json}");
    assert_eq!(json["file"], listed[0]);
    assert!(Path::new(listed[1]).is_file());
    assert_eq!(json["plan"]["non_transactional"], serde_json::json!([1]));
}
