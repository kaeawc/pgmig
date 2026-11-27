//! `migrate apply`: run a migration directory's pending files against a database.
//!
//! 1. The directory is checked against its sum file, and every file split into statements.
//! 2. A session-level advisory lock is taken on the target ([`ApplyOptions::lock`]): its key is
//!    the first 8 bytes of the SHA-256 of the lock name ([`pgmig_migrate::history::lock_key`]), by
//!    default `pgmig:<revisions schema>`. Advisory locks ignore `lock_timeout`, so pgmig polls
//!    `pg_try_advisory_lock` with backoff until [`ApplyOptions::lock_timeout_ms`] and then fails.
//!    The lock is released at the end, on error too (the session's end releases it anyway).
//! 3. The revisions table is created when missing and compared with the directory
//!    ([`pgmig_migrate::history::pending`]): a hand-edited row, an applied version without its file,
//!    or an applied file that changed refuse the run, as does an older pending file under the
//!    linear order.
//! 4. Each pending file runs in its transaction mode ([`TxMode`]), updating its revisions row as it
//!    goes. `-- pgmig:txmode none` before a file's first statement runs that file without a
//!    transaction whatever the mode.
//!
//! There is no approval prompt: the run applies what is pending. [`ApplyOptions::dry_run`]
//! (`--dry-run`) reports what would run and changes nothing.
//!
//! # Existing databases
//!
//! A database without history (no revisions table, or an empty one) that already holds objects
//! is refused ([`Error::DirtyDatabase`]): running every migration onto it would most likely fail
//! half way, or worse, succeed on top of objects nobody meant to keep. Objects are relations
//! (tables, views, materialized views, sequences, foreign tables, composite types), enums, domains,
//! range types, functions and procedures in a user schema, except those an extension owns and those
//! in the revisions schema; a schema that holds none of them, empty or not, does not count. Two ways
//! out:
//!
//! - [`ApplyOptions::baseline`] (`--baseline <version>`): the database already is the directory up
//!   to that version (typically a baseline file written by `migrate baseline`). Every file up to and
//!   including it is recorded as applied, kind `baseline`, without running it, in one transaction;
//!   the files after it are applied as usual. The version must name a file in every case. The
//!   option only records anything on a database without history that holds objects:
//!   - without history and without objects (a new database), it is ignored and every file runs,
//!     the baseline file included ([`MigrateApplyReport::baseline_skipped`]);
//!   - with history that has the version applied, it is ignored;
//!   - with other history, it is refused (`migrate set` changes history).
//!
//!   So a deploy script can always pass it: the database it adopted is baselined once, and every
//!   other database, new or already migrated, runs the files as usual.
//! - [`ApplyOptions::allow_dirty`] (`--allow-dirty`): apply every file onto it anyway.
//!
//! # Commit points
//!
//! `-- pgmig:commit` on a line of its own between two statements is a commit point. `migrate diff`
//! writes one after `ALTER TYPE ... ADD VALUE` statements when later statements follow, because a
//! new enum value cannot be used in the transaction that adds it (SQLSTATE 55P04). In `file` mode
//! the file's transaction records its progress in the revisions row and commits there, and a new
//! transaction (with the hooks) runs the rest; a failure after a commit point rolls back only the
//! statements after it, and a re-run resumes at it ([`MigrationOutcome::PartlyCommitted`]). `all`
//! mode refuses a pending file with a commit point, as it refuses `-- pgmig:txmode none`; without a
//! transaction every statement commits anyway, so a commit point changes nothing.
//!
//! # Assertions
//!
//! A line `-- pgmig:assert` directly above a statement makes it a pre-migration assertion;
//! `-- pgmig:assert any` makes it part of the file's one-of group. Assertions come before the file's
//! first other statement and are each a single query (`SELECT`, `WITH`, `VALUES`, `TABLE` or a
//! parenthesized one) returning one row of one boolean column. Before a pending file runs (before its revisions row, its hooks and its first
//! statement), and again on every run that starts it, resumes included, they are evaluated on the
//! target in a read-only transaction that is rolled back (with `--tx-mode all`, inside the run's
//! transaction, so they see the earlier files of the run): every plain assertion must be true, and
//! when the group exists at least one `any` assertion must be true; NULL is false. Otherwise the run
//! stops with [`Error::AssertionFailed`]: nothing of the file runs and no row is written; earlier
//! files of the run stay applied (with `--tx-mode all`, the run's transaction is rolled back). An
//! assertion that does not run, or returns another shape, stops the run the same way, as an
//! [`Error::AssertionFailed`] whose [`FailedAssertion::error`] (and `sqlstate`, for a query the
//! server refused) says why; the run's report is kept. A statement that is not a query, or a
//! misplaced or malformed directive, is a [`Error::Directive`] error before anything runs.
//!
//! Assertions must be side-effect free. `READ ONLY` makes the server refuse writes (SQLSTATE 25006,
//! a data-modifying `WITH` included) and `nextval`; with `--tx-mode all` they run in a savepoint
//! made read only and rolled back, so the run's transaction is untouched. It does not stop
//! functions with outside effects (`pg_terminate_backend`, advisory locks, `set_config`, ...):
//! do not call them.
//!
//! They run with the session's settings, not the hooks': in the per-file modes before a file's
//! `before_sql` hook (and its `BEGIN`), with `--tx-mode all` inside the run's transaction after the
//! run's `before_sql`.
//!
//! Assertions are not statements: they are not counted in a revisions row's `total`, `applied` or
//! hashes, nor in the report. The file's hash covers them (they are part of its text). Replay in the
//! shadow database (`migrate diff`, drift checks, `--verify`, fingerprints) skips them, as do
//! `--dry-run`, `status` and `--baseline`: the shadow has no data to check them against.
//!
//! # Hooks
//!
//! [`ApplyOptions::before_sql`] runs right after each `BEGIN`, [`ApplyOptions::after_sql`] right
//! before each `COMMIT`, so `SET LOCAL lock_timeout = '5s'` bounds every statement of the
//! transaction (not a file's assertions in the per-file modes, see above). A file without a transaction runs them at session level, before its first and
//! after its last statement. There `SET LOCAL` has no effect (Postgres warns and ignores it outside
//! a transaction), and a plain `SET` lasts for the rest of the session, into every later file of
//! the run: write hooks with `SET LOCAL`, and set what a non-transactional file needs in the file
//! itself, resetting it at its end.
//!
//! # Connections
//!
//! The advisory lock is held by the session, and a non-transactional file depends on its statements
//! running in one session. Connect directly, or through a pooler in session mode: a pooler in
//! transaction mode can run each transaction on another server connection, so the lock would not
//! exclude other runs and could be released by, or leak to, another client.
//!
//! # Failures
//!
//! A failing statement is reported with its file, line and column and the server's SQLSTATE
//! ([`Error::MigrationFailed`]). Its transaction is rolled back, then the file's row records the
//! error and the failing statement in a transaction of its own (the progress committed at earlier
//! commit points stays). A failure to write the revisions table is reported with its file
//! ([`Error::MigrationRecord`]).
//!
//! Without a transaction, the row is written before each statement, marking it in flight
//! ([`Revision::start_statement`]), and again after it, recording it as applied (`applied` and its
//! hash). A failing statement replaces the marker with its error, and a re-run resumes after the
//! applied statements once it has checked that their text did not change. If the run stops between
//! the two writes (the connection or the process dies, or the second write fails), the row still
//! marks the statement in flight: pgmig cannot tell whether it took effect, so a re-run refuses to
//! resume ([`Error::Resume`]) until someone checks the database and resolves the row (`pgmig migrate
//! set`; see [`crate::migrate_status`]). Each of these writes is a single transaction, so the row is never missing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pgmig_migrate::history::{self, Pending};
use pgmig_migrate::revisions::{Kind, Revision, Table};
use serde::{Deserialize, Serialize};

