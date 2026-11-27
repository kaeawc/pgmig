//! `--edit` on `schema apply` and `migrate diff`: the editor's saved text is what runs or is
//! written. The editor is a shell script; pgmig refuses to run one without a terminal, so these
//! tests run it under a pseudo-terminal (`script`). Unix only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::support::{SqlFile, TestDb, pgmig, shadow_arg};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A temp directory holding an editor script (`editor.sh`) and anything a test writes, removed on
/// drop. The script records the file it was given in `seen`.
struct Editor(PathBuf);

impl Editor {
    fn new(body: &str) -> Editor {
        use std::os::unix::fs::PermissionsExt as _;
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("pgmig_cli_edit_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let script = format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n{body}\n", dir.join("seen").display());
        let path = dir.join("editor.sh");
        std::fs::write(&path, script).expect("write editor");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod editor");
        Editor(dir)
    }

    fn command(&self) -> String {
        self.0.join("editor.sh").to_string_lossy().into_owned()
    }

    /// The file the editor was given, which must be a `.sql` file pgmig has removed since.
    fn assert_ran_and_cleaned_up(&self) {
        let seen = PathBuf::from(std::fs::read_to_string(self.0.join("seen")).expect("the editor ran"));
        assert!(seen.extension().is_some_and(|e| e == "sql"), "{}", seen.display());
        assert!(!seen.exists(), "{} was left behind", seen.display());
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `pgmig args` on a pseudo-terminal with `editor` as $EDITOR: its exit code, and stdout and
/// stderr together.
fn pgmig_tty(args: &[&str], editor: &Editor) -> (Option<i32>, String) {
    let bin = env!("CARGO_BIN_EXE_pgmig");
    let mut cmd = Command::new("script");
    if cfg!(target_os = "linux") {
        // util-linux: `script -qec COMMAND FILE`, the command run by a shell.
        let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
        let line = std::iter::once(bin).chain(args.iter().copied()).map(quote).collect::<Vec<_>>().join(" ");
        cmd.args(["-qec", &line, "/dev/null"]);
    } else {
        // BSD and macOS: `script -q FILE COMMAND ARGS...`.
        cmd.args(["-q", "/dev/null", bin]).args(args);
    }
    let out = cmd
        .env_remove("VISUAL")
        .env("EDITOR", editor.command())
        .stdin(Stdio::null())
        .output()
        .expect("run pgmig under `script` (needed for a terminal)");
    (out.status.code(), format!("{}{}", text(&out.stdout), text(&out.stderr)))
}

fn columns(db: &TestDb, table: &str) -> Vec<String> {
    let sql = format!("SELECT column_name FROM information_schema.columns WHERE table_name = '{table}' ORDER BY ordinal_position");
    db.connect().query(&sql).expect("query").into_iter().map(|r| r[0].clone().unwrap_or_default()).collect()
}

/// A target with `t (id)` and a desired state adding `t.name` and `t.age`.
fn setup() -> (TestDb, SqlFile, (String, Option<TestDb>)) {
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE t (id int);").expect("load target");
    (db, SqlFile::new("CREATE TABLE t (id int, name text, age int);"), shadow_arg())
}

fn apply_args<'a>(db: &'a TestDb, desired: &'a str, shadow: &'a str) -> Vec<&'a str> {
    vec!["schema", "apply", "--url", &db.conninfo, "--to", desired, "--shadow", shadow, "--edit", "--auto-approve"]
}

#[test]
fn apply_runs_what_the_editor_saved() {
    // Drop one planned statement and add one of the user's own.
    let editor = Editor::new(
        "grep -v age \"$1\" > \"$1.new\" && cat \"$1.new\" > \"$1\" && rm \"$1.new\"\nprintf 'CREATE TABLE extra (id int);\\n' >> \"$1\"",
    );
    let (db, desired, (shadow, _keep)) = setup();
    let (code, out) = pgmig_tty(&apply_args(&db, &desired.path(), &shadow), &editor);
    assert_eq!(code, Some(0), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("CREATE TABLE extra (id int);") && !out.contains("age"), "the edited plan is shown: {out}");
    assert!(out.contains("applied 2 statements"), "{out}");
    assert_eq!(columns(&db, "t"), ["id", "name"]);
    assert_eq!(columns(&db, "extra"), ["id"]);
}

