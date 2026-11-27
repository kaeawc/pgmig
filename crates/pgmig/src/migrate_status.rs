//! `migrate status` and `migrate set`: what a database's history says, and fixing it by hand.
//!
//! `migrate status` only reads: the directory (not required to match its sum file: a mismatch is
//! reported) and the revisions table, hand-edited rows included. It reports the current version
//! (the newest complete row), each file's status, the files left to apply, the last error, and
//! every problem that makes `migrate apply` refuse or stop, each with how to recover.
//!
//! `migrate set <version>` makes the history exactly the directory up to and including that
//! version, without running anything, in one transaction under `migrate apply`'s advisory lock:
//!
//! - a file without a row gets one: kind normal, every statement applied, the file's hash (never
//!   needs `force`: that is what `set` is for);
//! - a row that is not complete (a failure, a file without a transaction stopped part way, a
//!   statement left in flight) is resolved: recorded as complete. Some of its statements may not
//!   have run, so this needs `force`. This is the recovery path for a statement left in flight:
//!   check whether it took effect, finish the file by hand, then `set --force` its version; or undo
//!   what it applied and `set` the version before it, which drops the row (no `force` needed: it
//!   is not complete) so `apply` runs the file again;
//! - a row edited by hand is rewritten, as complete (needs `force` when it is not complete);
//! - a complete row whose file was edited after it was applied is rehashed (needs `force`);
//! - rows of later versions, or of versions without a file, are deleted; deleting one that records
//!   a complete migration needs `force`.
//!
//! `0` (when no file has that version) clears the history. The directory must match its sum file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pgmig_migrate::compare_versions;
use pgmig_migrate::revisions::{Kind, Revision, Stored, Table};
use serde::{Deserialize, Serialize};

use crate::migrate_maintain::{MissingRevision, RevisionStatus};
use crate::{Conn, Error, MigrationDir, Result};

/// A file of [`MigrateStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusFile {
    pub file: String,
    pub version: String,
    pub description: String,
    pub status: RevisionStatus,
    /// The row's kind (`normal`, `baseline`, `checkpoint`); null without a row.
    pub kind: Option<String>,
}

/// What kind of problem a [`StatusIssue`] is, serialized in snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    /// The directory does not match its sum file.
    Directory,
    /// A version the table records has no file.
    Missing,
    /// An applied file was edited after it was applied.
    Changed,
    /// A row was edited by hand.
    Edited,
    /// A file without a row is older than the current version (refused under the linear order).
    OutOfOrder,
    /// A file failed; running `apply` again retries it.
    Failed,
    /// A file without a transaction stopped part way; running `apply` again resumes it.
    Interrupted,
    /// A statement without a transaction was started and not recorded: someone must check it.
    InFlight,
}

/// A problem `migrate apply` would refuse or stop at, and how to recover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusIssue {
    pub kind: IssueKind,
    /// The version concerned; null for the directory.
    pub version: Option<String>,
    pub message: String,
    pub hint: String,
}

/// What [`migrate_status`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateStatus {
    pub dir: PathBuf,
    /// The revisions table; null when the database has none (nothing applied).
    pub revisions_table: Option<String>,
    /// The newest version recorded as complete; null when none is.
    pub current: Option<String>,
    /// Every file, in version order.
    pub files: Vec<StatusFile>,
    /// The files `migrate apply` would run (not yet applied, or started and not finished), in order.
    pub pending: Vec<String>,
    /// The error of the newest row that records one.
    pub last_error: Option<String>,
    /// Versions the table records without a file.
    pub missing: Vec<MissingRevision>,
    pub issues: Vec<StatusIssue>,
}

impl MigrateStatus {
    /// Nothing pending and no problems.
    pub fn is_up_to_date(&self) -> bool {
        self.pending.is_empty() && self.issues.is_empty()
    }
}

