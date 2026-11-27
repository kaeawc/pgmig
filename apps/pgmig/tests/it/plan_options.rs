//! `--if-exists`, `--cascade` and `[diff] if_exists`, `[diff] cascade`: `IF [NOT] EXISTS` and
//! `CASCADE` on the statements a plan generates.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

const FROM: &str = "CREATE TABLE t (id int, extra text);\nCREATE TABLE gone (id int);\nCREATE INDEX t_id_idx ON t (id);\n";
const TO: &str = "CREATE TABLE t (id int, added text);\nCREATE TABLE fresh (id int);\n";

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A temporary project directory, removed on drop.
struct Project(PathBuf);

impl Project {
    fn new(files: &[(&str, &str)]) -> Project {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path =
            std::env::temp_dir().join(format!("pgmig_cli_plan_options_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        for (name, text) in files {
            std::fs::write(path.join(name), text).expect("write file");
        }
        Project(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pgmig")).args(args).current_dir(&self.0).stdin(Stdio::null()).output().expect("run pgmig")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn diff(extra: &[&str]) -> String {
    let (shadow, _keep) = shadow_arg();
    let (from, to) = (SqlFile::new(FROM), SqlFile::new(TO));
    let args = ["schema", "diff", "--from", &from.path(), "--to", &to.path(), "--shadow", &shadow];
    let out = pgmig(&[&args[..], extra].concat());
    assert!(out.status.success(), "{extra:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

#[test]
fn schema_diff_prints_the_clauses_with_the_flags_only() {
    let plain = diff(&[]);
    assert!(plain.contains("CREATE TABLE public.fresh (") && plain.contains("DROP TABLE public.gone;"), "{plain}");
    assert!(!plain.contains("EXISTS") && !plain.contains("CASCADE"), "{plain}");

    let ie = diff(&["--if-exists"]);
    assert!(ie.contains("CREATE TABLE IF NOT EXISTS public.fresh (") && ie.contains("DROP TABLE IF EXISTS public.gone;"), "{ie}");
    assert!(ie.contains("ALTER TABLE public.t ADD COLUMN IF NOT EXISTS added text;"), "{ie}");
    assert!(
        ie.contains("ALTER TABLE public.t DROP COLUMN IF EXISTS extra;") && ie.contains("DROP INDEX IF EXISTS public.t_id_idx;"),
        "{ie}"
    );
    assert!(!ie.contains("CASCADE"), "{ie}");

    let cascade = diff(&["--cascade"]);
    assert!(
        cascade.contains("DROP TABLE public.gone CASCADE;") && cascade.contains("ALTER TABLE public.t DROP COLUMN extra CASCADE;"),
        "{cascade}"
    );
    assert!(cascade.contains("DROP INDEX public.t_id_idx CASCADE;") && !cascade.contains("EXISTS"), "{cascade}");

    let both = diff(&["--if-exists", "--cascade"]);
    assert!(both.contains("DROP TABLE IF EXISTS public.gone CASCADE;"), "{both}");
    // The last of a flag and its negation wins.
    assert_eq!(diff(&["--if-exists", "--no-if-exists", "--cascade", "--no-cascade"]), plain);
}

#[test]
fn the_config_keys_turn_them_on_and_the_flags_and_environments_override_them() {
    let (shadow, _keep) = shadow_arg();
    let shadow_line = format!("shadow = {shadow:?}\nfrom = \"a.sql\"\nto = \"b.sql\"\n");
    let p = Project::new(&[
        ("a.sql", FROM),
        ("b.sql", TO),
        (
            "pgmig.toml",
            &format!("{shadow_line}[diff]\nif_exists = true\ncascade = true\n[env.plain.diff]\nif_exists = false\ncascade = false\n"),
        ),
    ]);
    let ie = |out: &Output| text(&out.stdout).contains("DROP TABLE IF EXISTS");
    let casc = |out: &Output| {
        text(&out.stdout).contains("DROP TABLE public.gone CASCADE") || text(&out.stdout).contains("EXISTS public.gone CASCADE")
    };
    let out = p.run(&["schema", "diff"]);
    assert!(out.status.success() && ie(&out) && casc(&out), "{}", text(&out.stderr));
    let out = p.run(&["schema", "diff", "--env", "plain"]);
    assert!(!ie(&out) && !casc(&out));
    let out = p.run(&["schema", "diff", "--no-if-exists"]);
    assert!(!ie(&out) && casc(&out));
    let out = p.run(&["schema", "diff", "--no-cascade"]);
    assert!(ie(&out) && !casc(&out));
    let out = p.run(&["schema", "diff", "--env", "plain", "--if-exists", "--cascade"]);
    assert!(ie(&out) && casc(&out));

    for key in ["if_exists", "cascade"] {
        std::fs::write(p.0.join("pgmig.toml"), format!("[diff]\n{key} = \"yes\"\n")).expect("write");
        let out = p.run(&["schema", "diff"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(text(&out.stderr).contains(&format!("pgmig.toml: diff.{key}: expected a boolean")), "{}", text(&out.stderr));
        std::fs::write(p.0.join("pgmig.toml"), format!("[env.prod.diff]\n{key} = 1\n")).expect("write");
        let out = p.run(&["config", "validate", "--env", "prod"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(text(&out.stderr).contains(&format!("env.prod.diff.{key}: expected a boolean")), "{}", text(&out.stderr));
    }
}

#[test]
fn schema_apply_runs_the_statements_and_leaves_nothing_to_change() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    db.connect().batch(FROM).expect("load target");
    let to = SqlFile::new(TO);
    let args = ["schema", "apply", "--url", &db.conninfo, "--to", &to.path(), "--shadow", &shadow, "--if-exists", "--cascade", "--verify"];
    let out = pgmig(&[&args[..], &["--auto-approve"]].concat());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = pgmig(&[&args[..], &["--auto-approve"]].concat());
    assert!(text(&out.stderr).ends_with("no changes\n"), "{}", text(&out.stderr));
}

#[test]
fn schema_plan_file_carries_the_options_and_migrate_diff_writes_the_clauses() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    db.connect().batch(FROM).expect("load target");
    let to = SqlFile::new(TO);
    let out = pgmig(&["schema", "plan", "--url", &db.conninfo, "--to", &to.path(), "--shadow", &shadow, "--if-exists", "--cascade"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let file: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("plan file: {}", text(&out.stdout)));
    assert_eq!(file["options"]["if_exists"], json!(true));
    assert_eq!(file["options"]["cascade"], json!(true));
    let sql = file["sql"].as_str().expect("sql");
    assert!(sql.contains("DROP TABLE IF EXISTS public.gone CASCADE;"), "{sql}");

    let p = Project::new(&[("a.sql", FROM), ("b.sql", TO)]);
    let dir = p.0.join("migrations").to_string_lossy().into_owned();
    let (a, b) = (p.0.join("a.sql").to_string_lossy().into_owned(), p.0.join("b.sql").to_string_lossy().into_owned());
    for (name, to, extra) in [("init", &a, vec![]), ("change", &b, vec!["--if-exists", "--cascade"])] {
        let args = ["migrate", "diff", name, "--dir", &dir, "--to", to, "--shadow", &shadow];
        let out = p.run(&[&args[..], &extra[..]].concat());
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("dir").filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.retain(|f| f.extension().is_some_and(|x| x == "sql"));
    files.sort();
    let written = std::fs::read_to_string(files.last().expect("a file")).expect("read");
    assert!(written.contains("DROP TABLE IF EXISTS public.gone CASCADE;"), "{written}");
}