use crate::{Conn, Error, MigrationDir, Result};

/// How pending files are grouped into transactions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxMode {
    /// Each file in its own transaction, with its revisions row.
    #[default]
    File,
    /// Every pending file in one transaction: all of them or none. Refused when a pending file
    /// says `-- pgmig:txmode none`.
    All,
    /// No transaction: each statement commits on its own and the row records each one.
    None,
}

impl TxMode {
    pub fn parse(s: &str) -> Option<TxMode> {
        match s {
            "file" => Some(TxMode::File),
            "all" => Some(TxMode::All),
            "none" => Some(TxMode::None),
            _ => None,
        }
    }
}

/// Which files older than the newest applied one run ([`history::ExecOrder`]). Serialized as
/// `linear`, `linear-skip`, `non-linear`, like the flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecOrder {
    #[default]
    Linear,
    LinearSkip,
    NonLinear,
}

impl ExecOrder {
    pub fn parse(s: &str) -> Option<ExecOrder> {
        history::ExecOrder::parse(s).map(ExecOrder::from)
    }
}

impl From<history::ExecOrder> for ExecOrder {
    fn from(o: history::ExecOrder) -> ExecOrder {
        match o {
            history::ExecOrder::Linear => ExecOrder::Linear,
            history::ExecOrder::LinearSkip => ExecOrder::LinearSkip,
            history::ExecOrder::NonLinear => ExecOrder::NonLinear,
        }
    }
}

impl From<ExecOrder> for history::ExecOrder {
    fn from(o: ExecOrder) -> history::ExecOrder {
        match o {
            ExecOrder::Linear => history::ExecOrder::Linear,
            ExecOrder::LinearSkip => history::ExecOrder::LinearSkip,
            ExecOrder::NonLinear => history::ExecOrder::NonLinear,
        }
    }
}

/// The default [`ApplyOptions::lock_timeout_ms`].
pub const DEFAULT_LOCK_TIMEOUT_MS: u64 = 10_000;

/// How [`migrate_apply`] runs. Every field may be left out of its JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApplyOptions {
    pub tx_mode: TxMode,
    pub exec_order: ExecOrder,
    /// Take the advisory lock (default true).
    pub lock: bool,
    /// The lock's name; `None` is `pgmig:<revisions_schema>`.
    pub lock_name: Option<String>,
    /// How long to wait for the lock; 0 tries once.
    pub lock_timeout_ms: u64,
    /// SQL run right after each `BEGIN`.
    pub before_sql: Option<String>,
    /// SQL run right before each `COMMIT`.
    pub after_sql: Option<String>,
    /// Apply at most this many pending files.
    pub limit: Option<usize>,
    /// Report what would be applied and change nothing (the revisions table is not created).
    pub dry_run: bool,
    /// The schema of the revisions table.
    pub revisions_schema: String,
    /// On a database without history that holds objects, record the files up to and including
    /// this version (exact version or file name) as applied without running them; ignored on an
    /// empty database, which runs every file (see the module docs).
    pub baseline: Option<String>,
    /// Apply onto a database without history that already holds objects, instead of refusing.
    pub allow_dirty: bool,
}

impl Default for ApplyOptions {
    fn default() -> ApplyOptions {
        ApplyOptions {
            tx_mode: TxMode::File,
            exec_order: ExecOrder::Linear,
            lock: true,
            lock_name: None,
            lock_timeout_ms: DEFAULT_LOCK_TIMEOUT_MS,
            before_sql: None,
            after_sql: None,
            limit: None,
            dry_run: false,
            revisions_schema: pgmig_migrate::revisions::DEFAULT_SCHEMA.to_owned(),
            baseline: None,
            allow_dirty: false,
        }
    }
}

impl ApplyOptions {
    /// The lock name in use.
    pub fn lock_name(&self) -> String {
        self.lock_name.clone().unwrap_or_else(|| history::default_lock_name(&self.revisions_schema))
    }
}

/// What became of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// Applied by an earlier run.
    AlreadyApplied,
    /// Applied by this run.
    Applied,
    /// Left to apply: a dry run, or beyond the limit.
    Pending,
    /// Older than the newest applied file and left out (`linear-skip`).
    Skipped,
    /// Recorded as applied by this run without running it (`--baseline`); with `dry_run`, would be.
    Baselined,
    /// A statement of it (or its own bookkeeping) failed in this run. Only in the report of a
    /// failure.
    Failed,
    /// This run ran it in the one transaction of `--tx-mode all`, which was rolled back (because a
    /// later file failed, or `after_sql` or `COMMIT` failed). Only in the report of a failure.
    RolledBack,
    /// This run ran it in the one transaction of `--tx-mode all` and `COMMIT`'s outcome is not
    /// known (the connection was lost): it may or may not be applied. Only in the report of a
    /// failure.
    Unknown,
}

/// One migration file in a [`MigrateApplyReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReport {
    pub file: String,
    pub version: String,
    pub status: FileStatus,
    /// How it runs (or ran): `none` for a file that says `-- pgmig:txmode none`. Null for files
    /// that are not run.
    pub tx_mode: Option<TxMode>,
    /// Statements run (or to run) by this run.
    pub statements: usize,
    /// Statements an earlier, interrupted run applied, which this run resumes after.
    pub resumed_after: usize,
    pub execution_ms: u64,
    /// The statements this run ran, in order, each with its time. Without the SQL text (it may hold
    /// secrets): the hash is the one the revisions table keeps. After a failure, the statements
    /// before the failing one; empty for a file that did not run.
    pub statement_timings: Vec<StatementTiming>,
}

/// One statement a run ran, in a [`FileReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementTiming {
    /// 0-based index in its file.
    pub index: usize,
    /// 1-based line in its file where the statement starts.
    pub line: u32,
    /// The statement's hash, as the revisions table records it.
    pub hash: String,
    pub execution_ms: u64,
}

/// What [`migrate_apply`] did, or with `dry_run` would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateApplyReport {
    pub dry_run: bool,
    /// The files that were pending when the run started (to run, or beyond the limit), in the
    /// order they run.
    pub pending: Vec<String>,
    /// Every file of the directory, in version order.
    pub files: Vec<FileReport>,
    /// Files applied by this run.
    pub applied: usize,
    /// Statements run by this run.
    pub statements: usize,
    /// Time from the first statement of the run to its end, hooks and bookkeeping included.
    pub execution_ms: u64,
    /// `baseline` was given and ignored because the database has no history and holds no objects:
    /// every file runs (or with `dry_run`, would run) instead.
    pub baseline_skipped: bool,
}

/// What became of the work when a migration failed, serialized in snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationOutcome {
    /// The file's transaction was rolled back; files before it stay applied.
    FileRolledBack,
    /// The run's one transaction was rolled back: nothing of this run is applied.
    AllRolledBack,
    /// The file's transaction since its last commit point (`-- pgmig:commit`) was rolled back; the
    /// statements committed before it stay applied, and running again resumes after them.
    PartlyCommitted,
    /// No transaction: the file's statements before the failing one stay applied, and running
    /// again resumes after them.
    Partial,
    /// The connection failed during `COMMIT`: the file may or may not be applied.
    Unknown,
    /// No transaction: a statement ran but could not be recorded as applied. Its row still marks it
    /// in flight, and running again refuses to resume until that is resolved.
    InFlight,
}

