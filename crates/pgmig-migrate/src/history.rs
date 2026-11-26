//! What a database's history says is left to apply: the revisions table compared with the
//! directory, under an execution order policy.
//!
//! # Rules
//!
//! - Every row must name a file of the directory ([`HistoryError::Missing`]): a migration that was
//!   applied and then deleted is an error, not something to forget.
//! - A complete row's `hash` must be the file's hash now ([`File::hash`]; [`HistoryError::Changed`]):
//!   an applied file was edited. The hash is the file's own (its name and bytes), not the chained
//!   sum-file line, so inserting an older file (non-linear order) does not change the recorded
//!   hash of every file after it.
//! - A row that is not complete (a failure, or a non-transactional file stopped part way) is
//!   resumed whatever its position: it was already started. Its hash may differ (the failing
//!   statement was fixed); the statements it recorded as applied are checked against their hashes
//!   when it resumes.
//! - A file without a row older than the newest complete row is out of order: [`ExecOrder`] decides.

use std::cmp::Ordering;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

use crate::revisions::Revision;
use crate::{Dir, File, compare_versions};

/// How files older than the newest applied one are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecOrder {
    /// Every pending file must be newer than the newest applied one; an older one is an error.
    #[default]
    Linear,
    /// Older pending files are skipped (and reported); newer ones are applied.
    LinearSkip,
    /// Older pending files are applied too, in version order with the rest.
    NonLinear,
}

impl ExecOrder {
    pub fn parse(s: &str) -> Option<ExecOrder> {
        match s {
            "linear" => Some(ExecOrder::Linear),
            "linear-skip" => Some(ExecOrder::LinearSkip),
            "non-linear" => Some(ExecOrder::NonLinear),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ExecOrder::Linear => "linear",
            ExecOrder::LinearSkip => "linear-skip",
            ExecOrder::NonLinear => "non-linear",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(
        "version {version} is applied ({table}) but no file of {dir} has it; restore the file, or fix the table with `pgmig migrate set`"
    )]
    Missing { table: String, dir: PathBuf, version: String },
    #[error("{file}: edited after it was applied (its hash does not match the one recorded in {table})")]
    Changed { table: String, file: PathBuf },
    #[error("{file}: older than {last}, which is already applied; give it a later version, or pass --exec-order linear-skip or non-linear")]
    OutOfOrder { file: PathBuf, last: String },
}

/// A file left to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending<'a> {
    pub file: &'a File,
    /// Its row, when an earlier run started it and did not finish.
    pub resume: Option<Revision>,
}

/// What [`pending`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status<'a> {
    /// Files with a complete row, in version order.
    pub applied: Vec<&'a File>,
    /// Files to apply, in version order.
    pub pending: Vec<Pending<'a>>,
    /// Older files left out under [`ExecOrder::LinearSkip`].
    pub skipped: Vec<&'a File>,
}

/// Compare `rows` (the revisions table, in version order) with `dir` (already checked against its
/// sum file); `table` names the table in errors.
pub fn pending<'a>(dir: &'a Dir, rows: &[Revision], order: ExecOrder, table: &str) -> Result<Status<'a>, HistoryError> {
    let find = |v: &str| dir.files().iter().find(|f| compare_versions(&f.version, v) == Ordering::Equal);
    for r in rows {
        let Some(f) = find(&r.version) else {
            return Err(HistoryError::Missing { table: table.to_owned(), dir: dir.path.clone(), version: r.version.clone() });
        };
        if r.is_complete() && r.hash != f.hash() {
            return Err(HistoryError::Changed { table: table.to_owned(), file: dir.path.join(&f.name) });
        }
    }
    let row = |f: &File| rows.iter().find(|r| compare_versions(&r.version, &f.version) == Ordering::Equal);
    let last = rows.iter().filter(|r| r.is_complete()).map(|r| r.version.as_str()).max_by(|a, b| compare_versions(a, b));
    let mut status = Status::default();
    for f in dir.files() {
        match row(f) {
            Some(r) if r.is_complete() => status.applied.push(f),
            Some(r) => status.pending.push(Pending { file: f, resume: Some(r.clone()) }),
            None => match last {
                Some(l) if compare_versions(&f.version, l) == Ordering::Less => match order {
                    ExecOrder::Linear => return Err(HistoryError::OutOfOrder { file: dir.path.join(&f.name), last: l.to_owned() }),
                    ExecOrder::LinearSkip => status.skipped.push(f),
                    ExecOrder::NonLinear => status.pending.push(Pending { file: f, resume: None }),
                },
                _ => status.pending.push(Pending { file: f, resume: None }),
            },
        }
    }
    Ok(status)
}

/// SHA-256 (base64) of a statement's text, as recorded in `partial_hashes`.
pub fn statement_hash(sql: &str) -> String {
    BASE64.encode(Sha256::digest(sql.as_bytes()))
}

