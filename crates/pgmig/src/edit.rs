//! `--edit`: open SQL in the user's editor and read back what they saved.
//!
//! The editor is a program and its arguments (no shell); the file's path is added as the last
//! argument. The SQL goes to a new file in the system temp directory (`pgmig-edit-*.sql`, readable
//! only by its owner on Unix), which is removed afterwards whatever happens.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{Error, Plan, Result};

/// The name errors give the text of [`plan_from_sql`]: its temp file is gone by then.
pub const EDITED_PLAN: &str = "edited plan";

/// Run `editor` on `file` and wait for it. When it cannot start or fails, the error's reason ends
/// with `failed`, which says what was left undone.
pub(crate) fn launch(editor: &[String], file: &Path, failed: &str) -> Result<()> {
    let Some((program, args)) = editor.split_first() else {
        return Err(Error::Options("no editor: set VISUAL or EDITOR".into()));
    };
    let command = editor.join(" ");
    let status = std::process::Command::new(program)
        .args(args)
        .arg(file)
        .status()
        .map_err(|e| Error::Editor { command: command.clone(), reason: format!("{e}; {failed}") })?;
    if !status.success() {
        return Err(Error::Editor { command, reason: format!("it exited with {status}; {failed}") });
    }
    Ok(())
}

/// A temp file, removed when dropped.
struct TempSql {
    path: PathBuf,
}