pub(crate) fn outcome_message(outcome: MigrationOutcome, applied_files: usize) -> String {
    let earlier = match applied_files {
        0 => String::new(),
        1 => "; 1 earlier file of this run stays applied".to_owned(),
        n => format!("; {n} earlier files of this run stay applied"),
    };
    match outcome {
        MigrationOutcome::FileRolledBack => format!("the file's transaction was rolled back{earlier}"),
        MigrationOutcome::AllRolledBack => "the run's transaction was rolled back: nothing was applied".to_owned(),
        MigrationOutcome::PartlyCommitted => format!(
            "the file's transaction since its last commit point was rolled back; the statements committed before it stay applied, and running again resumes after them{earlier}"
        ),
        MigrationOutcome::Partial => {
            format!(
                "no transaction: the statements before it stay applied. To recover, fix it or anything after it (statements before it must not change), run `pgmig migrate hash`, and run apply again: it resumes at the failing statement. Or record the file as complete with `pgmig migrate set <version> --force`{earlier}"
            )
        }
        MigrationOutcome::Unknown => {
            format!("the connection failed during COMMIT, so the file may or may not be applied; check the revisions table{earlier}")
        }
        MigrationOutcome::InFlight => format!(
            "no transaction: the statement ran but is not recorded as applied; its revisions row marks it in flight, so running again stops until you check whether it took effect{earlier}"
        ),
    }
}

/// A file ready to run.
struct Prepared<'a> {
    file: &'a pgmig_migrate::File,
    path: PathBuf,
    sql: pgmig_sql::File,
    /// `-- pgmig:txmode none`.
    no_tx: bool,
    /// Commit points (`-- pgmig:commit`): the index of the statement each one comes before, and
    /// where the directive is. Sorted, without duplicates, never 0 or past the last statement.
    commits: Vec<(usize, pgmig_sql::Pos)>,
    /// `-- pgmig:assert` statements, taken out of `sql`.
    assertions: Vec<Assertion>,
}

/// A pre-migration assertion: a statement taken out of its file.
pub(crate) struct Assertion {
    pos: pgmig_sql::Pos,
    sql: String,
    /// `-- pgmig:assert any`.
    any: bool,
}

/// An assertion that was not true or could not be evaluated, in [`Error::AssertionFailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedAssertion {
    /// 1-based line in its file where the statement starts.
    pub line: u32,
    pub statement: String,
    /// What it returned: `Some(false)`, or `None` for NULL.
    pub value: Option<bool>,
    /// It belongs to the `-- pgmig:assert any` group, of which none was true.
    pub any: bool,
    /// The assertion did not evaluate to a value: the server's message for a query that failed
    /// (see `sqlstate`), or why its result has the wrong shape. Then `value` is `None`, and
    /// evaluation stopped at this assertion, `any` or not.
    pub error: Option<String>,
    /// The server's SQLSTATE for a query that failed.
    pub sqlstate: Option<String>,
}

impl Prepared<'_> {
    fn commits_before(&self, index: usize) -> bool {
        self.commits.iter().any(|(i, _)| *i == index)
    }
}

/// A pending file and where it starts.
struct Job<'a, 'b> {
    p: &'b Prepared<'a>,
    resume: Option<Revision>,
    mode: TxMode,
}

impl Job<'_, '_> {
    fn start(&self) -> usize {
        self.resume.as_ref().map_or(0, |r| r.applied as usize)
    }

    /// The row as the file starts: an earlier run's progress, or nothing.
    fn revision(&self) -> Revision {
        let f = self.p.file;
        let total = u32::try_from(self.p.sql.statements.len()).unwrap_or(u32::MAX);
        let mut rev = Revision::start(&f.version, &f.description, Kind::Normal, total, &f.hash());
        if let Some(r) = &self.resume {
            rev.applied = r.applied;
            rev.partial_hashes.clone_from(&r.partial_hashes);
        }
        rev
    }
}

/// Why a step of the run failed, before it is turned into an [`Error`].
enum Failure {
    Statement {
        index: usize,
        source: pgmig_conn::Error,
    },
    Step {
        step: String,
        source: pgmig_conn::Error,
    },
    Revisions(pgmig_migrate::revisions::Error),
    /// An assertion failed or could not be evaluated (`--tx-mode all`, inside the run's transaction).
    Assertion(Box<Error>),
    /// Without a transaction: statement `index` ran, and recording it failed.
    Record {
        index: usize,
        source: pgmig_migrate::revisions::Error,
    },
}

impl From<pgmig_migrate::revisions::Error> for Failure {
    fn from(e: pgmig_migrate::revisions::Error) -> Failure {
        Failure::Revisions(e)
    }
}

/// `migrate apply`: apply the pending files of the migration directory at `dir` to the database at
/// `target` (see the module docs).
pub fn migrate_apply(target: &str, dir: &Path, opts: &ApplyOptions) -> Result<MigrateApplyReport> {
    if opts.revisions_schema.is_empty() {
        return Err(Error::Options("revisions_schema must be a schema name, not an empty string".into()));
    }
    if opts.lock_name.as_deref() == Some("") {
        return Err(Error::Options("the lock name must not be empty".into()));
    }
    let dir = MigrationDir::open(dir)?;
    pgmig_migrate::validate(&dir)?;
    crate::check_no_imports(&dir)?;
    let prepared = dir.files().iter().map(|f| prepare(&dir, f)).collect::<Result<Vec<_>>>()?;
    let mut conn = Conn::connect(target)?;
    let key = history::lock_key(&opts.lock_name());
    if opts.lock {
        lock(&mut conn, &opts.lock_name(), key, Duration::from_millis(opts.lock_timeout_ms))?;
    }
    let res = locked(&mut conn, &dir, &prepared, opts);
    if opts.lock {
        // The session's end releases it too; an error here would only hide the result.
        let _ = conn.query(&format!("SELECT pg_advisory_unlock({key})"));
    }
    res
}

/// Split `f` and read its `-- pgmig:txmode`, `-- pgmig:commit` and `-- pgmig:assert` directives.
fn prepare<'a>(dir: &MigrationDir, f: &'a pgmig_migrate::File) -> Result<Prepared<'a>> {
    let path = dir.path.join(&f.name);
    let text = std::str::from_utf8(&f.bytes)
        .map_err(|e| Error::Io { path: path.clone(), source: std::io::Error::new(std::io::ErrorKind::InvalidData, e) })?;
    let (sql, assertions) = split_migration_text(&path, text)?;
    let (no_tx, commits) = check_directives(&path, &sql)?;
    Ok(Prepared { file: f, path, sql, no_tx, commits, assertions })
}

/// Split the text of a migration file named `path` and take its assertions out of the statements
/// ([`take_assertions`]), so that statement indexes, counts and hashes never see them.
pub(crate) fn split_migration_text(path: &Path, text: &str) -> Result<(pgmig_sql::File, Vec<Assertion>)> {
    let mut sql = pgmig_sql::split(text).map_err(|source| Error::Split { path: path.to_owned(), source })?;
    let assertions = take_assertions(path, &mut sql)?;
    Ok((sql, assertions))
}