#[test]
fn apply_with_an_emptied_plan_applies_nothing() {
    let editor = Editor::new(": > \"$1\"");
    let (db, desired, (shadow, _keep)) = setup();
    let (code, out) = pgmig_tty(&apply_args(&db, &desired.path(), &shadow), &editor);
    assert_eq!(code, Some(0), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("nothing to apply"), "{out}");
    assert_eq!(columns(&db, "t"), ["id"]);
}

#[test]
fn apply_aborts_when_the_editor_fails_or_the_text_does_not_split() {
    let (db, desired, (shadow, _keep)) = setup();
    let editor = Editor::new("printf 'SELECT 1;\\n' >> \"$1\"\nexit 1");
    let (code, out) = pgmig_tty(&apply_args(&db, &desired.path(), &shadow), &editor);
    assert_eq!(code, Some(1), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("exited with") && out.contains("discarded"), "{out}");

    let editor = Editor::new("printf \"SELECT 'unterminated;\\n\" >> \"$1\"");
    let (code, out) = pgmig_tty(&apply_args(&db, &desired.path(), &shadow), &editor);
    assert_eq!(code, Some(1), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("edited plan:3:8: unterminated quoted string"), "{out}");
    assert_eq!(columns(&db, "t"), ["id"]);
}

#[test]
fn migrate_diff_writes_what_the_editor_saved() {
    let editor = Editor::new("printf -- '-- reviewed\\n' >> \"$1\"\nprintf 'CREATE INDEX t_name ON public.t (name);\\n' >> \"$1\"");
    let (_, desired, (shadow, _keep)) = setup();
    let root = Editor::new("");
    let dir = root.0.join("migrations");
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = ["migrate", "diff", "edited", "--dir", &dir_arg, "--to", &desired.path(), "--shadow", &shadow, "--edit"];
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(0), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("(as edited)"), "{out}");
    let [file] = sql_files(&dir).try_into().expect("one file");
    let written = std::fs::read_to_string(dir.join(&file)).expect("read");
    assert_eq!(
        written,
        "CREATE TABLE public.t (\n    id integer,\n    name text,\n    age integer\n);\n-- reviewed\nCREATE INDEX t_name ON public.t (name);\n"
    );
    // Hashed as written.
    let out = pgmig(&["migrate", "validate", "--dir", &dir_arg, "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

#[test]
fn migrate_diff_writes_nothing_when_emptied_or_when_the_editor_fails() {
    let (_, desired, (shadow, _keep)) = setup();
    let root = Editor::new("");
    let dir = root.0.join("migrations");
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = ["migrate", "diff", "edited", "--dir", &dir_arg, "--to", &desired.path(), "--shadow", &shadow, "--edit"];

    let editor = Editor::new("printf -- '-- nothing to do\\n' > \"$1\"");
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(0), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("no statements: nothing written"), "{out}");
    assert!(sql_files(&dir).is_empty() && !dir.join("pgmig.sum").exists());

    let editor = Editor::new("exit 3");
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(1), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(sql_files(&dir).is_empty() && !dir.join("pgmig.sum").exists());

    let editor = Editor::new("printf -- '-- pgmig:txmode none\\n' >> \"$1\"");
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("edited migration:") && out.contains("before its first statement"), "{out}");
    assert!(sql_files(&dir).is_empty());
}