/// `migrate status`: the history of the database at `target` (revisions table in
/// `revisions_schema`) against the directory at `dir`. Only reads; takes no lock.
pub fn migrate_status(target: &str, dir: &Path, revisions_schema: &str) -> Result<MigrateStatus> {
    read_status(target, dir, revisions_schema).map(|(status, ..)| status)
}

/// [`migrate_status`], with the directory as read (not checked against its sum file) and the
/// revisions table's rows, hand-edited ones included: what `drift check` replays.
pub(crate) fn read_status(target: &str, dir: &Path, revisions_schema: &str) -> Result<(MigrateStatus, MigrationDir, Vec<Stored>)> {
    if revisions_schema.is_empty() {
        return Err(Error::Options("revisions_schema must be a schema name, not an empty string".into()));
    }
    let d = MigrationDir::open(dir)?;
    let mut issues = Vec::new();
    // A directory with no migrations and no sum file has nothing to check.
    let checked = match d.read_sum() {
        Ok(None) if d.files().is_empty() => Ok(()),
        Ok(_) => pgmig_migrate::validate(&d),
        Err(e) => Err(e),
    };
    if let Err(e) = checked {
        issues.push(StatusIssue {
            kind: IssueKind::Directory,
            version: None,
            message: e.to_string(),
            hint: "`migrate apply` refuses the directory: undo the change, or run `pgmig migrate hash` if it is intended".into(),
        });
    }
    let mut conn = Conn::connect(target)?;
    let table = Table::open(&mut conn, revisions_schema)?;
    let rows = match &table {
        Some(t) => t.read_unchecked(&mut conn)?,
        None => Vec::new(),
    };
    drop(conn);
    let current = rows
        .iter()
        .filter(|s| !s.edited && s.revision.is_complete())
        .map(|s| s.revision.version.clone())
        .max_by(|a, b| compare_versions(a, b));
    let row = |v: &str| rows.iter().find(|s| compare_versions(&s.revision.version, v).is_eq());
    let mut files = Vec::new();
    let mut pending = Vec::new();
    for f in d.files() {
        let issue = |kind, message: String, hint: String| StatusIssue { kind, version: Some(f.version.clone()), message, hint };
        let status = match row(&f.version) {
            None => {
                if let Some(c) = current.as_deref().filter(|c| compare_versions(&f.version, c).is_lt()) {
                    issues.push(issue(
                        IssueKind::OutOfOrder,
                        format!("{} is not applied and older than the current version {c}", f.name),
                        "`migrate apply` refuses it under the linear order: give it a later version (`pgmig migrate rebase`), or pass \
                         --exec-order non-linear (apply it) or linear-skip (leave it out)"
                            .into(),
                    ));
                }
                pending.push(f.name.clone());
                RevisionStatus::Pending
            }
            Some(s) if s.edited => {
                issues.push(issue(
                    IssueKind::Edited,
                    format!("the row of {} was edited by hand", f.name),
                    format!(
                        "`migrate apply` refuses the table: check the database, then rewrite the row with `pgmig migrate set {}` (or an \
                         earlier version to drop it)",
                        f.version
                    ),
                ));
                RevisionStatus::Edited
            }
            Some(s) if s.revision.is_complete() && s.revision.hash != f.hash() => {
                issues.push(issue(
                    IssueKind::Changed,
                    format!("{} was edited after it was applied", f.name),
                    format!(
                        "`migrate apply` refuses it: restore the file as it was applied, or record it as it is now with `pgmig migrate set {} --force`",
                        current.as_deref().unwrap_or(&f.version)
                    ),
                ));
                RevisionStatus::Changed
            }
            Some(s) if s.revision.is_complete() => RevisionStatus::Applied,
            Some(s) => {
                let r = &s.revision;
                let done = format!("{} of {} statements applied", r.applied, r.total);
                issues.push(match (r.in_flight(), r.error.as_deref()) {
                    (Some(i), _) => issue(
                        IssueKind::InFlight,
                        format!("{}: statement {} was started without a transaction and whether it finished was not recorded ({done})", f.name, i + 1),
                        format!(
                            "`migrate apply` stops here. Check whether the statement took effect; then either finish the file's remaining \
                             statements by hand and run `pgmig migrate set {} --force`, or undo the statements it applied and run `pgmig migrate set` \
                             with the version before it, so `migrate apply` runs the file again",
                            f.version
                        ),
                    ),
                    (None, Some(e)) => issue(
                        IssueKind::Failed,
                        format!("{} failed ({done}): {e}", f.name),
                        "the statements before the failing one are applied and recorded. Fix the failing statement (or what it depends on) \
                         and edit only it and the statements after it, then run `pgmig migrate hash` and `pgmig migrate apply`: it resumes at the \
                         failing statement. Changing an applied statement makes `apply` refuse; `pgmig migrate set <version> --force` records the \
                         file as complete instead"
                            .into(),
                    ),
                    (None, None) => issue(
                        IssueKind::Interrupted,
                        format!("{} stopped part way ({done})", f.name),
                        "run `pgmig migrate apply`: it resumes after the statements already applied".into(),
                    ),
                });
                pending.push(f.name.clone());
                RevisionStatus::Partial
            }
        };
        files.push(StatusFile {
            file: f.name.clone(),
            version: f.version.clone(),
            description: f.description.clone(),
            status,
            kind: row(&f.version).map(|s| s.revision.kind.to_string()),
        });
    }
    let has_file = |v: &str| d.files().iter().any(|f| compare_versions(&f.version, v).is_eq());
    let mut missing = Vec::new();
    for s in rows.iter().filter(|s| !has_file(&s.revision.version)) {
        let r = &s.revision;
        missing.push(MissingRevision { version: r.version.clone(), description: r.description.clone() });
        issues.push(StatusIssue {
            kind: IssueKind::Missing,
            version: Some(r.version.clone()),
            message: format!("version {} ({}) is recorded but no file has it", r.version, r.description),
            hint: "`migrate apply` refuses the table: restore the file, or drop the row with `pgmig migrate set` and the version before \
                   it (--force when it is applied)"
                .into(),
        });
    }
    let last_error = rows.iter().rev().filter(|s| !s.revision.is_complete()).find_map(|s| s.revision.error.clone());
    let status = MigrateStatus {
        dir: dir.to_owned(),
        revisions_table: table.map(|t| t.name()),
        current,
        files,
        pending,
        last_error,
        missing,
        issues,
    };
    Ok((status, d, rows))
}