/// The advisory lock key for `name`: the first 8 bytes of its SHA-256, big endian.
pub fn lock_key(name: &str) -> i64 {
    let d = Sha256::digest(name.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    i64::from_be_bytes(b)
}

/// The lock name used unless one is given: `pgmig:<revisions schema>`, so runs that share a
/// revisions table exclude each other and runs against different ones do not.
pub fn default_lock_name(revisions_schema: &str) -> String {
    format!("pgmig:{revisions_schema}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::revisions::Kind;

    fn dir(files: &[(&str, &str)]) -> Dir {
        let files = files.iter().map(|(n, t)| File::new(n, t.as_bytes()).expect("name")).collect();
        Dir::from_files(Path::new("m"), files).expect("dir")
    }

    fn done(f: &File) -> Revision {
        Revision::start(&f.version, &f.description, Kind::Normal, 0, &f.hash())
    }

    fn names(fs: &[&File]) -> Vec<String> {
        fs.iter().map(|f| f.name.clone()).collect()
    }

    fn pending_names(s: &Status) -> Vec<String> {
        s.pending.iter().map(|p| p.file.name.clone()).collect()
    }

    #[test]
    fn new_files_after_the_last_applied_are_pending() {
        let d = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let rows = vec![done(&d.files()[0])];
        let s = pending(&d, &rows, ExecOrder::Linear, "t").expect("status");
        assert_eq!(names(&s.applied), ["1_a.sql"]);
        assert_eq!(pending_names(&s), ["2_b.sql", "3_c.sql"]);
        let s = pending(&d, &[], ExecOrder::Linear, "t").expect("status");
        assert_eq!(pending_names(&s).len(), 3);
    }

    #[test]
    fn older_files_follow_the_order_policy() {
        let d = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let rows = vec![done(&d.files()[0]), done(&d.files()[2])];
        let err = pending(&d, &rows, ExecOrder::Linear, "t").expect_err("linear");
        assert!(matches!(&err, HistoryError::OutOfOrder { file, last } if file == Path::new("m/2_b.sql") && last == "3"), "{err}");
        let s = pending(&d, &rows, ExecOrder::LinearSkip, "t").expect("skip");
        assert_eq!((names(&s.skipped), pending_names(&s)), (vec!["2_b.sql".to_owned()], vec![]));
        let s = pending(&d, &rows, ExecOrder::NonLinear, "t").expect("non-linear");
        assert_eq!(pending_names(&s), ["2_b.sql"]);
    }

    #[test]
    fn history_must_match_the_directory() {
        let d = dir(&[("1_a.sql", "A"), ("2_b.sql", "B")]);
        let mut gone = done(&d.files()[0]);
        gone.version = "7".into();
        assert!(matches!(pending(&d, &[gone], ExecOrder::NonLinear, "t"), Err(HistoryError::Missing { version, .. }) if version == "7"));
        let mut edited = done(&d.files()[1]);
        edited.hash = d.files()[0].hash();
        assert!(
            matches!(pending(&d, &[edited], ExecOrder::NonLinear, "t"), Err(HistoryError::Changed { file, .. }) if file == Path::new("m/2_b.sql"))
        );
    }

    #[test]
    fn unfinished_rows_resume_even_when_edited_or_older() {
        let d = dir(&[("1_a.sql", "A"), ("2_b.sql", "B"), ("3_c.sql", "C")]);
        let mut failed = Revision::start("2", "b", Kind::Normal, 2, "old");
        failed.fail("boom", "SELECT 1");
        let rows = vec![done(&d.files()[0]), failed.clone(), done(&d.files()[2])];
        let s = pending(&d, &rows, ExecOrder::Linear, "t").expect("status");
        assert_eq!(s.pending, [Pending { file: &d.files()[1], resume: Some(failed) }]);
    }

    #[test]
    fn file_hashes_are_their_own() {
        let a = dir(&[("1_a.sql", "A"), ("2_b.sql", "B")]);
        let b = dir(&[("2_b.sql", "B")]);
        assert_eq!(a.files()[1].hash(), b.files()[0].hash());
        assert_eq!(a.files()[0].hash(), a.sum().lines[0].hash);
        assert_ne!(a.files()[1].hash(), a.sum().lines[1].hash);
    }

    #[test]
    fn lock_keys_are_stable() {
        assert_eq!(lock_key("pgmig:pgmig"), lock_key(&default_lock_name("pgmig")));
        assert_ne!(lock_key("pgmig:a"), lock_key("pgmig:b"));
        assert_eq!(statement_hash("SELECT 1"), statement_hash("SELECT 1"));
        assert_ne!(statement_hash("SELECT 1"), statement_hash("SELECT 2"));
        for (s, ok) in [("linear", true), ("linear-skip", true), ("non-linear", true), ("nonlinear", false)] {
            assert_eq!(ExecOrder::parse(s).map(ExecOrder::as_str) == Some(s), ok);
        }
    }
}