impl TempSql {
    fn create(text: &str) -> Result<TempSql> {
        use std::io::Write as _;
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let mut tries = 0;
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("pgmig-edit-{}-{nanos:09}-{n}.sql", std::process::id()));
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            match opts.open(&path) {
                Ok(mut f) => {
                    // Owned from here: removed even when the write fails.
                    let tmp = TempSql { path };
                    f.write_all(text.as_bytes()).map_err(|source| Error::Io { path: tmp.path.clone(), source })?;
                    return Ok(tmp);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 100 => tries += 1,
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
    }
}

impl Drop for TempSql {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Open `sql` in `editor` and return the text it saved. The temp file is removed in every case;
/// when the editor fails, its error says so and nothing of the edit is returned.
pub fn edit_sql(sql: &str, editor: &[String]) -> Result<String> {
    if editor.is_empty() {
        return Err(Error::Options("no editor: set VISUAL or EDITOR".into()));
    }
    let tmp = TempSql::create(sql)?;
    launch(editor, &tmp.path, "the edit was discarded")?;
    let bytes = std::fs::read(&tmp.path).map_err(|source| Error::Io { path: tmp.path.clone(), source })?;
    String::from_utf8(bytes)
        .map_err(|e| Error::Io { path: tmp.path.clone(), source: std::io::Error::new(std::io::ErrorKind::InvalidData, e) })
}

/// A plan from SQL text such as [`Plan::to_sql`] prints: its statements, and the commit point a
/// `-- pgmig:commit` line between two statements marks. A plan has at most one commit point, and
/// no other directive. Errors name the text [`EDITED_PLAN`], with the line and column.
pub fn plan_from_sql(text: &str) -> Result<Plan> {
    let path = PathBuf::from(EDITED_PLAN);
    let sql = pgmig_sql::split(text).map_err(|source| Error::Split { path: path.clone(), source })?;
    let all = sql.directives.iter().chain(sql.statements.iter().flat_map(|s| &s.directives));
    if let Some(d) = all.filter(|d| d.name != "commit").min_by_key(|d| d.pos) {
        let message = format!("pgmig:{} is not understood in a plan; only pgmig:commit is", d.name);
        return Err(Error::Directive { path, pos: d.pos, message });
    }
    let commits = crate::migrate_apply::commit_points(&sql).map_err(|(pos, message)| Error::Directive {
        path: path.clone(),
        pos,
        message: message.to_owned(),
    })?;
    if let Some((_, pos)) = commits.get(1) {
        let message = "a plan has at most one commit point (pgmig:commit)".to_owned();
        return Err(Error::Directive { path, pos: *pos, message });
    }
    Ok(Plan { statements: sql.statements.into_iter().map(|s| s.sql).collect(), commit_after: commits.first().map(|(i, _)| i - 1) })
}

/// Check the text of an edited migration as `migrate apply` would read it: it splits, and its
/// directives are well placed. Returns its number of statements. Errors name the text `label`.
pub(crate) fn check_migration_sql(text: &str, label: &str) -> Result<usize> {
    let path = PathBuf::from(label);
    let sql = pgmig_sql::split(text).map_err(|source| Error::Split { path: path.clone(), source })?;
    let all = sql.directives.iter().chain(sql.statements.iter().flat_map(|s| &s.directives));
    if let Some(d) = all.filter(|d| d.name == "import").min_by_key(|d| d.pos) {
        return Err(Error::MigrationImport { path, pos: d.pos });
    }
    crate::migrate_apply::check_directives(&path, &sql)?;
    Ok(sql.statements.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_round_trips_through_its_sql() {
        let plan = Plan {
            statements: vec!["ALTER TYPE e ADD VALUE 'b'".into(), "CREATE TABLE t (c e DEFAULT 'b')".into(), "SELECT ';'".into()],
            commit_after: Some(0),
        };
        assert_eq!(plan_from_sql(&plan.to_sql()).expect("parse"), plan);
        let one = Plan { statements: vec!["SELECT 1".into()], commit_after: None };
        assert_eq!(plan_from_sql(&one.to_sql()).expect("parse"), one);
        assert_eq!(plan_from_sql("-- nothing\n").expect("parse"), Plan::default());
    }

    #[test]
    fn bad_plans_are_located() {
        let err = plan_from_sql("SELECT 1;\nSELECT 'x;\n").expect_err("unterminated");
        assert_eq!(err.to_string(), "edited plan:2:8: unterminated quoted string");
        let err = plan_from_sql("SELECT 1;\n-- pgmig:commit\nSELECT 2;\n-- pgmig:commit\nSELECT 3;\n").expect_err("two");
        assert!(err.to_string().starts_with("edited plan:4:1: a plan has at most one"), "{err}");
        let err = plan_from_sql("-- pgmig:txmode none\nSELECT 1;\n").expect_err("txmode");
        assert!(err.to_string().starts_with("edited plan:1:1: pgmig:txmode is not understood"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn edit_reads_back_and_removes_the_file() {
        // `sh -c SCRIPT NAME FILE`: the script sees the file as $1.
        let editor = |script: &str| vec!["sh".to_owned(), "-c".to_owned(), script.to_owned(), "editor".to_owned()];
        let seen = std::env::temp_dir().join(format!("pgmig-edit-test-{}", std::process::id()));
        let script = format!("printf '%s' \"$1\" > '{}'; printf 'SELECT 2;\\n' >> \"$1\"", seen.display());
        assert_eq!(edit_sql("SELECT 1;\n", &editor(&script)).expect("edit"), "SELECT 1;\nSELECT 2;\n");
        let file = PathBuf::from(std::fs::read_to_string(&seen).expect("seen"));
        assert!(file.extension().is_some_and(|e| e == "sql") && !file.exists(), "{}", file.display());

        let script = format!("printf '%s' \"$1\" > '{}'; exit 3", seen.display());
        let err = edit_sql("SELECT 1;\n", &editor(&script)).expect_err("fails");
        assert!(matches!(&err, Error::Editor { reason, .. } if reason.contains("discarded")), "{err}");
        assert!(!PathBuf::from(std::fs::read_to_string(&seen).expect("seen")).exists());
        let _ = std::fs::remove_file(&seen);
    }

    #[cfg(unix)]
    #[test]
    fn the_temp_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempSql::create("SELECT 1;\n").expect("create");
        let mode = std::fs::metadata(&tmp.path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let path = tmp.path.clone();
        drop(tmp);
        assert!(!path.exists());
    }
}