/// Move the `-- pgmig:assert` statements of `sql` out of its statements. They must be directly
/// above their statement, come before the first other statement, take no argument or `any`, and
/// be a query (`SELECT`, `WITH`, `VALUES`, `TABLE` or `(`). Other directives on an assertion move to the file's.
fn take_assertions(path: &Path, sql: &mut pgmig_sql::File) -> Result<Vec<Assertion>> {
    let bad = |pos, message: &str| Error::Directive { path: path.to_owned(), pos, message: message.to_owned() };
    if let Some(d) = sql.directives.iter().filter(|d| d.name == "assert").min_by_key(|d| d.pos) {
        return Err(bad(d.pos, "pgmig:assert must be on the line directly above its statement (no blank line between)"));
    }
    let mut assertions = Vec::new();
    let mut kept = Vec::new();
    let mut moved = Vec::new();
    for s in std::mem::take(&mut sql.statements) {
        let Some(d) = s.directives.iter().find(|d| d.name == "assert") else {
            kept.push(s);
            continue;
        };
        if s.directives.iter().filter(|d| d.name == "assert").count() > 1 {
            return Err(bad(d.pos, "a statement takes one pgmig:assert"));
        }
        if d.pos > s.pos {
            return Err(bad(d.pos, "pgmig:assert must be on the line directly above its statement, not inside one"));
        }
        if !kept.is_empty() {
            return Err(bad(d.pos, "pgmig:assert statements must come before the file's first other statement"));
        }
        let any = match d.args.as_str() {
            "" => false,
            "any" => true,
            _ => return Err(bad(d.pos, "pgmig:assert takes no argument, or `any`")),
        };
        let text = s.sql.trim_start();
        let word = text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next().unwrap_or("");
        if !(text.starts_with('(') || ["select", "with", "values", "table"].iter().any(|w| word.eq_ignore_ascii_case(w))) {
            return Err(bad(
                s.pos,
                "assertion error: an assertion must be a query (SELECT, WITH, VALUES, TABLE or a parenthesized query) returning one row with one boolean column",
            ));
        }
        assertions.push(Assertion { pos: s.pos, sql: s.sql, any });
        moved.extend(s.directives.into_iter().filter(|d| d.name != "assert"));
    }
    sql.statements = kept;
    sql.directives.extend(moved);
    Ok(assertions)
}

/// Evaluate the assertions of `p` (see the module docs), read-only and without leaving a trace.
/// `in_transaction`: the run's read-write transaction is open: they run in a savepoint made read
/// only and rolled back, so even a data-modifying `WITH` cannot change what the run commits.
/// Otherwise they run in a read-only transaction that is rolled back.
fn check_assertions(conn: &mut Conn, p: &Prepared, in_transaction: bool, applied_files: usize) -> Result<()> {
    if p.assertions.is_empty() {
        return Ok(());
    }
    if in_transaction {
        conn.batch("SAVEPOINT pgmig_assert")?;
        if let Err(e) = conn.batch("SET TRANSACTION READ ONLY") {
            let _ = conn.batch("ROLLBACK TO SAVEPOINT pgmig_assert");
            return Err(e.into());
        }
    } else {
        conn.batch("BEGIN READ ONLY")?;
    }
    let res = evaluate(conn, p, in_transaction, applied_files);
    if in_transaction {
        // Undoes the read-only setting too; works in an aborted transaction.
        let cleanup = conn.batch("ROLLBACK TO SAVEPOINT pgmig_assert").and_then(|()| conn.batch("RELEASE SAVEPOINT pgmig_assert"));
        if res.is_ok() {
            cleanup?;
        }
    } else {
        let _ = conn.batch("ROLLBACK");
    }
    res
}

fn evaluate(conn: &mut Conn, p: &Prepared, in_transaction: bool, applied_files: usize) -> Result<()> {
    // An assertion that cannot run, or returns another shape, stops the run like a false one.
    let broken = |a: &Assertion, error: String, sqlstate: Option<String>| {
        let failed = FailedAssertion { line: a.pos.line, statement: a.sql.clone(), value: None, any: a.any, error: Some(error), sqlstate };
        assertion_error(p, vec![failed], in_transaction, applied_files)
    };
    let mut group: Vec<FailedAssertion> = Vec::new();
    let mut group_passed = false;
    for a in &p.assertions {
        let rows = match conn.query(&a.sql) {
            Ok(rows) => rows,
            Err(source) => return Err(broken(a, source.message.clone(), source.code.clone())),
        };
        let value = match rows.as_slice() {
            [row] if row.len() == 1 => match row[0].as_deref() {
                Some("t") => Some(true),
                Some("f") => Some(false),
                None => None,
                Some(other) => return Err(broken(a, format!("an assertion must return a boolean, not {other:?}"), None)),
            },
            _ => {
                let columns = rows.first().map_or(0, Vec::len);
                return Err(broken(
                    a,
                    format!("an assertion must return one row with one boolean column, not {} row(s) of {columns} column(s)", rows.len()),
                    None,
                ));
            }
        };
        let failed = FailedAssertion { line: a.pos.line, statement: a.sql.clone(), value, any: a.any, error: None, sqlstate: None };
        match (a.any, value) {
            (false, Some(true)) => {}
            (false, _) => return Err(assertion_error(p, vec![failed], in_transaction, applied_files)),
            (true, Some(true)) => group_passed = true,
            (true, _) => group.push(failed),
        }
    }
    if !group.is_empty() && !group_passed {
        return Err(assertion_error(p, group, in_transaction, applied_files));
    }
    Ok(())
}

fn assertion_error(p: &Prepared, failures: Vec<FailedAssertion>, in_transaction: bool, applied_files: usize) -> Error {
    // `--tx-mode all` is the one mode that evaluates inside the run's transaction.
    let outcome = if in_transaction { MigrationOutcome::AllRolledBack } else { MigrationOutcome::FileRolledBack };
    Error::AssertionFailed { path: p.path.clone(), failures, outcome, applied_files, report: Box::default() }
}

pub(crate) fn assertion_message(path: &Path, failures: &[FailedAssertion], outcome: MigrationOutcome, applied_files: usize) -> String {
    let value = |f: &FailedAssertion| if f.value.is_none() { "NULL" } else { "false" };
    let mut out = match failures {
        [f] if f.error.is_some() => {
            let state = f.sqlstate.as_deref().map(|c| format!(" (SQLSTATE {c})")).unwrap_or_default();
            let error = f.error.as_deref().unwrap_or_default();
            format!("{}:{}: assertion error: {}: {error}{state}", path.display(), f.line, f.statement)
        }
        [f] if !f.any => format!("{}:{}: assertion failed: {} returned {}", path.display(), f.line, f.statement, value(f)),
        _ => {
            let mut m = format!("{}: none of the `-- pgmig:assert any` assertions was true:", path.display());
            for f in failures {
                m.push_str(&format!("\n  line {}: {} returned {}", f.line, f.statement, value(f)));
            }
            m
        }
    };
    out.push('\n');
    out.push_str(&match (outcome, applied_files) {
        (MigrationOutcome::AllRolledBack, _) => "the run's transaction was rolled back: nothing was applied".to_owned(),
        (_, 0) => "no statement of the file ran and no revisions row was written".to_owned(),
        (_, 1) => "no statement of the file ran and no revisions row was written; 1 earlier file of this run stays applied".to_owned(),
        (_, n) => format!("no statement of the file ran and no revisions row was written; {n} earlier files of this run stay applied"),
    });
    out
}