/// How [`migrate_set`] runs. Every field may be left out of its JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetOptions {
    /// Record rows that are not complete as complete, delete rows that record a complete
    /// migration, and rehash applied files that were edited.
    pub force: bool,
    /// Report what would change and change nothing (the revisions table is not created).
    pub dry_run: bool,
    /// Take `migrate apply`'s advisory lock (default true).
    pub lock: bool,
    /// The lock's name; `None` is `pgmig:<revisions_schema>`, as for `migrate apply`.
    pub lock_name: Option<String>,
    /// How long to wait for the lock; 0 tries once.
    pub lock_timeout_ms: u64,
    pub revisions_schema: String,
}

impl Default for SetOptions {
    fn default() -> SetOptions {
        SetOptions {
            force: false,
            dry_run: false,
            lock: true,
            lock_name: None,
            lock_timeout_ms: crate::migrate_apply::DEFAULT_LOCK_TIMEOUT_MS,
            revisions_schema: pgmig_migrate::revisions::DEFAULT_SCHEMA.to_owned(),
        }
    }
}

impl SetOptions {
    /// The lock name in use.
    pub fn lock_name(&self) -> String {
        self.lock_name.clone().unwrap_or_else(|| pgmig_migrate::history::default_lock_name(&self.revisions_schema))
    }
}

/// What [`migrate_set`] does to a row, serialized in snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetAction {
    /// A file without a row, recorded as applied.
    Insert,
    /// A row that was not complete, recorded as complete (needs `force`).
    Resolve,
    /// A complete row whose file was edited, given the file's hash now.
    Rehash,
    /// A row edited by hand, rewritten.
    Repair,
    /// A row after the version, or without a file, deleted.
    Delete,
}