#[test]
fn edit_is_refused_without_a_terminal_or_with_json() {
    let (db, desired, (shadow, _keep)) = setup();
    let base = ["schema", "apply", "--url", &db.conninfo, "--to", &desired.path(), "--shadow", &shadow, "--edit", "--auto-approve"];
    let out = pgmig(&base);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("stdin is not a terminal"), "{}", text(&out.stderr));

    let out = pgmig(&[&base[..], &["--format", "json"]].concat());
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stdout));
    assert!(text(&out.stdout).contains(r#""code": "usage""#), "{}", text(&out.stdout));
    assert_eq!(columns(&db, "t"), ["id"]);

    let root = Editor::new("");
    let dir = root.0.join("migrations");
    let out = pgmig(&["migrate", "diff", "x", "--dir", &dir.to_string_lossy(), "--to", &desired.path(), "--shadow", &shadow, "--edit"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(!dir.exists());
}

/// The `.sql` files of a directory, in name order.
fn sql_files(d: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(d)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".sql")).collect())
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn apply_verify_aborts_an_edited_plan_before_touching_the_target() {
    let editor = Editor::new("grep -v age \"$1\" > \"$1.new\" && cat \"$1.new\" > \"$1\" && rm \"$1.new\"");
    let (db, desired, (shadow, _keep)) = setup();
    let path = desired.path();
    let args = [&apply_args(&db, &path, &shadow)[..], &["--verify"]].concat();
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(1), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("verification failed") && out.contains("add_column public.t.age"), "{out}");
    assert!(!out.contains("applied 1 statement"), "{out}");
    assert_eq!(columns(&db, "t"), ["id"]);

    // The edit kept whole verifies and applies.
    let editor = Editor::new("printf -- '-- reviewed\\n' >> \"$1\"");
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("verified:") && out.contains("applied 2 statements"), "{out}");
    assert_eq!(columns(&db, "t"), ["id", "name", "age"]);
}

#[test]
fn migrate_diff_verify_writes_nothing_when_the_edit_misses_a_change() {
    let editor = Editor::new("printf 'CREATE TABLE public.t (id integer, name text);\\n' > \"$1\"");
    let (_, desired, (shadow, _keep)) = setup();
    let root = Editor::new("");
    let dir = root.0.join("migrations");
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = ["migrate", "diff", "edited", "--dir", &dir_arg, "--to", &desired.path(), "--shadow", &shadow, "--edit", "--verify"];
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(1), "{out}");
    editor.assert_ran_and_cleaned_up();
    assert!(out.contains("verification failed") && out.contains("add_column public.t.age"), "{out}");
    assert!(sql_files(&dir).is_empty() && !dir.join("pgmig.sum").exists());
}

#[test]
fn an_edited_plan_keeps_its_statements_outside_a_transaction() {
    // The editor only adds a comment: the marker above the concurrent build must survive the round
    // trip through the text, or the build would run in a transaction and fail.
    let editor = Editor::new("printf -- '-- reviewed\\n' >> \"$1\"");
    let db = TestDb::new();
    db.connect().batch("CREATE TABLE t (id int, a int);").expect("load target");
    let desired = SqlFile::new("CREATE TABLE t (id int, a int);\nCREATE INDEX t_a_idx ON t (a);");
    let (shadow, _keep) = shadow_arg();
    let (code, out) = pgmig_tty(&[&apply_args(&db, &desired.path(), &shadow)[..], &["--concurrent-indexes"]].concat(), &editor);
    assert_eq!(code, Some(0), "{out}");
    // A pseudo-terminal ends lines with CR LF.
    let out = out.replace("\r\n", "\n");
    assert!(out.contains("-- pgmig:notx\nCREATE INDEX CONCURRENTLY t_a_idx"), "{out}");
    assert!(out.contains("applied 1 statements: 0 in 0 transaction(s), 1 outside a transaction"), "{out}");
    let rows = db.connect().query("SELECT indisvalid FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE i.relname = 't_a_idx'");
    assert_eq!(rows.expect("query"), vec![vec![Some("t".to_owned())]]);
}

#[test]
fn migrate_diff_edit_is_refused_when_the_plan_needs_several_files() {
    let editor = Editor::new("");
    let (shadow, _keep) = shadow_arg();
    let root = Editor::new("");
    let dir = root.0.join("migrations");
    let dir_arg = dir.to_string_lossy().into_owned();
    let base = SqlFile::new("CREATE TABLE t (id int);");
    let out = pgmig(&["migrate", "diff", "init", "--dir", &dir_arg, "--to", &base.path(), "--shadow", &shadow]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    let desired = SqlFile::new("CREATE TABLE t (id int, name text);\nCREATE INDEX t_name ON t (name);");
    let args =
        ["migrate", "diff", "idx", "--dir", &dir_arg, "--to", &desired.path(), "--shadow", &shadow, "--edit", "--concurrent-indexes"];
    let (code, out) = pgmig_tty(&args, &editor);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("--edit cannot be combined with concurrent_indexes"), "{out}");
    assert!(!editor.0.join("seen").exists(), "the editor was not run");
    assert_eq!(sql_files(&dir).len(), 1);
}