/// The `-- pgmig:txmode` and `-- pgmig:commit` directives of the file at `path`: whether it runs
/// without a transaction, and its commit points ([`commit_points`]). A `-- pgmig:notx` is refused.
pub(crate) fn check_directives(path: &Path, sql: &pgmig_sql::File) -> Result<(bool, Vec<(usize, pgmig_sql::Pos)>)> {
    // `-- pgmig:notx` marks a statement in the text of a plan (`schema diff`, `--edit`); a file runs
    // in one mode, so running it as written would put the statement in a transaction.
    let all = sql.directives.iter().chain(sql.statements.iter().flat_map(|s| &s.directives));
    if let Some(d) = all.filter(|d| d.name == "notx").min_by_key(|d| d.pos) {
        return Err(Error::Directive {
            path: path.to_owned(),
            pos: d.pos,
            message: "pgmig:notx belongs to the text of a plan, not to a migration file: put the statement in a file of its own \
                      that starts with `-- pgmig:txmode none` (`migrate diff` writes such files)"
                .to_owned(),
        });
    }
    let first = sql.statements.first().map(|s| s.pos);
    let mut no_tx = false;
    let on_statements = sql.statements.iter().enumerate().flat_map(|(i, s)| s.directives.iter().map(move |d| (i > 0, d)));
    let on_file = sql.directives.iter().map(|d| (first.is_some_and(|p| d.pos > p), d));
    for (late, d) in on_file.chain(on_statements).filter(|(_, d)| d.name == "txmode") {
        let message = if late {
            "pgmig:txmode applies to the whole file and must come before its first statement"
        } else if d.args != "none" {
            "pgmig:txmode takes one value: `none` (run this file without a transaction)"
        } else {
            no_tx = true;
            continue;
        };
        return Err(Error::Directive { path: path.to_owned(), pos: d.pos, message: message.to_owned() });
    }
    let commits =
        commit_points(sql).map_err(|(pos, message)| Error::Directive { path: path.to_owned(), pos, message: message.to_owned() })?;
    Ok((no_tx, commits))
}

/// The `-- pgmig:commit` directives of `sql`: the statement each comes before. One inside a
/// statement, or with arguments, is refused; one before the first or after the last statement
/// commits nothing and is left out.
pub(crate) fn commit_points(sql: &pgmig_sql::File) -> std::result::Result<Vec<(usize, pgmig_sql::Pos)>, (pgmig_sql::Pos, &'static str)> {
    let mut out = Vec::new();
    let on_statements = sql.statements.iter().enumerate().flat_map(|(i, s)| s.directives.iter().map(move |d| (i, d, d.pos > s.pos)));
    let on_file =
        sql.directives.iter().map(|d| (sql.statements.iter().position(|s| s.pos > d.pos).unwrap_or(sql.statements.len()), d, false));
    for (i, d, inside) in on_statements.chain(on_file).filter(|(_, d, _)| d.name == "commit") {
        if inside {
            return Err((d.pos, "pgmig:commit must be on a line of its own between two statements, not inside one"));
        }
        if !d.args.is_empty() {
            return Err((d.pos, "pgmig:commit takes no arguments"));
        }
        if i > 0 && i < sql.statements.len() {
            out.push((i, d.pos));
        }
    }
    out.sort_by_key(|(i, _)| *i);
    out.dedup_by_key(|(i, _)| *i);
    Ok(out)
}

/// A run of a file's statements: in one transaction, or each committing on its own.
pub(crate) struct Segment {
    /// The statements, by index in the file.
    pub(crate) range: std::ops::Range<usize>,
    pub(crate) transaction: bool,
}

/// How `migrate apply` lays out a file of `len` statements: with `no_tx` one segment run without a
/// transaction; otherwise one transaction per stretch between commit points (see [`commit_points`]).
pub(crate) fn segments(no_tx: bool, commits: &[(usize, pgmig_sql::Pos)], len: usize) -> Vec<Segment> {
    if no_tx {
        return vec![Segment { range: 0..len, transaction: false }];
    }
    let mut out = Vec::new();
    let mut start = 0;
    for end in commits.iter().map(|(i, _)| *i).chain(std::iter::once(len)) {
        if end > start {
            out.push(Segment { range: start..end, transaction: true });
        }
        start = end;
    }
    out
}

/// Run a migration file as `migrate apply` does, without its revision row, hooks or lock: its
/// directives are checked, each stretch between commit points runs in a transaction of its own and a
/// `-- pgmig:txmode none` file runs one statement at a time with none. An error names the file and
/// the statement.
pub(crate) fn run_file(conn: &mut Conn, path: &Path, file: &pgmig_sql::File) -> Result<()> {
    let (no_tx, commits) = check_directives(path, file)?;
    for seg in segments(no_tx, &commits, file.statements.len()) {
        if seg.transaction {
            conn.batch("BEGIN").map_err(|source| Error::Load {
                path: path.to_owned(),
                pos: file.statements[seg.range.start].pos,
                source,
            })?;
        }
        for stmt in &file.statements[seg.range.clone()] {
            if let Err(source) = conn.batch(&stmt.sql) {
                if seg.transaction {
                    let _ = conn.batch("ROLLBACK");
                }
                return Err(Error::Load { path: path.to_owned(), pos: source.position.map_or(stmt.pos, |n| stmt.locate(n)), source });
            }
        }
        if seg.transaction {
            conn.batch("COMMIT").map_err(|source| Error::Load {
                path: path.to_owned(),
                pos: file.statements[seg.range.end - 1].pos,
                source,
            })?;
        }
    }
    Ok(())
}

/// Take the session-level advisory lock `key`, polling until `timeout`.
pub(crate) fn lock(conn: &mut Conn, name: &str, key: i64, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut wait = Duration::from_millis(20);
    loop {
        let rows = conn.query(&format!("SELECT pg_try_advisory_lock({key})"))?;
        if rows.first().and_then(|r| r.first()).and_then(Option::as_deref) == Some("t") {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::LockTimeout {
                name: name.to_owned(),
                key,
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            });
        }
        std::thread::sleep(wait.min(deadline - now));
        wait = (wait * 2).min(Duration::from_secs(1));
    }
}