/// A row [`migrate_set`] changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetChange {
    pub version: String,
    /// The file; null for a row without one.
    pub file: Option<String>,
    pub action: SetAction,
}

/// What [`migrate_set`] did, or with `dry_run` would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateSet {
    pub dry_run: bool,
    /// The version the history now ends at; null for `0` (no history).
    pub version: Option<String>,
    pub file: Option<String>,
    /// The rows changed, in version order; rows already right are left out.
    pub changes: Vec<SetChange>,
}

/// `migrate set`: make the history of the database at `target` exactly the files of the directory
/// at `dir` up to and including `version` (exact version or file name; see the module docs).
pub fn migrate_set(target: &str, dir: &Path, version: &str, opts: &SetOptions) -> Result<MigrateSet> {
    if opts.revisions_schema.is_empty() {
        return Err(Error::Options("revisions_schema must be a schema name, not an empty string".into()));
    }
    if opts.lock_name.as_deref() == Some("") {
        return Err(Error::Options("the lock name must not be empty".into()));
    }
    let d = MigrationDir::open(dir)?;
    pgmig_migrate::validate(&d)?;
    let upto = match crate::exact_file(&d, version) {
        Ok(f) => Some(f),
        Err(_) if !version.is_empty() && version.bytes().all(|b| b == b'0') => None,
        Err(e) => return Err(e),
    };
    let mut wanted = Vec::new();
    if let Some(u) = upto {
        for f in d.files().iter().filter(|f| compare_versions(&f.version, &u.version).is_le()) {
            wanted.push((f, statement_count(&d, f)?));
        }
    }
    let mut conn = Conn::connect(target)?;
    let key = pgmig_migrate::history::lock_key(&opts.lock_name());
    if opts.lock {
        crate::migrate_apply::lock(&mut conn, &opts.lock_name(), key, Duration::from_millis(opts.lock_timeout_ms))?;
    }
    let res = set_locked(&mut conn, &d, &wanted, version, opts);
    if opts.lock {
        let _ = conn.query(&format!("SELECT pg_advisory_unlock({key})"));
    }
    let changes = res?;
    Ok(MigrateSet { dry_run: opts.dry_run, version: upto.map(|f| f.version.clone()), file: upto.map(|f| f.name.clone()), changes })
}

/// The statements in `f`.
fn statement_count(d: &MigrationDir, f: &pgmig_migrate::File) -> Result<u32> {
    let path = d.path.join(&f.name);
    let text = std::str::from_utf8(&f.bytes)
        .map_err(|e| Error::Io { path: path.clone(), source: std::io::Error::new(std::io::ErrorKind::InvalidData, e) })?;
    let (sql, _) = crate::migrate_apply::split_migration_text(&path, text)?;
    Ok(u32::try_from(sql.statements.len()).unwrap_or(u32::MAX))
}

/// Why recording the incomplete row `r` of `f` as complete needs `force`: which statements may not
/// have run.
fn resolve_reason(f: &pgmig_migrate::File, r: &Revision) -> String {
    let total = r.total as usize;
    // Statements `first` (1-based) to the last.
    let from = |first: usize| match first {
        _ if first > total => None,
        _ if first == total => Some(format!("statement {total}")),
        _ => Some(format!("statements {first}-{total}")),
    };
    let next = r.applied as usize + 1;
    let what = match (r.in_flight(), r.error.as_deref()) {
        (Some(i), _) => {
            let after = from(i + 2).map_or_else(String::new, |s| format!(", and {s} did not run"));
            format!("statement {} was left in flight (whether it took effect is unknown){after}", i + 1)
        }
        (None, Some(_)) => {
            format!("it failed at statement {next}: {} may not have run", from(next).unwrap_or_else(|| "the last statements".into()))
        }
        (None, None) => format!(
            "it stopped after {} of {total} statements: {} may not have run",
            r.applied,
            from(next).unwrap_or_else(|| "the last statements".into())
        ),
    };
    format!("record version {} ({}) as complete, though {what}", f.version, f.name)
}

