//! `pgmig.toml`: `--config`, `--env`, `--var`, precedence over it by flags, and `config validate`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

use crate::support::{TestDb, shadow_arg};

/// A temporary project directory, removed on drop.
struct Project(PathBuf);

impl Project {
    fn new(files: &[(&str, &str)]) -> Project {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig_cli_config_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        let p = Project(path);
        for (name, text) in files {
            p.write(name, text);
        }
        p
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create dir");
        }
        std::fs::write(path, text).expect("write file");
    }

    /// Runs `pgmig` in this directory.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pgmig"));
        cmd.args(args).current_dir(&self.0).stdin(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("run pgmig")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(out: &Output) -> String {
    assert!(out.status.success(), "{}", stderr(out));
    stdout(out)
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    format!("{s:?}")
}

const A: &str = "CREATE TABLE a (id int);\n";
const B: &str = "CREATE TABLE a (id int);\nCREATE TABLE b (id int);\n";
const C: &str = "CREATE TABLE a (id int);\nCREATE TABLE c (id int);\n";
const CREATE_B: &str = "CREATE TABLE public.b (\n    id integer\n);\n";
const CREATE_C: &str = "CREATE TABLE public.c (\n    id integer\n);\n";

/// `a.sql`, `b.sql`, `c.sql` and a pgmig.toml: top level diffs a → b, `[env.c]` a → c.
fn project(shadow: &str, extra: &str) -> Project {
    let toml = format!("shadow = {}\nfrom = \"a.sql\"\nto = \"b.sql\"\n{extra}\n[env.c]\nto = \"c.sql\"\n", toml_str(shadow));
    Project::new(&[("a.sql", A), ("b.sql", B), ("c.sql", C), ("pgmig.toml", &toml)])
}

#[test]
fn diff_runs_from_the_config_file_alone() {
    let (shadow, _keep) = shadow_arg();
    let p = project(&shadow, "");
    assert_eq!(ok(&p.run(&["schema", "diff"])), CREATE_B);
    // The environment's `to` wins over the top level's.
    assert_eq!(ok(&p.run(&["--env", "c", "schema", "diff"])), CREATE_C);
    // `--env` and `--config` are global: they may follow the subcommand.
    assert_eq!(ok(&p.run(&["schema", "diff", "--env", "c"])), CREATE_C);
    // `to` is also the source of `schema inspect` and `schema validate`.
    assert_eq!(ok(&p.run(&["--env", "c", "schema", "inspect"])), "CREATE TABLE public.a (\n    id integer\n);\n".to_owned() + CREATE_C);
    let v = p.run(&["schema", "validate"]);
    assert!(v.status.success() && v.stdout.is_empty() && v.stderr.is_empty(), "{}", stderr(&v));
}

#[test]
fn diff_skip_comes_from_the_config_file() {
    let (shadow, _keep) = shadow_arg();
    let p = project(&shadow, "[diff]\nskip = [\"drop_table\"]\n");
    p.write("pgmig.toml", &format!("shadow = {}\nfrom = \"b.sql\"\nto = \"a.sql\"\n[diff]\nskip = [\"drop_table\"]\n", toml_str(&shadow)));
    let out = p.run(&["schema", "diff"]);
    assert_eq!(ok(&out), "");
    assert_eq!(stderr(&out), "skipped drop_table public.b (policy)\n");
    // A --skip flag replaces the list.
    let out = p.run(&["schema", "diff", "--skip", "drop_column"]);
    assert_eq!(ok(&out), "DROP TABLE public.b;\n");
    assert_eq!(stderr(&out), "");
    // An unknown kind is a config error naming the key.
    p.write("pgmig.toml", "[diff]\nskip = [\"drop_tabel\"]\n");
    let out = p.run(&["schema", "diff"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("pgmig.toml: diff.skip[0]: unknown kind of change \"drop_tabel\""), "{}", stderr(&out));
}

#[test]
fn flags_win_over_the_config_file() {
    let (shadow, _keep) = shadow_arg();
    let p = project(&shadow, "exclude = [\"public.c\"]\nformat = \"json\"\n");
    // --to over `[env.c] to`, and --format over `format`.
    assert_eq!(ok(&p.run(&["--env", "c", "schema", "diff", "--to", "b.sql", "--format", "text"])), CREATE_B);
    // The config's `format` applies without the flag.
    let json: Value = serde_json::from_str(&ok(&p.run(&["schema", "diff"]))).expect("json");
    assert_eq!(json["sql"], CREATE_B);
    // The config's `exclude` hides c; an --exclude flag replaces the whole list.
    assert_eq!(ok(&p.run(&["--env", "c", "schema", "diff", "--format", "text"])), "");
    assert_eq!(ok(&p.run(&["--env", "c", "schema", "diff", "--format", "text", "--exclude", "public.zzz"])), CREATE_C);
}

#[test]
fn the_configured_revisions_schema_is_never_managed() {
    let (shadow, _keep) = shadow_arg();
    let toml = |extra: &str| format!("shadow = {}\nfrom = \"a.sql\"\nto = \"ops.sql\"\n{extra}", toml_str(&shadow));
    let ops = "CREATE TABLE a (id int);\nCREATE SCHEMA ops;\nCREATE TABLE ops.t (id int);\nCREATE SCHEMA pgmig;\nCREATE TABLE pgmig.t (id int);\n";
    let p = Project::new(&[("a.sql", A), ("ops.sql", ops), ("pgmig.toml", &toml("[migrations]\nrevisions_schema = \"ops\"\n"))]);
    // `ops` is left out; `pgmig` is an ordinary schema now.
    let out = ok(&p.run(&["schema", "diff"]));
    assert!(!out.contains("ops") && out.contains("CREATE TABLE pgmig.t"), "{out}");
    // By default `pgmig` is the one left out.
    p.write("pgmig.toml", &toml(""));
    let out = ok(&p.run(&["schema", "diff"]));
    assert!(out.contains("CREATE TABLE ops.t") && !out.contains("pgmig"), "{out}");
}

#[test]
fn config_path_and_variables() {
    let (shadow, _keep) = shadow_arg();
    let files = Project::new(&[("a.sql", A), ("b.sql", B), ("c.sql", C)]);
    let dir = files.0.to_string_lossy().into_owned();
    let toml = format!("shadow = {}\nfrom = \"${{DIR}}/a.sql\"\nto = \"${{DIR}}/${{TO}}.sql\"\n", toml_str(&shadow));
    let elsewhere = Project::new(&[("conf/custom.toml", &toml)]);
    let config = elsewhere.0.join("conf/custom.toml").to_string_lossy().into_owned();
    // No pgmig.toml in the current directory: --config names the file.
    let out = files.run_with(&["--config", &config, "--var", &format!("DIR={dir}"), "schema", "diff"], &[("TO", "c")]);
    assert_eq!(ok(&out), CREATE_C);
    // --var wins over the environment.
    let out = files.run_with(&["--config", &config, "--var", &format!("DIR={dir}"), "--var", "TO=b", "schema", "diff"], &[("TO", "c")]);
    assert_eq!(ok(&out), CREATE_B);
    // Undefined: an error naming the key and the variable.
    let out = files.run_with(&["--config", &config, "--var", &format!("DIR={dir}"), "schema", "diff"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("custom.toml: to: undefined variable `TO`"), "{}", stderr(&out));
}

#[test]
fn a_missing_value_is_a_usage_error_naming_the_flag_and_the_key() {
    let p = Project::new(&[("pgmig.toml", "shadow = \"pgmem\"\n")]);
    let out = p.run(&["schema", "diff", "--to", "x.sql"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("missing --from: pass it, or set `from` in pgmig.toml"), "{}", stderr(&out));
    let out = p.run(&["schema", "apply", "--to", "x.sql", "--format", "json"]);
    assert_eq!(out.status.code(), Some(2));
    let err: Value = serde_json::from_str(&stdout(&out)).expect("json error");
    assert_eq!(err["code"], "usage");
    assert!(err["message"].as_str().is_some_and(|m| m.contains("--url") && m.contains("`url`")), "{err}");
    let out = p.run(&["migrate", "hash"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("missing --dir: pass it, or set `migrations.dir`"), "{}", stderr(&out));
    // Without any config file too.
    let bare = Project::new(&[]);
    assert_eq!(bare.run(&["schema", "inspect"]).status.code(), Some(2));
}

#[test]
fn migrations_dir_comes_from_the_config_file() {
    let p = Project::new(&[("pgmig.toml", "[migrations]\ndir = \"db\"\n[env.other.migrations]\ndir = \"none\"\n"), ("db/1_a.sql", A)]);
    let out = p.run(&["migrate", "hash"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(p.0.join("db/pgmig.sum").is_file());
    assert_eq!(p.run(&["--env", "other", "migrate", "hash"]).status.code(), Some(1));
}

#[test]
fn config_errors_and_unknown_environments() {
    let p = Project::new(&[("pgmig.toml", "url = \"host=h password=hunter2\"\n[env.dev]\nurll = \"hunter2\"\n")]);
    let out = p.run(&["schema", "diff"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("pgmig.toml: env.dev.urll: unknown key"), "{}", stderr(&out));
    assert!(!stderr(&out).contains("hunter2"));

    let p = Project::new(&[("pgmig.toml", "[env.dev]\nurl = \"u\"\n")]);
    let out = p.run(&["--env", "prod", "schema", "diff"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no environment `prod` (--env); defined: dev"), "{}", stderr(&out));

    // --env without a config file.
    let bare = Project::new(&[]);
    let out = bare.run(&["--env", "dev", "schema", "diff"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--env needs a config file"), "{}", stderr(&out));
}

#[test]
fn config_validate() {
    let good = "shadow = \"pgmem\"\n[env.prod]\nurl = \"host=db password=${PW}\"\n";
    let p = Project::new(&[("pgmig.toml", good)]);
    let out = p.run(&["config", "validate"]);
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(), "{}", stderr(&out));
    // With --env the environment's placeholders must resolve; secrets stay out of the output.
    let out = p.run_with(&["config", "validate", "--env", "prod"], &[("PW", "hunter2")]);
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(), "{}", stderr(&out));
    let out = p.run(&["config", "validate", "--env", "prod"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr(&out), "error: pgmig.toml: env.prod.url: undefined variable `PW` (set it in the environment or pass --var PW=...)\n");
    let out = p.run(&["config", "validate", "--env", "prod", "--var", "PW=x", "--format", "json"]);
    assert_eq!(ok(&out).trim(), "{\n  \"diagnostics\": []\n}");

    let bad = "shadoww = \"pgmem\"\n[env.prod]\nformat = \"yaml\"\n[env.prod.migrations]\ndirr = \"m\"\n";
    let p = Project::new(&[("custom.toml", bad)]);
    let out = p.run(&["--config", "custom.toml", "config", "validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_str(&stdout(&out)).expect("json");
    let messages: Vec<&str> =
        v["diagnostics"].as_array().expect("diagnostics").iter().map(|d| d["message"].as_str().expect("message")).collect();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(messages[0], "custom.toml: env.prod.format: expected \"text\" or \"json\"");
    assert!(
        messages[1].starts_with("custom.toml: env.prod.migrations.dirr: unknown key (expected one of `dir`, `revisions_schema`, "),
        "{messages:?}"
    );
    assert!(messages[2].starts_with("custom.toml: shadoww: unknown key"), "{messages:?}");
    assert_eq!(v["diagnostics"][0]["code"], "config");
    assert_eq!(v["diagnostics"][0]["file"], "custom.toml");

    // A syntax error has a position.
    let p = Project::new(&[("pgmig.toml", "shadow = \"pgmem\"\nurl = \n")]);
    let out = p.run(&["config", "validate", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(v["diagnostics"][0]["line"], 2, "{v}");

    // No file to check.
    let bare = Project::new(&[]);
    let out = bare.run(&["config", "validate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).starts_with("error: pgmig.toml:"), "{}", stderr(&out));
    assert!(!Path::new(&bare.0.join("pgmig.toml")).exists());
}

#[test]
fn apply_takes_its_target_from_the_config_file_and_names_it() {
    let (shadow, _keep) = shadow_arg();
    let db = TestDb::new();
    db.connect().batch(A).expect("load target");
    let toml = format!(
        "shadow = {}\nurl = {}\nto = \"b.sql\"\n[env.stage]\nurl = \"${{DATABASE_URL}}\"\n",
        toml_str(&shadow),
        toml_str(&db.conninfo)
    );
    let p = Project::new(&[("b.sql", B), ("pgmig.toml", &toml)]);
    let shown = pgmig::config::redact(&db.conninfo);

    let out = p.run(&["schema", "apply", "--dry-run"]);
    assert_eq!(ok(&out), CREATE_B);
    assert_eq!(stderr(&out), format!("Target: {shown} (from `url` in pgmig.toml)\n"));

    let out = p.run_with(&["--env", "stage", "schema", "apply", "--dry-run", "--format", "json"], &[("DATABASE_URL", &db.conninfo)]);
    let json: Value = serde_json::from_str(&ok(&out)).expect("json");
    assert_eq!(json["target"], serde_json::json!({"display": shown, "source": "`env.stage.url` in pgmig.toml"}));
    assert_eq!(json["sql"], CREATE_B);

    // A flag names itself; the plan was never applied.
    let out = p.run(&["schema", "apply", "--dry-run", "--url", &db.conninfo]);
    assert!(stderr(&out).ends_with("(from --url)\n"), "{}", stderr(&out));
    let rows = db.connect().query("SELECT count(*) FROM pg_tables WHERE tablename = 'b'").expect("query");
    assert_eq!(rows[0][0].as_deref(), Some("0"));
}

#[test]
fn schema_scope_comes_from_the_config_file() {
    let (shadow, _keep) = shadow_arg();
    let p = project(&shadow, "schema = \"public\"\n");
    // With a schema scope, diff prints names unqualified.
    assert_eq!(ok(&p.run(&["schema", "diff"])), "CREATE TABLE b (\n    id integer\n);\n");
}

#[test]
fn a_shadow_flag_wins_over_the_config_file() {
    let (shadow, _keep) = shadow_arg();
    // Nothing listens on port 1.
    let p = project("host=127.0.0.1 port=1 user=nobody", "");
    assert_eq!(p.run(&["schema", "diff"]).status.code(), Some(1));
    assert_eq!(ok(&p.run(&["schema", "diff", "--shadow", &shadow])), CREATE_B);
}

#[test]
fn a_config_shadow_turns_on_migrate_validate_replay() {
    let (shadow, _keep) = shadow_arg();
    let files = [("db/1_a.sql", A), ("db/2_b.sql", "CREATE TABLE b (id int);\n")];
    let with = Project::new(&files);
    with.write("pgmig.toml", &format!("shadow = {}\n[migrations]\ndir = \"db\"\n", toml_str(&shadow)));
    let without = Project::new(&files);
    without.write("pgmig.toml", "[migrations]\ndir = \"db\"\n");
    for (p, replayed) in [(&with, true), (&without, false)] {
        assert!(p.run(&["migrate", "hash"]).status.success());
        let json: Value = serde_json::from_str(&ok(&p.run(&["migrate", "validate", "--format", "json"]))).expect("json");
        assert_eq!(json["replayed"], replayed, "{json}");
    }
}

#[test]
fn an_environments_include_and_exclude_win_over_the_top_level() {
    let (shadow, _keep) = shadow_arg();
    // The top level leaves c out; `[env.wide]` brings it back, once with each key.
    for (key, top, env) in [("exclude", "public.c", "public.zzz"), ("include", "public.a", "public")] {
        let toml = format!(
            "shadow = {}\nfrom = \"a.sql\"\nto = \"c.sql\"\n{key} = [\"{top}\"]\n[env.wide]\n{key} = [\"{env}\"]\n",
            toml_str(&shadow)
        );
        let p = Project::new(&[("a.sql", A), ("c.sql", C), ("pgmig.toml", &toml)]);
        assert_eq!(ok(&p.run(&["schema", "diff"])), "", "{key}");
        assert_eq!(ok(&p.run(&["--env", "wide", "schema", "diff"])), CREATE_C, "{key}");
    }
}

#[test]
fn clean_takes_its_target_from_the_config_file_and_names_it() {
    let db = TestDb::new();
    db.connect().batch(A).expect("load target");
    let p = Project::new(&[("pgmig.toml", &format!("url = {}\n", toml_str(&db.conninfo)))]);
    let out = p.run(&["schema", "clean", "--dry-run"]);
    assert!(!ok(&out).is_empty());
    let shown = pgmig::config::redact(&db.conninfo);
    assert!(stderr(&out).contains(&format!("Target: {shown} (from `url` in pgmig.toml), database ")), "{}", stderr(&out));
}
