//! `migrate ls`, `show`, `edit`, `rm` and `rebase`: the migration directory's maintenance commands.
//!
//! A migration is named by its file name, its version, or the start of exactly one file's version
//! ([`pgmig_migrate::Dir::find`]). The commands that change the directory check it against its sum
//! file first and rewrite the sum file after, so `migrate validate` passes when they succeed; they
//! never hash a change they did not make.
//!
//! With a target database, `rm` and `rebase` refuse a file its revisions table records (applied or
//! started), unless forced: the database's history would then name a file that no longer exists.
//! The target is only read: a database without a revisions table has applied nothing.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pgmig_migrate::compare_versions;
use pgmig_migrate::revisions::{Revision, Table};
use serde::{Deserialize, Serialize};

use crate::{Conn, Error, MigrationDir, Result};

/// What a target's revisions table says about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionStatus {
    /// Applied, and the file is unchanged since.
    Applied,
    /// Not in the table.
    Pending,
    /// Started and not finished: a failure, or a file without a transaction stopped part way.
    Partial,
    /// Applied, but the file was edited after (its hash does not match the recorded one).
    Changed,
    /// Its row was edited by hand (`migrate status` only: `migrate ls` refuses such a table).
    Edited,
}

impl RevisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RevisionStatus::Applied => "applied",
            RevisionStatus::Pending => "pending",
            RevisionStatus::Partial => "partial",
            RevisionStatus::Changed => "changed",
            RevisionStatus::Edited => "edited",
        }
    }
}

/// A file of [`migrate_ls`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedMigration {
    /// The file name.
    pub file: String,
    pub version: String,
    pub description: String,
    /// Its status in the target; null without one.
    pub status: Option<RevisionStatus>,
}

/// A row of the target's revisions table that no file of the directory has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingRevision {
    pub version: String,
    pub description: String,
}

/// What [`migrate_ls`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateLs {
    pub dir: PathBuf,
    /// Every migration, in version order.
    pub files: Vec<ListedMigration>,
    /// Versions the target records that have no file; empty without a target.
    pub missing: Vec<MissingRevision>,
}

/// `migrate ls`: the migrations of the directory at `dir` in order and, with a `target`, what its
/// revisions table (in `revisions_schema`) says about each. The directory is not checked against
/// its sum file and the target is only read.
pub fn migrate_ls(dir: &Path, target: Option<&str>, revisions_schema: &str) -> Result<MigrateLs> {
    let d = MigrationDir::open(dir)?;
    let rows = match target {
        Some(t) => Some(read_revisions(t, revisions_schema)?.map(|(_, rows)| rows).unwrap_or_default()),
        None => None,
    };
    let row = |v: &str| rows.as_ref().and_then(|rows| rows.iter().find(|r| compare_versions(&r.version, v) == Ordering::Equal));
    let files = d
        .files()
        .iter()
        .map(|f| ListedMigration {
            file: f.name.clone(),
            version: f.version.clone(),
            description: f.description.clone(),
            status: rows.as_ref().map(|_| match row(&f.version) {
                None => RevisionStatus::Pending,
                Some(r) if !r.is_complete() => RevisionStatus::Partial,
                Some(r) if r.hash != f.hash() => RevisionStatus::Changed,
                Some(_) => RevisionStatus::Applied,
            }),
        })
        .collect();
    let has_file = |v: &str| d.files().iter().any(|f| compare_versions(&f.version, v) == Ordering::Equal);
    let missing = rows
        .iter()
        .flatten()
        .filter(|r| !has_file(&r.version))
        .map(|r| MissingRevision { version: r.version.clone(), description: r.description.clone() })
        .collect();
    Ok(MigrateLs { dir: dir.to_owned(), files, missing })
}

/// The revisions table of `target` and its rows; `None` when there is no table. Never writes.
fn read_revisions(target: &str, schema: &str) -> Result<Option<(String, Vec<Revision>)>> {
    if schema.is_empty() {
        return Err(Error::Options("revisions_schema must be a schema name, not an empty string".into()));
    }
    let mut conn = Conn::connect(target)?;
    match Table::open(&mut conn, schema)? {
        Some(t) => Ok(Some((t.name(), t.read(&mut conn)?))),
        None => Ok(None),
    }
}