/// The run, with the lock held.
fn locked(conn: &mut Conn, dir: &MigrationDir, prepared: &[Prepared], opts: &ApplyOptions) -> Result<MigrateApplyReport> {
    let prepared_of = |f: &pgmig_migrate::File| prepared.iter().find(|p| std::ptr::eq(p.file, f)).expect("every file is prepared");
    let before = Table::open(conn, &opts.revisions_schema)?;
    let history = match &before {
        Some(t) => t.read(conn)?,
        None => Vec::new(),
    };
    let mut baselined: Vec<Revision> =
        baseline_files(dir, &history, opts)?.into_iter().map(|f| baseline_revision(prepared_of(f))).collect();
    let mut baseline_skipped = false;
    // Without history: a baseline applies only to a database that holds objects; an empty one
    // runs every file. Without a baseline, one that holds objects is refused unless allowed.
    if history.is_empty() && !dir.files().is_empty() && (!baselined.is_empty() || !opts.allow_dirty) {
        let objects = existing_objects(conn, &opts.revisions_schema)?;
        if objects.is_empty() {
            baseline_skipped = !baselined.is_empty();
            baselined.clear();
        } else if baselined.is_empty() {
            return Err(Error::DirtyDatabase { count: objects.len(), objects: objects.into_iter().take(5).collect() });
        }
    }
    let table = if opts.dry_run { before } else { Some(Table::ensure(conn, &opts.revisions_schema)?) };
    let rows = match &table {
        _ if opts.dry_run => history.into_iter().chain(baselined.iter().cloned()).collect(),
        Some(t) => {
            if !baselined.is_empty() {
                record_baseline(conn, t, &baselined)?;
            }
            t.read(conn)?
        }
        None => Vec::new(),
    };
    let table_name = table.as_ref().map_or_else(|| format!("{}.pgmig_revisions", opts.revisions_schema), Table::name);
    let status = history::pending(dir, &rows, opts.exec_order.into(), &table_name)?;
    let limit = opts.limit.unwrap_or(usize::MAX);
    let mut jobs = Vec::new();
    let mut waiting = Vec::new();
    for (i, Pending { file, resume }) in status.pending.into_iter().enumerate() {
        let p = prepared_of(file);
        if i >= limit {
            waiting.push(p);
            continue;
        }
        let mode = if p.no_tx { TxMode::None } else { opts.tx_mode };
        if opts.tx_mode == TxMode::All && p.no_tx {
            return Err(Error::TxModeConflict { path: p.path.clone(), reason: "says -- pgmig:txmode none".to_owned() });
        }
        if let (TxMode::All, Some((_, pos))) = (opts.tx_mode, p.commits.first()) {
            return Err(Error::TxModeConflict {
                path: p.path.clone(),
                reason: format!("has a commit point (-- pgmig:commit) at line {}", pos.line),
            });
        }
        if let Some(r) = &resume {
            check_resume(p, r)?;
        }
        jobs.push(Job { p, resume, mode });
    }

    let mut reports: Vec<FileReport> = Vec::new();
    let report_of = |p: &Prepared, status, tx_mode, statements, resumed_after| FileReport {
        file: p.file.name.clone(),
        version: p.file.version.clone(),
        status,
        tx_mode,
        statements,
        resumed_after,
        execution_ms: 0,
        statement_timings: Vec::new(),
    };
    for f in &status.applied {
        let now = baselined.iter().any(|r| r.version == f.version);
        reports.push(report_of(prepared_of(f), if now { FileStatus::Baselined } else { FileStatus::AlreadyApplied }, None, 0, 0));
    }
    for f in &status.skipped {
        reports.push(report_of(prepared_of(f), FileStatus::Skipped, None, 0, 0));
    }
    for p in &waiting {
        reports.push(report_of(p, FileStatus::Pending, None, p.sql.statements.len(), 0));
    }
    let job_status = if opts.dry_run { FileStatus::Pending } else { FileStatus::Applied };
    let first_job = reports.len();
    for j in &jobs {
        let start = j.start();
        reports.push(report_of(j.p, job_status, Some(j.mode), j.p.sql.statements.len().saturating_sub(start), start));
    }

    let pending = jobs.iter().map(|j| j.p.file.name.clone()).chain(waiting.iter().map(|p| p.file.name.clone())).collect();
    let mut out =
        MigrateApplyReport { dry_run: opts.dry_run, pending, files: reports, applied: 0, statements: 0, execution_ms: 0, baseline_skipped };
    let mut failure = None;
    if !opts.dry_run && !jobs.is_empty() {
        let table = table.expect("ensured");
        let started = Instant::now();
        let res = run(conn, &table, &jobs, opts, &mut out.files[first_job..]);
        out.execution_ms = elapsed(started);
        let ran = &out.files[first_job..];
        out.applied = ran.iter().filter(|r| r.status == FileStatus::Applied).count();
        out.statements = ran.iter().map(|r| r.statement_timings.len()).sum();
        failure = res.err();
    }
    out.files.sort_by(|a, b| pgmig_migrate::compare_versions(&a.version, &b.version));
    match failure {
        Some(e) => Err(e.with_report(out)),
        None => Ok(out),
    }
}

/// The files `--baseline` would record: on a database without history, every file up to and
/// including the baseline version (the caller drops them when the database holds no objects); none
/// when the option is not given, or when the database already has that version applied. A database
/// with other history is refused.
fn baseline_files<'a>(dir: &'a MigrationDir, history: &[Revision], opts: &ApplyOptions) -> Result<Vec<&'a pgmig_migrate::File>> {
    let Some(key) = &opts.baseline else { return Ok(Vec::new()) };
    let target = crate::exact_file(dir, key)?;
    let same = |v: &str| pgmig_migrate::compare_versions(v, &target.version).is_eq();
    if history.iter().any(|r| r.is_complete() && same(&r.version)) {
        return Ok(Vec::new());
    }
    if !history.is_empty() {
        return Err(Error::Options(format!(
            "--baseline {key}: the database already has migration history without version {}; a baseline only applies to a database \
             without history (see `pgmig migrate status`, and `pgmig migrate set` to change history)",
            target.version
        )));
    }
    Ok(dir.files().iter().filter(|f| pgmig_migrate::compare_versions(&f.version, &target.version).is_le()).collect())
}

/// The row of a file recorded by `--baseline`: complete, kind baseline, never run.
fn baseline_revision(p: &Prepared) -> Revision {
    let f = p.file;
    let total = u32::try_from(p.sql.statements.len()).unwrap_or(u32::MAX);
    let mut rev = Revision::start(&f.version, &f.description, Kind::Baseline, total, &f.hash());
    rev.applied = total;
    rev
}

/// Insert the baseline rows in one transaction.
fn record_baseline(conn: &mut Conn, table: &Table, rows: &[Revision]) -> Result<()> {
    conn.batch("BEGIN")?;
    let res = rows.iter().try_for_each(|r| table.insert(conn, r)).map_err(Error::from).and_then(|()| Ok(conn.batch("COMMIT")?));
    if res.is_err() {
        let _ = conn.batch("ROLLBACK");
    }
    res
}

/// The objects a database without history already holds (see the module docs), as `table
/// public.users`, sorted: every relation (table, view, materialized view, sequence, foreign table,
/// composite type), enum, domain, range type, function and procedure in a user schema (the schemas
/// inspection reads), except those an extension owns and those in the revisions schema. Indexes,
/// constraints and triggers belong to a table and are left out. A schema holding none of these
/// does not count, nor does an empty one.
fn existing_objects(conn: &mut Conn, revisions_schema: &str) -> Result<Vec<String>> {
    let user = format!(
        "n.nspname NOT IN ('pg_catalog', 'information_schema') AND left(n.nspname, 3) <> 'pg_' AND n.nspname <> {}",
        pgmig_model::quote_literal(revisions_schema)
    );
    let not_extension = |catalog: &str, alias: &str| {
        format!(
            "NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = '{catalog}'::regclass AND d.objid = {alias}.oid AND d.deptype = 'e')"
        )
    };
    let queries = [
        format!(
            "SELECT c.relkind, n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f', 'c') AND {user} AND {}",
            not_extension("pg_class", "c")
        ),
        format!(
            "SELECT t.typtype, n.nspname, t.typname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE t.typtype IN ('e', 'd', 'r') AND {user} AND {}",
            not_extension("pg_type", "t")
        ),
        format!(
            "SELECT p.prokind, n.nspname, p.proname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE {user} AND {}",
            not_extension("pg_proc", "p")
        ),
    ];
    let mut objects = Vec::new();
    for (i, sql) in queries.iter().enumerate() {
        for row in conn.query(sql)? {
            let col = |j: usize| row.get(j).cloned().flatten().unwrap_or_default();
            let what = match (i, col(0).as_str()) {
                (0, "v") => "view",
                (0, "m") => "materialized view",
                (0, "S") => "sequence",
                (0, "f") => "foreign table",
                (0, "c") | (1, _) => "type",
                (0, _) => "table",
                (_, "p") => "procedure",
                _ => "function",
            };
            objects.push((col(1), col(2), what));
        }
    }
    objects.sort();
    Ok(objects.into_iter().map(|(s, n, what)| format!("{what} {}", pgmig_model::QualName::new(s, n))).collect())
}