enum Write {
    Replace(Revision),
    Delete(String),
}

fn set_locked(
    conn: &mut Conn,
    d: &MigrationDir,
    wanted: &[(&pgmig_migrate::File, u32)],
    version: &str,
    opts: &SetOptions,
) -> Result<Vec<SetChange>> {
    let table = if opts.dry_run { Table::open(conn, &opts.revisions_schema)? } else { Some(Table::ensure(conn, &opts.revisions_schema)?) };
    let stored: Vec<Stored> = match &table {
        Some(t) => t.read_unchecked(conn)?,
        None => Vec::new(),
    };
    let mut changes = Vec::new();
    let mut writes = Vec::new();
    let mut reasons = Vec::new();
    let mut matched = vec![false; stored.len()];
    for (f, n) in wanted {
        let found = stored.iter().position(|s| compare_versions(&s.revision.version, &f.version).is_eq());
        let (action, mut rev) = match found {
            None => (SetAction::Insert, Revision::start(&f.version, &f.description, Kind::Normal, *n, &f.hash())),
            Some(i) => {
                matched[i] = true;
                let s = &stored[i];
                let r = &s.revision;
                let action = if s.edited {
                    if !r.is_complete() {
                        reasons.push(resolve_reason(f, r));
                    }
                    SetAction::Repair
                } else if !r.is_complete() {
                    reasons.push(resolve_reason(f, r));
                    SetAction::Resolve
                } else if r.hash != f.hash() {
                    reasons.push(format!("record {} as it is now, though it was edited after it was applied", f.name));
                    SetAction::Rehash
                } else {
                    continue;
                };
                let mut rev = r.clone();
                if s.edited {
                    rev.partial_hashes.clear();
                }
                (action, rev)
            }
        };
        rev.description.clone_from(&f.description);
        rev.hash = f.hash();
        rev.total = *n;
        rev.applied = *n;
        rev.partial_hashes.truncate(*n as usize);
        rev.error = None;
        rev.error_stmt = None;
        rev.pgmig_version = env!("CARGO_PKG_VERSION").to_owned();
        changes.push(SetChange { version: f.version.clone(), file: Some(f.name.clone()), action });
        writes.push(Write::Replace(rev));
    }
    for (s, _) in stored.iter().zip(&matched).filter(|(_, m)| !**m) {
        let r = &s.revision;
        let file = d.files().iter().find(|f| compare_versions(&f.version, &r.version).is_eq()).map(|f| f.name.clone());
        if r.is_complete() {
            reasons.push(format!("drop the row of applied version {} ({})", r.version, file.as_deref().unwrap_or("no file")));
        }
        changes.push(SetChange { version: r.version.clone(), file, action: SetAction::Delete });
        writes.push(Write::Delete(r.version.clone()));
    }
    if !reasons.is_empty() && !opts.force {
        let name = table.as_ref().map_or_else(|| format!("{}.pgmig_revisions", opts.revisions_schema), Table::name);
        return Err(Error::NeedsForce { table: name, version: version.to_owned(), reasons });
    }
    changes.sort_by(|a, b| compare_versions(&a.version, &b.version));
    if opts.dry_run || writes.is_empty() {
        return Ok(changes);
    }
    let table = table.expect("ensured");
    conn.batch("BEGIN")?;
    let res = (|| -> Result<()> {
        for w in &writes {
            match w {
                Write::Replace(rev) => table.replace(conn, rev, true)?,
                Write::Delete(v) => {
                    table.delete(conn, v)?;
                }
            }
        }
        Ok(conn.batch("COMMIT")?)
    })();
    if res.is_err() {
        let _ = conn.batch("ROLLBACK");
    }
    res.map(|()| changes)
}