/// A migration file and its contents, as [`migrate_show`] prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateShow {
    /// The file's path.
    pub file: PathBuf,
    pub version: String,
    pub description: String,
    /// The file's text, exactly as on disk.
    pub sql: String,
}

/// `migrate show`: the migration `migration` (file name, version or unique version prefix) of the
/// directory at `dir`. A file that is not UTF-8 is an error.
pub fn migrate_show(dir: &Path, migration: &str) -> Result<MigrateShow> {
    let d = MigrationDir::open(dir)?;
    let f = d.find(migration)?;
    let file = d.path.join(&f.name);
    let sql = String::from_utf8(f.bytes.clone())
        .map_err(|e| Error::Io { path: file.clone(), source: std::io::Error::new(std::io::ErrorKind::InvalidData, e) })?;
    Ok(MigrateShow { file, version: f.version.clone(), description: f.description.clone(), sql })
}

/// What [`migrate_edit`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateEdit {
    /// The file edited.
    pub file: PathBuf,
    pub version: String,
    /// Whether its contents changed.
    pub changed: bool,
    /// The sum file, rewritten for the file's new contents.
    pub sum_file: PathBuf,
}

/// `migrate edit`: run `editor` (a program and its arguments; the file's path is added as the last
/// one, and no shell is involved) on the migration `migration` of the directory at `dir`, then
/// rehash it. The directory must match its sum file first. When the editor fails nothing is
/// rehashed, so `migrate validate` reports any change it left.
pub fn migrate_edit(dir: &Path, migration: &str, editor: &[String]) -> Result<MigrateEdit> {
    let Some((program, args)) = editor.split_first() else {
        return Err(Error::Options("no editor: set VISUAL or EDITOR".into()));
    };
    let d = open_validated(dir)?;
    let f = d.find(migration)?.clone();
    let file = d.path.join(&f.name);
    let command = editor.join(" ");
    let status = std::process::Command::new(program)
        .args(args)
        .arg(&file)
        .status()
        .map_err(|e| Error::Editor { command: command.clone(), reason: e.to_string() })?;
    if !status.success() {
        return Err(Error::Editor { command, reason: format!("it exited with {status}; {} was not rehashed", file.display()) });
    }
    let (now, _) = pgmig_migrate::maintain::rehash_file(&d, &f.name)?;
    Ok(MigrateEdit { file, version: f.version, changed: now.bytes != f.bytes, sum_file: d.sum_path() })
}

/// The target `rm` and `rebase` check before they touch a file the database may have applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Guard {
    /// The database whose revisions table is read; `None` checks nothing.
    pub target: Option<String>,
    /// Go ahead when the table records the file, instead of refusing.
    pub force: bool,
    /// The schema of the revisions table (default `pgmig`).
    pub revisions_schema: String,
}

impl Default for Guard {
    fn default() -> Guard {
        Guard { target: None, force: false, revisions_schema: pgmig_migrate::revisions::DEFAULT_SCHEMA.to_owned() }
    }
}

impl Guard {
    /// The files of `files` the target records, refusing them unless forced. Empty without a target.
    fn check(&self, dir: &MigrationDir, files: &[&pgmig_migrate::File]) -> Result<Vec<String>> {
        let Some(target) = &self.target else { return Ok(Vec::new()) };
        let Some((table, rows)) = read_revisions(target, &self.revisions_schema)? else { return Ok(Vec::new()) };
        let mut recorded = Vec::new();
        for f in files {
            let Some(r) = rows.iter().find(|r| compare_versions(&r.version, &f.version) == Ordering::Equal) else { continue };
            if !self.force {
                return Err(Error::MigrationApplied { path: dir.path.join(&f.name), table, partial: !r.is_complete() });
            }
            recorded.push(f.name.clone());
        }
        Ok(recorded)
    }
}

/// What [`migrate_rm`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateRm {
    /// The file deleted.
    pub file: PathBuf,
    pub version: String,
    /// The sum file, rewritten without it.
    pub sum_file: PathBuf,
    /// Whether the target records the file (only with `force`): its history now names a version
    /// without a file, which `migrate apply` refuses until the row is fixed.
    pub recorded: bool,
}