/// An interrupted file resumes only when the statements it recorded are still its first ones.
fn check_resume(p: &Prepared, r: &Revision) -> Result<()> {
    let applied = r.applied as usize;
    let refuse = |reason: String| Err(Error::Resume { path: p.path.clone(), reason });
    if let Some(i) = r.in_flight() {
        let line = p.sql.statements.get(i).map_or_else(String::new, |s| format!(" (line {})", s.pos.line));
        return refuse(format!(
            "statement {}{line} was started without a transaction by a run that stopped before recording whether it finished; \
             check whether it took effect, finish the file by hand and record it with `pgmig migrate set <version>`, or undo what it \
             applied and drop the row with `pgmig migrate set` and the version before it (see `pgmig migrate status`)",
            i + 1
        ));
    }
    if applied > p.sql.statements.len() {
        return refuse(format!("{applied} statements are recorded as applied, but the file now has {}", p.sql.statements.len()));
    }
    if r.partial_hashes.len() != applied {
        return refuse(format!("{applied} statements are recorded as applied without their hashes; fix the row with `pgmig migrate set`"));
    }
    for (i, (want, s)) in r.partial_hashes.iter().zip(&p.sql.statements).enumerate() {
        if *want != history::statement_hash(&s.sql) {
            return refuse(format!(
                "statement {} (line {}) was applied and has changed since; restore it (only statements after the applied ones may change), or accept the change by recording the file with `pgmig migrate set <version> --force`",
                i + 1,
                s.pos.line
            ));
        }
    }
    Ok(())
}

/// Run the jobs, recording in `reports` (one per job) each file's time and statement timings; after
/// a failure, what became of each file.
fn run(conn: &mut Conn, table: &Table, jobs: &[Job], opts: &ApplyOptions, reports: &mut [FileReport]) -> Result<()> {
    if opts.tx_mode == TxMode::All {
        let started = Instant::now();
        let mut current: Option<usize> = None;
        let mut file_started = started;
        let mut in_file = false;
        let res = (|| -> std::result::Result<(), Failure> {
            step(conn, "BEGIN")?;
            hook(conn, "before_sql", opts.before_sql.as_deref())?;
            for (i, j) in jobs.iter().enumerate() {
                let t = Instant::now();
                file_started = t;
                current = Some(i);
                in_file = true;
                check_assertions(conn, j.p, true, 0).map_err(|e| Failure::Assertion(Box::new(e)))?;
                let mut rev = j.revision();
                let mut committed = rev.clone();
                table.replace(conn, &rev, true)?;
                // Files with commit points are refused in this mode, so this never commits.
                let done = in_transaction(conn, table, j, &mut rev, &mut committed, opts, t, &mut reports[i]);
                reports[i].execution_ms = elapsed(t);
                done?;
                in_file = false;
            }
            hook(conn, "after_sql", opts.after_sql.as_deref())?;
            step(conn, "COMMIT")
        })();
        return res.map_err(|f| {
            let _ = conn.batch("ROLLBACK");
            let outcome = commit_outcome(&f).unwrap_or(MigrationOutcome::AllRolledBack);
            // The files ran in one transaction: none of them is applied. When COMMIT's outcome is
            // unknown, each may be. A failure in `after_sql` or `COMMIT` is not any file's own.
            let status = if outcome == MigrationOutcome::Unknown { FileStatus::Unknown } else { FileStatus::RolledBack };
            let reached = current.map_or(0, |i| i + 1);
            for (i, r) in reports.iter_mut().enumerate() {
                r.status = if i >= reached { FileStatus::Pending } else { status };
            }
            if let (Some(i), MigrationOutcome::AllRolledBack, true) = (current, outcome, in_file) {
                reports[i].status = FileStatus::Failed;
                reports[i].execution_ms = elapsed(file_started);
            }
            match current {
                // Nothing of the file ran: no row records it.
                Some(i) if !matches!(f, Failure::Assertion(_)) => {
                    fail(conn, table, &jobs[i], jobs[i].revision(), f, outcome, 0, file_started)
                }
                _ => failure_error(None, f, outcome, 0),
            }
        });
    }
    for (done, j) in jobs.iter().enumerate() {
        if let Err(e) = check_assertions(conn, j.p, false, done) {
            reports[done].status = FileStatus::Failed;
            for r in &mut reports[done + 1..] {
                r.status = FileStatus::Pending;
            }
            return Err(e);
        }
        let t = Instant::now();
        let mut rev = j.revision();
        // The row as last committed, for a failure after a commit point.
        let mut committed = rev.clone();
        let rep = &mut reports[done];
        let res = if j.mode == TxMode::None {
            (|| -> std::result::Result<(), Failure> {
                hook(conn, "before_sql", opts.before_sql.as_deref())?;
                table.replace(conn, &rev, false)?;
                no_transaction(conn, table, j, &mut rev, t, rep)?;
                hook(conn, "after_sql", opts.after_sql.as_deref())
            })()
        } else {
            (|| -> std::result::Result<(), Failure> {
                step(conn, "BEGIN")?;
                hook(conn, "before_sql", opts.before_sql.as_deref())?;
                table.replace(conn, &rev, true)?;
                in_transaction(conn, table, j, &mut rev, &mut committed, opts, t, rep)?;
                hook(conn, "after_sql", opts.after_sql.as_deref())?;
                step(conn, "COMMIT")
            })()
        };
        reports[done].execution_ms = elapsed(t);
        if let Err(f) = res {
            reports[done].status = FileStatus::Failed;
            for r in &mut reports[done + 1..] {
                r.status = FileStatus::Pending;
            }
            return Err(if j.mode == TxMode::None {
                // Progress is recorded as it is made; record the error on top of it. After a
                // statement that ran and could not be recorded, the row keeps its in-flight mark.
                let mut outcome = MigrationOutcome::Partial;
                let error = match &f {
                    Failure::Statement { index, source } => Some((source.to_string(), j.p.sql.statements[*index].sql.clone())),
                    Failure::Step { step, source } => Some((source.to_string(), step.clone())),
                    Failure::Revisions(e) => Some((e.to_string(), String::new())),
                    Failure::Assertion(_) => None,
                    Failure::Record { .. } => {
                        outcome = MigrationOutcome::InFlight;
                        None
                    }
                };
                if let Some((message, stmt)) = error {
                    rev.fail(&message, &stmt);
                    rev.execution_ms = elapsed(t);
                    if table.update(conn, &rev).is_err() && matches!(f, Failure::Statement { .. }) {
                        outcome = MigrationOutcome::InFlight;
                    }
                }
                failure_error(Some(j), f, outcome, done)
            } else {
                let _ = conn.batch("ROLLBACK");
                let partly = committed.applied as usize > j.start();
                let rolled_back = if partly { MigrationOutcome::PartlyCommitted } else { MigrationOutcome::FileRolledBack };
                let outcome = commit_outcome(&f).unwrap_or(rolled_back);
                fail(conn, table, j, committed, f, outcome, done, t)
            });
        }
    }
    Ok(())
}