/// `migrate rm`: delete the migration `migration` of the directory at `dir` and rehash. The
/// directory must match its sum file first; with a target, see [`Guard`].
pub fn migrate_rm(dir: &Path, migration: &str, guard: &Guard) -> Result<MigrateRm> {
    let d = open_validated(dir)?;
    let f = d.find(migration)?;
    let recorded = !guard.check(&d, &[f])?.is_empty();
    pgmig_migrate::maintain::remove(&d, &f.name)?;
    Ok(MigrateRm { file: d.path.join(&f.name), version: f.version.clone(), sum_file: d.sum_path(), recorded })
}

/// A file [`migrate_rebase`] moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebasedFile {
    /// Its path before, and after.
    pub from: PathBuf,
    pub to: PathBuf,
    pub from_version: String,
    pub to_version: String,
}

/// What [`migrate_rebase`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateRebase {
    /// The files moved, in their new order; empty when they already came last.
    pub moved: Vec<RebasedFile>,
    pub sum_file: PathBuf,
}

/// `migrate rebase`: move the migrations `migrations` of the directory at `dir` after every other
/// file, each with a new version (the UTC time, or one more than the file before) and its
/// description, and rehash. For a merge in which both branches added files: keep the other
/// branch's sum file, then rebase your own files.
///
/// The directory must match its sum file, either as a whole or without the files moved (they are
/// the ones the sum file does not know yet). With a target, see [`Guard`].
pub fn migrate_rebase(dir: &Path, migrations: &[String], guard: &Guard) -> Result<MigrateRebase> {
    migrate_rebase_at(dir, migrations, guard, SystemTime::now())
}

fn migrate_rebase_at(dir: &Path, migrations: &[String], guard: &Guard, now: SystemTime) -> Result<MigrateRebase> {
    if migrations.is_empty() {
        return Err(Error::Options("rebase needs at least one migration".into()));
    }
    let d = MigrationDir::open(dir)?;
    let mut names: Vec<&str> = Vec::new();
    for m in migrations {
        let name = d.find(m)?.name.as_str();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    validate_rebase(&d, &names)?;
    let files: Vec<&pgmig_migrate::File> = d.files().iter().filter(|f| names.contains(&f.name.as_str())).collect();
    guard.check(&d, &files)?;
    let (moves, _) = pgmig_migrate::maintain::rebase(&d, &names, now)?;
    let moved = moves
        .into_iter()
        .map(|m| RebasedFile { from: d.path.join(&m.from), to: d.path.join(&m.to), from_version: m.from_version, to_version: m.to_version })
        .collect();
    Ok(MigrateRebase { moved, sum_file: d.sum_path() })
}

/// The directory matches its sum file, or does without the files `names` when the sum file lists
/// none of them.
fn validate_rebase(d: &MigrationDir, names: &[&str]) -> Result<()> {
    let Err(whole) = pgmig_migrate::validate(d) else { return Ok(()) };
    let sum = match d.read_sum() {
        Ok(Some(sum)) => sum,
        _ => return Err(whole.into()),
    };
    if sum.lines.iter().any(|l| names.contains(&l.file.as_str())) {
        return Err(whole.into());
    }
    Ok(pgmig_migrate::check(&d.without(names), &sum)?)
}

/// The directory at `dir`, checked against its sum file.
fn open_validated(dir: &Path) -> Result<MigrationDir> {
    let d = MigrationDir::open(dir)?;
    pgmig_migrate::validate(&d)?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_print_as_their_names() {
        for s in
            [RevisionStatus::Applied, RevisionStatus::Pending, RevisionStatus::Partial, RevisionStatus::Changed, RevisionStatus::Edited]
        {
            assert_eq!(serde_json::to_string(&s).expect("serialize"), format!("\"{}\"", s.as_str()));
        }
    }

    #[test]
    fn rebase_at_a_fixed_time() {
        let path = std::env::temp_dir().join(format!("pgmig-rebase-at-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("dir");
        std::fs::write(path.join("20261101000000_a.sql"), "A").expect("a");
        std::fs::write(path.join("20261105000000_b.sql"), "B").expect("b");
        crate::migrate_hash(&path).expect("hash");
        std::fs::write(path.join("20261103000000_mine.sql"), "M").expect("mine");
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_795_689_000);
        let res = migrate_rebase_at(&path, &["20261103".into()], &Guard::default(), now).expect("rebase");
        assert_eq!(res.moved[0].to, path.join("20261126103000_mine.sql"));
        crate::migrate_validate(&path, None).expect("valid");
        let _ = std::fs::remove_dir_all(&path);
    }
}