fn timing(index: usize, s: &pgmig_sql::Statement, began: Instant) -> StatementTiming {
    StatementTiming { index, line: s.pos.line, hash: history::statement_hash(&s.sql), execution_ms: elapsed(began) }
}

fn elapsed(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A `COMMIT` that lost the connection may or may not have committed.
fn commit_outcome(f: &Failure) -> Option<MigrationOutcome> {
    match f {
        Failure::Step { step, source } if step == "COMMIT" && source.code.is_none() => Some(MigrationOutcome::Unknown),
        _ => None,
    }
}

fn step(conn: &mut Conn, sql: &str) -> std::result::Result<(), Failure> {
    conn.batch(sql).map_err(|source| Failure::Step { step: sql.to_owned(), source })
}

fn hook(conn: &mut Conn, name: &str, sql: Option<&str>) -> std::result::Result<(), Failure> {
    match sql.filter(|s| !s.trim().is_empty()) {
        Some(sql) => conn.batch(sql).map_err(|source| Failure::Step { step: name.to_owned(), source }),
        None => Ok(()),
    }
}

/// Run the file's statements after those already applied, in the transaction in progress. At each
/// commit point the row records the progress, the transaction commits (with the hooks) and a new
/// one begins; `committed` follows the row as last committed. The row is written at the end. Each
/// statement that ran is added to `rep`.
#[allow(clippy::too_many_arguments)]
fn in_transaction(
    conn: &mut Conn,
    table: &Table,
    j: &Job,
    rev: &mut Revision,
    committed: &mut Revision,
    opts: &ApplyOptions,
    t: Instant,
    rep: &mut FileReport,
) -> std::result::Result<(), Failure> {
    let start = j.start();
    for (index, s) in j.p.sql.statements.iter().enumerate().skip(start) {
        if index > start && j.p.commits_before(index) {
            rev.execution_ms = elapsed(t);
            table.update(conn, rev)?;
            hook(conn, "after_sql", opts.after_sql.as_deref())?;
            step(conn, "COMMIT")?;
            committed.clone_from(rev);
            step(conn, "BEGIN")?;
            hook(conn, "before_sql", opts.before_sql.as_deref())?;
        }
        let began = Instant::now();
        conn.batch(&s.sql).map_err(|source| Failure::Statement { index, source })?;
        rep.statement_timings.push(timing(index, s, began));
        rev.progress(&history::statement_hash(&s.sql));
    }
    rev.execution_ms = elapsed(t);
    table.update(conn, rev)?;
    Ok(())
}

/// Run the file's statements after those already applied, each committing on its own: the row
/// marks each statement in flight before it runs and records it as applied after.
fn no_transaction(
    conn: &mut Conn,
    table: &Table,
    j: &Job,
    rev: &mut Revision,
    t: Instant,
    rep: &mut FileReport,
) -> std::result::Result<(), Failure> {
    for (index, s) in j.p.sql.statements.iter().enumerate().skip(j.start()) {
        let mut marked = rev.clone();
        marked.start_statement(index, &s.sql);
        marked.execution_ms = elapsed(t);
        table.update(conn, &marked)?;
        let began = Instant::now();
        conn.batch(&s.sql).map_err(|source| Failure::Statement { index, source })?;
        rep.statement_timings.push(timing(index, s, began));
        rev.progress(&history::statement_hash(&s.sql));
        rev.execution_ms = elapsed(t);
        table.update(conn, rev).map_err(|source| Failure::Record { index, source })?;
    }
    Ok(())
}

/// After a rollback: record the failure in the file's row, in a transaction of its own, and build
/// the error. A failure to record it is not reported over the failure itself.
#[allow(clippy::too_many_arguments)]
fn fail(
    conn: &mut Conn,
    table: &Table,
    j: &Job,
    mut rev: Revision,
    f: Failure,
    outcome: MigrationOutcome,
    done: usize,
    t: Instant,
) -> Error {
    if outcome != MigrationOutcome::Unknown {
        let (message, stmt) = match &f {
            Failure::Statement { index, source } => (source.to_string(), j.p.sql.statements[*index].sql.clone()),
            Failure::Step { step, source } => (source.to_string(), step.clone()),
            Failure::Revisions(e) | Failure::Record { source: e, .. } => (e.to_string(), String::new()),
            Failure::Assertion(e) => (e.to_string(), String::new()),
        };
        // A rolled-back resume keeps what the earlier run, or an earlier commit point, applied.
        rev.fail(&message, &stmt);
        rev.execution_ms = elapsed(t);
        let _ = table.replace(conn, &rev, false);
    }
    failure_error(Some(j), f, outcome, done)
}

impl Error {
    /// A migration failure with what became of the run's files.
    fn with_report(mut self, run: MigrateApplyReport) -> Error {
        if let Error::MigrationFailed { report, .. }
        | Error::MigrationStep { report, .. }
        | Error::MigrationRecord { report, .. }
        | Error::AssertionFailed { report, .. } = &mut self
        {
            **report = run;
        }
        self
    }
}

fn failure_error(j: Option<&Job>, f: Failure, outcome: MigrationOutcome, applied_files: usize) -> Error {
    let record = |statement_index, source| match j {
        Some(j) => Error::MigrationRecord {
            path: j.p.path.clone(),
            statement_index,
            source: Box::new(source),
            outcome,
            applied_files,
            report: Box::default(),
        },
        None => Error::Revisions(source),
    };
    match f {
        Failure::Statement { index, source } => {
            let j = j.expect("a statement belongs to a file");
            let s = &j.p.sql.statements[index];
            Error::MigrationFailed {
                path: j.p.path.clone(),
                pos: source.position.map_or(s.pos, |n| s.locate(n)),
                statement_index: index,
                statement: s.sql.clone(),
                source: Box::new(source),
                outcome,
                applied_files,
                report: Box::default(),
            }
        }
        Failure::Step { step, source } => Error::MigrationStep {
            path: j.map(|j| j.p.path.clone()),
            step,
            source: Box::new(source),
            outcome,
            applied_files,
            report: Box::default(),
        },
        Failure::Revisions(e) => record(None, e),
        Failure::Assertion(e) => *e,
        Failure::Record { index, source } => record(Some(index), source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(sql: &str) -> Vec<usize> {
        commit_points(&pgmig_sql::split(sql).expect("split")).expect("commit points").into_iter().map(|(i, _)| i).collect()
    }

    /// The planner's marker is the directive the engine commits at.
    #[test]
    fn the_planner_marker_is_a_commit_point() {
        let plan = pgmig_plan::Plan {
            statements: vec!["SELECT 1".into(), "SELECT 2".into(), "SELECT 3".into()],
            commit_after: Some(0),
            ..Default::default()
        };
        assert_eq!(points(&plan.to_sql()), [1]);
        assert_eq!(points("SELECT 1;\n\n-- pgmig:commit\n\nSELECT 2;\n-- pgmig:commit\n-- pgmig:commit\nSELECT 3;\n"), [1, 2]);
        assert!(points("-- pgmig:commit\nSELECT 1;\n-- pgmig:commit\n").is_empty());
        // A comment that only reads like one is not.
        assert!(points("SELECT 1;\n-- commit: here\nSELECT 2;\n").is_empty());
    }
}
