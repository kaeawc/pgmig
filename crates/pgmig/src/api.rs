//! The commands as serde types: one request and one response per command, and [`ApiError`] for
//! failures. The CLI, JSON output and language packages call these; each wraps the function of the
//! same name at the crate root, which does the work.
//!
//! Requests may leave out `shadow` (pgmem) and `options` (manage everything, database mode).
//! Responses hold everything a caller prints: the plan, and the changes or model it was made from.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ApplyOptions, ApplyReport, Change, ChecksumError, Error, MigrateApplyReport, MigrationOutcome, Model, Options, Plan, ShadowSpec,
    Source, SumFile, filter::PatternError,
};

/// `schema diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffRequest {
    pub from: Source,
    pub to: Source,
    #[serde(default)]
    pub shadow: ShadowSpec,
    #[serde(default)]
    pub options: Options,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResponse {
    /// The SQL that turns `from` into `to`.
    pub plan: Plan,
    /// The changes the plan makes, for the objects the options manage.
    pub changes: Vec<Change>,
    /// `plan` rendered as a SQL script ([`Plan::to_sql`]).
    pub sql: String,
}

/// `schema inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub source: Source,
    #[serde(default)]
    pub shadow: ShadowSpec,
    #[serde(default)]
    pub options: Options,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectResponse {
    /// SQL that recreates the managed objects in a new database.
    pub plan: Plan,
    /// The managed objects.
    pub model: Model,
    /// `plan` rendered as a SQL script ([`Plan::to_sql`]).
    pub sql: String,
}

/// `schema validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateRequest {
    pub source: Source,
    #[serde(default)]
    pub shadow: ShadowSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateResponse {
    /// What is wrong with the files; empty when they load. Loading stops at the first error, so
    /// there is at most one for now.
    pub diagnostics: Vec<Diagnostic>,
}

impl ValidateResponse {
    pub fn is_valid(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// `schema apply`, first half: what would change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanApplyRequest {
    /// The database to change.
    pub target: String,
    pub desired: Source,
    #[serde(default)]
    pub shadow: ShadowSpec,
    #[serde(default)]
    pub options: Options,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanApplyResponse {
    /// The plan to pass to [`apply`]: schema-qualified, whatever the options' schema mode.
    pub plan: Plan,
    pub changes: Vec<Change>,
    /// `plan` rendered as a SQL script ([`Plan::to_sql`]).
    pub sql: String,
}

/// `schema apply`, second half: run a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    pub target: String,
    pub plan: Plan,
}

pub type ApplyResponse = ApplyReport;

/// `migrate hash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrateHashRequest {
    /// The migration directory.
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateHashResponse {
    /// The sum file's path.
    pub sum_file: PathBuf,
    /// Whether the sum file was written; `false` when it already matched.
    pub written: bool,
    /// Line one, `h1:<base64>`.
    pub dir_hash: String,
    /// One entry per migration, in order.
    pub files: Vec<HashedFile>,
}

/// A line of the sum file after the first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedFile {
    /// The migration's file name.
    pub file: String,
    /// `h1:<base64>`, chaining every earlier file.
    pub hash: String,
}

impl MigrateHashResponse {
    fn new(sum_file: PathBuf, sum: &SumFile, written: bool) -> MigrateHashResponse {
        MigrateHashResponse {
            sum_file,
            written,
            dir_hash: format!("h1:{}", sum.dir_hash),
            files: sum.lines.iter().map(|l| HashedFile { file: l.file.clone(), hash: format!("h1:{}", l.hash) }).collect(),
        }
    }
}

/// `migrate validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrateValidateRequest {
    /// The migration directory.
    pub dir: PathBuf,
    /// Also replay every migration in order into this shadow database. `None` checks only the sum
    /// file.
    #[serde(default)]
    pub shadow: Option<ShadowSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateValidateResponse {
    /// What is wrong with the directory; empty when it is valid. Checking stops at the first
    /// problem, so there is at most one for now.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether every migration was replayed into the shadow database without error.
    pub replayed: bool,
}

impl MigrateValidateResponse {
    pub fn is_valid(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// `migrate diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrateDiffRequest {
    /// The migration directory; created when a file is written to a directory that does not exist.
    pub dir: PathBuf,
    /// The new migration's description, sanitized to `[a-z0-9_]`.
    pub name: String,
    /// The desired state.
    pub to: Source,
    #[serde(default)]
    pub shadow: ShadowSpec,
    #[serde(default)]
    pub options: Options,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateDiffResponse {
    /// The migration written; null when there were no changes and nothing was written.
    pub file: Option<PathBuf>,
    /// Its version; null when nothing was written.
    pub version: Option<String>,
    /// The plan from the replayed directory to the desired state (schema-qualified).
    pub plan: Plan,
    pub changes: Vec<Change>,
    /// `plan` rendered as a SQL script ([`Plan::to_sql`]): the new file's contents.
    pub sql: String,
}

/// `migrate new`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrateNewRequest {
    /// The migration directory; created when it does not exist.
    pub dir: PathBuf,
    /// The new migration's description, sanitized to `[a-z0-9_]`.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateNewResponse {
    /// The empty migration written.
    pub file: PathBuf,
    pub version: String,
}

/// `migrate apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrateApplyRequest {
    /// The database to migrate.
    pub target: String,
    /// The migration directory.
    pub dir: PathBuf,
    #[serde(default)]
    pub options: ApplyOptions,
}

/// Every file of the directory with its status, and what this run applied.
pub type MigrateApplyResponse = MigrateApplyReport;

/// `config validate`. `Debug` shows the `vars` names only: their values may be secrets.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigValidateRequest {
    /// The `pgmig.toml` to check.
    pub path: PathBuf,
    /// Also check that this `[env.<name>]` section exists and resolves; `None` checks the top level.
    #[serde(default)]
    pub env: Option<String>,
    /// `--var` values for `${NAME}` placeholders; names not here come from the environment.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

impl std::fmt::Debug for ConfigValidateRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigValidateRequest")
            .field("path", &self.path)
            .field("env", &self.env)
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValidateResponse {
    /// Everything wrong with the file; empty when it is valid. Messages name keys, never values.
    pub diagnostics: Vec<Diagnostic>,
}

impl ConfigValidateResponse {
    pub fn is_valid(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// A stable identifier for each kind of failure, serialized in snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A file or directory could not be read.
    Io,
    /// `schema validate` was given a database.
    NotSql,
    /// A directory holds no `.sql` files.
    NoSql,
    /// A file could not be split into statements (an unterminated string, say).
    Split,
    /// A statement failed in the shadow database.
    Load,
    /// `-- pgmig:import` without a path.
    ImportArgs,
    /// `-- pgmig:import` after the first statement.
    ImportPlacement,
    /// `-- pgmig:import` of a path that does not exist.
    ImportMissing,
    /// `-- pgmig:import` of a directory without `.sql` files.
    ImportEmpty,
    ImportCycle,
    /// `-- pgmig:import` in a migration file.
    MigrationImport,
    /// Connecting to, or querying, a database failed.
    Conn,
    /// The shadow database could not be started or is not empty.
    Shadow,
    /// Reading the catalog failed.
    Inspect,
    /// The changes cannot be planned (a dependency cycle, an unsupported change).
    Plan,
    /// A statement of an applied plan failed.
    Apply,
    /// An `--include` / `--exclude` pattern is invalid.
    Pattern,
    /// The command line asks for something that cannot be done (applying without approval).
    Usage,
    /// A `.sql` file in a migration directory is not named `<version>_<description>.sql`, or two
    /// files have the same version.
    MigrationName,
    /// The migration directory has no sum file.
    SumMissing,
    /// The sum file cannot be parsed.
    SumMalformed,
    /// The sum file's directory hash does not match its lines: it was edited by hand.
    SumEdited,
    /// A migration file changed after it was hashed.
    FileEdited,
    /// A migration file was added before files already in the sum file.
    FileInserted,
    /// A file in the sum file is missing from the directory.
    FileDeleted,
    /// A migration file was added after the last hashed one and the sum file not updated.
    FileNotHashed,
    /// The files in the directory are in a different order than in the sum file.
    FileReordered,
    /// `pgmig.toml` is invalid: a syntax error, an unknown key, an undefined `${NAME}`, an unknown
    /// `--env`.
    Config,
    /// A `-- pgmig:<name>` directive is misplaced or has bad arguments.
    Directive,
    /// The revisions table cannot be used (a newer format, a table pgmig did not create).
    Revisions,
    /// A row of the revisions table was edited by hand.
    RevisionEdited,
    /// An applied version has no file in the directory.
    HistoryMissing,
    /// An applied file was edited after it was applied.
    HistoryChanged,
    /// A pending file is older than the newest applied one under the linear order.
    OutOfOrder,
    /// Another `migrate apply` held the advisory lock past the timeout.
    LockTimeout,
    /// `--tx-mode all` with a file that runs without a transaction.
    TxModeConflict,
    /// A partly applied file cannot resume: its applied statements changed.
    Resume,
    /// A migration failed: a statement, a hook, `BEGIN` or `COMMIT`.
    Migration,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Io => "io",
            ErrorCode::NotSql => "not_sql",
            ErrorCode::NoSql => "no_sql",
            ErrorCode::Split => "split",
            ErrorCode::Load => "load",
            ErrorCode::ImportArgs => "import_args",
            ErrorCode::ImportPlacement => "import_placement",
            ErrorCode::ImportMissing => "import_missing",
            ErrorCode::ImportEmpty => "import_empty",
            ErrorCode::ImportCycle => "import_cycle",
            ErrorCode::MigrationImport => "migration_import",
            ErrorCode::Conn => "conn",
            ErrorCode::Shadow => "shadow",
            ErrorCode::Inspect => "inspect",
            ErrorCode::Plan => "plan",
            ErrorCode::Apply => "apply",
            ErrorCode::Pattern => "pattern",
            ErrorCode::Usage => "usage",
            ErrorCode::MigrationName => "migration_name",
            ErrorCode::SumMissing => "sum_missing",
            ErrorCode::SumMalformed => "sum_malformed",
            ErrorCode::SumEdited => "sum_edited",
            ErrorCode::FileEdited => "file_edited",
            ErrorCode::FileInserted => "file_inserted",
            ErrorCode::FileDeleted => "file_deleted",
            ErrorCode::FileNotHashed => "file_not_hashed",
            ErrorCode::FileReordered => "file_reordered",
            ErrorCode::Config => "config",
            ErrorCode::Directive => "directive",
            ErrorCode::Revisions => "revisions",
            ErrorCode::RevisionEdited => "revision_edited",
            ErrorCode::HistoryMissing => "history_missing",
            ErrorCode::HistoryChanged => "history_changed",
            ErrorCode::OutOfOrder => "out_of_order",
            ErrorCode::LockTimeout => "lock_timeout",
            ErrorCode::TxModeConflict => "tx_mode_conflict",
            ErrorCode::Resume => "resume",
            ErrorCode::Migration => "migration",
        }
    }
}

/// A failure as callers see it. `message` is the error as the CLI prints it (with the file and
/// position, and the server's SQLSTATE, when there are any); the other fields repeat its parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// The server's SQLSTATE, verbatim.
    pub sqlstate: Option<String>,
    /// The SQL file the error is in.
    pub file: Option<PathBuf>,
    /// 1-based line and column in `file`.
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Set for `apply` failures.
    pub apply: Option<Box<ApplyFailure>>,
    /// Set for `migration` failures; left out otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<Box<MigrationFailure>>,
}

/// Where a `migrate apply` stopped and what became of the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationFailure {
    /// 0-based index of the failing statement in its file; null when a hook, `BEGIN` or `COMMIT`
    /// failed.
    pub statement_index: Option<usize>,
    /// The failing statement, or `before_sql`, `after_sql`, `BEGIN`, `COMMIT`.
    pub statement: String,
    /// Files of this run committed before the failure.
    pub applied_files: usize,
    pub outcome: MigrationOutcome,
}

/// Where an `apply` stopped and what became of the batch it was in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyFailure {
    /// 0-based index into the plan's statements; null when `BEGIN` or `COMMIT` itself failed.
    pub statement_index: Option<usize>,
    /// The failing statement (`BEGIN` or `COMMIT` included).
    pub statement: String,
    /// Batches committed before the failing one; their changes stay applied.
    pub committed_batches: usize,
    pub outcome: ApplyOutcome,
}

/// What became of the batch an `apply` failure interrupted, serialized in snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyOutcome {
    /// The batch's transaction was rolled back.
    RolledBack,
    /// `BEGIN` failed: the batch never started.
    NotStarted,
    /// The connection failed during `COMMIT`: the batch may or may not be applied.
    Unknown,
}

/// An error in the files `schema validate` loads.
pub type Diagnostic = ApiError;

impl ApiError {
    fn new(code: ErrorCode, e: &impl std::fmt::Display) -> ApiError {
        ApiError { code, message: e.to_string(), sqlstate: None, file: None, line: None, col: None, apply: None, migration: None }
    }

    /// A command line that cannot be run as given.
    pub fn usage(message: impl Into<String>) -> ApiError {
        ApiError {
            code: ErrorCode::Usage,
            message: message.into(),
            sqlstate: None,
            file: None,
            line: None,
            col: None,
            apply: None,
            migration: None,
        }
    }

    fn sqlstate(mut self, e: &pgmig_conn::Error) -> ApiError {
        self.sqlstate = e.code.clone();
        self
    }

    fn at(mut self, file: &std::path::Path, pos: Option<pgmig_sql::Pos>) -> ApiError {
        self.file = Some(file.to_owned());
        self.line = pos.map(|p| p.line);
        self.col = pos.map(|p| p.col);
        self
    }
}

impl From<&Error> for ApiError {
    fn from(e: &Error) -> ApiError {
        let err = |code| ApiError::new(code, e);
        match e {
            Error::Io { path, .. } => err(ErrorCode::Io).at(path, None),
            Error::NotSql => err(ErrorCode::NotSql),
            Error::NoSql { path } => err(ErrorCode::NoSql).at(path, None),
            Error::Split { path, source } => err(ErrorCode::Split).at(path, Some(source.pos)),
            Error::Load { path, pos, source } => err(ErrorCode::Load).at(path, Some(*pos)).sqlstate(source),
            Error::ImportArgs { path, pos } => err(ErrorCode::ImportArgs).at(path, Some(*pos)),
            Error::ImportPlacement { path, pos } => err(ErrorCode::ImportPlacement).at(path, Some(*pos)),
            Error::ImportMissing { path, pos, .. } => err(ErrorCode::ImportMissing).at(path, Some(*pos)),
            Error::ImportEmpty { path, pos, .. } => err(ErrorCode::ImportEmpty).at(path, Some(*pos)),
            Error::ImportCycle { .. } => err(ErrorCode::ImportCycle),
            Error::MigrationDescription { .. } => err(ErrorCode::Usage),
            Error::Options(_) => err(ErrorCode::Usage),
            Error::MigrationImport { path, pos } => err(ErrorCode::MigrationImport).at(path, Some(*pos)),
            Error::Migrate(m) => match m {
                pgmig_migrate::Error::Io { path, .. } => err(ErrorCode::Io).at(path, None),
                pgmig_migrate::Error::Name { path, .. } => err(ErrorCode::MigrationName).at(path, None),
                pgmig_migrate::Error::DuplicateVersion { second, .. } => err(ErrorCode::MigrationName).at(second, None),
            },
            Error::Checksum(c) => {
                let code = match c {
                    ChecksumError::MissingSum { .. } => ErrorCode::SumMissing,
                    ChecksumError::Io { .. } => ErrorCode::Io,
                    ChecksumError::Malformed { .. } => ErrorCode::SumMalformed,
                    ChecksumError::SumEdited { .. } => ErrorCode::SumEdited,
                    ChecksumError::Deleted { .. } => ErrorCode::FileDeleted,
                    ChecksumError::Inserted { .. } => ErrorCode::FileInserted,
                    ChecksumError::NotHashed { .. } => ErrorCode::FileNotHashed,
                    ChecksumError::Reordered { .. } => ErrorCode::FileReordered,
                    ChecksumError::Edited { .. } => ErrorCode::FileEdited,
                };
                let mut e = err(code).at(c.path(), None);
                if let ChecksumError::Malformed { line, .. } = c {
                    e.line = Some(*line);
                }
                e
            }
            Error::Conn(c) => err(ErrorCode::Conn).sqlstate(c),
            Error::Shadow(pgmig_shadow::Error::Conn(c)) => err(ErrorCode::Shadow).sqlstate(c),
            Error::Shadow(_) => err(ErrorCode::Shadow),
            Error::Inspect(pgmig_inspect::Error::Query { source, .. }) => err(ErrorCode::Inspect).sqlstate(source),
            Error::Inspect(_) => err(ErrorCode::Inspect),
            Error::Plan(_) => err(ErrorCode::Plan),
            Error::Apply { statement_index, statement, source, committed_batches } => {
                let outcome = match (statement_index, statement.as_str()) {
                    (None, "BEGIN") => ApplyOutcome::NotStarted,
                    (None, "COMMIT") if source.code.is_none() => ApplyOutcome::Unknown,
                    _ => ApplyOutcome::RolledBack,
                };
                let mut e = err(ErrorCode::Apply).sqlstate(source);
                e.apply = Some(Box::new(ApplyFailure {
                    statement_index: *statement_index,
                    statement: statement.clone(),
                    committed_batches: *committed_batches,
                    outcome,
                }));
                e
            }
            Error::Directive { path, pos, .. } => err(ErrorCode::Directive).at(path, Some(*pos)),
            Error::Revisions(r) => match r {
                pgmig_migrate::revisions::Error::Edited { .. } => err(ErrorCode::RevisionEdited),
                pgmig_migrate::revisions::Error::Conn(c) => err(ErrorCode::Revisions).sqlstate(c),
                _ => err(ErrorCode::Revisions),
            },
            Error::History(h) => match h {
                pgmig_migrate::history::HistoryError::Missing { .. } => err(ErrorCode::HistoryMissing),
                pgmig_migrate::history::HistoryError::Changed { file, .. } => err(ErrorCode::HistoryChanged).at(file, None),
                pgmig_migrate::history::HistoryError::OutOfOrder { file, .. } => err(ErrorCode::OutOfOrder).at(file, None),
            },
            Error::LockTimeout { .. } => err(ErrorCode::LockTimeout),
            Error::TxModeConflict { path, .. } => err(ErrorCode::TxModeConflict).at(path, None),
            Error::Resume { path, .. } => err(ErrorCode::Resume).at(path, None),
            Error::MigrationFailed { path, pos, statement_index, statement, source, outcome, applied_files } => {
                let mut e = err(ErrorCode::Migration).at(path, Some(*pos)).sqlstate(source);
                e.migration = Some(Box::new(MigrationFailure {
                    statement_index: Some(*statement_index),
                    statement: statement.clone(),
                    applied_files: *applied_files,
                    outcome: *outcome,
                }));
                e
            }
            Error::MigrationRecord { path, statement_index, source, outcome, applied_files } => {
                let mut e = match source.as_ref() {
                    pgmig_migrate::revisions::Error::Conn(c) => err(ErrorCode::Revisions).sqlstate(c),
                    _ => err(ErrorCode::Revisions),
                }
                .at(path, None);
                e.migration = Some(Box::new(MigrationFailure {
                    statement_index: *statement_index,
                    statement: String::new(),
                    applied_files: *applied_files,
                    outcome: *outcome,
                }));
                e
            }
            Error::MigrationStep { path, step, source, outcome, applied_files } => {
                let mut e = err(ErrorCode::Migration).sqlstate(source);
                if let Some(p) = path {
                    e = e.at(p, None);
                }
                e.migration = Some(Box::new(MigrationFailure {
                    statement_index: None,
                    statement: step.clone(),
                    applied_files: *applied_files,
                    outcome: *outcome,
                }));
                e
            }
        }
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        ApiError::from(&e)
    }
}

impl ApiError {
    /// One problem of a `pgmig.toml`, as a diagnostic.
    pub fn config(path: &std::path::Path, problem: &crate::config::Problem) -> ApiError {
        let mut e = ApiError::new(ErrorCode::Config, &problem.in_file(path));
        e.file = Some(path.to_owned());
        (e.line, e.col) = (problem.line, problem.col);
        e
    }
}

/// An unreadable file is `io`, a bad `--var` is `usage`, the rest `config` (positioned at the first
/// problem; the message has them all).
impl From<&crate::config::ConfigError> for ApiError {
    fn from(e: &crate::config::ConfigError) -> ApiError {
        use crate::config::ConfigError;
        match e {
            ConfigError::Io { path, .. } => ApiError::new(ErrorCode::Io, e).at(path, None),
            ConfigError::Var { .. } | ConfigError::VarSyntax => ApiError::usage(e.to_string()),
            ConfigError::Invalid { path, problems } => {
                let mut err = ApiError::new(ErrorCode::Config, e).at(path, None);
                if let Some(p) = problems.first() {
                    (err.line, err.col) = (p.line, p.col);
                }
                err
            }
        }
    }
}

impl From<crate::config::ConfigError> for ApiError {
    fn from(e: crate::config::ConfigError) -> ApiError {
        ApiError::from(&e)
    }
}

impl From<&PatternError> for ApiError {
    fn from(e: &PatternError) -> ApiError {
        ApiError::new(ErrorCode::Pattern, e)
    }
}

impl From<PatternError> for ApiError {
    fn from(e: PatternError) -> ApiError {
        ApiError::from(&e)
    }
}

/// Whether `e` is about the files being loaded (a diagnostic) rather than about loading them
/// (a shadow that would not start, a request that names a database).
fn is_diagnostic(e: &Error) -> bool {
    match e {
        Error::Io { .. }
        | Error::NoSql { .. }
        | Error::Split { .. }
        | Error::Load { .. }
        | Error::ImportArgs { .. }
        | Error::ImportPlacement { .. }
        | Error::ImportMissing { .. }
        | Error::ImportEmpty { .. }
        | Error::ImportCycle { .. }
        | Error::MigrationImport { .. }
        | Error::Migrate(_)
        | Error::Checksum(_) => true,
        Error::NotSql
        | Error::MigrationDescription { .. }
        | Error::Options(_)
        | Error::Conn(_)
        | Error::Shadow(_)
        | Error::Inspect(_)
        | Error::Plan(_)
        | Error::Apply { .. }
        | Error::Directive { .. }
        | Error::Revisions(_)
        | Error::History(_)
        | Error::LockTimeout { .. }
        | Error::TxModeConflict { .. }
        | Error::Resume { .. }
        | Error::MigrationFailed { .. }
        | Error::MigrationStep { .. }
        | Error::MigrationRecord { .. } => false,
    }
}

/// [`crate::schema_diff`].
pub fn schema_diff(req: &DiffRequest) -> Result<DiffResponse, ApiError> {
    let (changes, plan) = crate::diff_sources(&req.from, &req.to, &req.shadow, &req.options, &req.options.plan_options())?;
    Ok(DiffResponse { sql: plan.to_sql(), plan, changes })
}

/// [`crate::schema_inspect`].
pub fn schema_inspect(req: &InspectRequest) -> Result<InspectResponse, ApiError> {
    let (model, plan) = crate::inspect_source(&req.source, &req.shadow, &req.options)?;
    Ok(InspectResponse { sql: plan.to_sql(), plan, model })
}

/// [`crate::schema_validate`]: errors in the files are diagnostics; an `Err` means they could not
/// be checked.
pub fn schema_validate(req: &ValidateRequest) -> Result<ValidateResponse, ApiError> {
    match crate::schema_validate(&req.source, &req.shadow) {
        Ok(()) => Ok(ValidateResponse { diagnostics: vec![] }),
        Err(e) if is_diagnostic(&e) => Ok(ValidateResponse { diagnostics: vec![ApiError::from(&e)] }),
        Err(e) => Err(e.into()),
    }
}

/// [`crate::migrate_hash`].
pub fn migrate_hash(req: &MigrateHashRequest) -> Result<MigrateHashResponse, ApiError> {
    let (sum, written) = crate::migrate_hash(&req.dir)?;
    Ok(MigrateHashResponse::new(req.dir.join(crate::SUM_FILE), &sum, written))
}

/// [`crate::migrate_validate`]: problems with the directory, its sum file or a migration are
/// diagnostics; an `Err` means they could not be checked (the shadow database would not start).
pub fn migrate_validate(req: &MigrateValidateRequest) -> Result<MigrateValidateResponse, ApiError> {
    match crate::migrate_validate(&req.dir, req.shadow.as_ref()) {
        Ok(()) => Ok(MigrateValidateResponse { diagnostics: vec![], replayed: req.shadow.is_some() }),
        Err(e) if is_diagnostic(&e) => Ok(MigrateValidateResponse { diagnostics: vec![ApiError::from(&e)], replayed: false }),
        Err(e) => Err(e.into()),
    }
}

/// [`crate::migrate_diff`].
pub fn migrate_diff(req: &MigrateDiffRequest) -> Result<MigrateDiffResponse, ApiError> {
    let res = crate::migrate_diff(&req.dir, &req.name, &req.to, &req.shadow, &req.options)?;
    let (file, version) = res.written.map(|w| (w.path, w.version)).unzip();
    Ok(MigrateDiffResponse { file, version, sql: res.plan.to_sql(), plan: res.plan, changes: res.changes })
}

/// [`crate::migrate_new`].
pub fn migrate_new(req: &MigrateNewRequest) -> Result<MigrateNewResponse, ApiError> {
    let new = crate::migrate_new(&req.dir, &req.name)?;
    Ok(MigrateNewResponse { file: new.path, version: new.version })
}

/// [`crate::migrate_apply`].
pub fn migrate_apply(req: &MigrateApplyRequest) -> Result<MigrateApplyResponse, ApiError> {
    Ok(crate::migrate_apply(&req.target, &req.dir, &req.options)?)
}

/// [`crate::config::validate`]: problems with the file are diagnostics; an `Err` means it could
/// not be read.
pub fn config_validate(req: &ConfigValidateRequest) -> Result<ConfigValidateResponse, ApiError> {
    let vars = crate::config::Vars::new(req.vars.clone());
    let problems = crate::config::validate(&req.path, req.env.as_deref(), &vars)?;
    Ok(ConfigValidateResponse { diagnostics: problems.iter().map(|p| ApiError::config(&req.path, p)).collect() })
}

/// [`crate::schema_plan_apply`].
pub fn schema_plan_apply(req: &PlanApplyRequest) -> Result<PlanApplyResponse, ApiError> {
    let (changes, plan) = crate::plan_apply_changes(&req.target, &req.desired, &req.shadow, &req.options)?;
    Ok(PlanApplyResponse { sql: plan.to_sql(), plan, changes })
}

/// [`crate::apply`].
pub fn apply(req: &ApplyRequest) -> Result<ApplyResponse, ApiError> {
    Ok(crate::apply(&req.target, &req.plan)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Filter;
    use pgmig_model::QualName;

    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> String {
        let json = serde_json::to_string(v).expect("serialize");
        let back: T = serde_json::from_str(&json).expect(&json);
        assert_eq!(&back, v, "{json}");
        json
    }

    fn options() -> Options {
        Options { filter: Filter::parse(&["app"], &["app.tmp_*"]).expect("patterns"), schema: Some("app".into()), ..Options::default() }
    }

    fn plan() -> Plan {
        Plan { statements: vec!["CREATE SCHEMA app".into()], commit_after: None }
    }

    #[test]
    fn requests_round_trip() {
        let sql = Source::Sql("schema/".into());
        let db = Source::Database("postgres://localhost/db".into());
        let pg = ShadowSpec::Postgres("host=localhost dbname=shadow".into());
        round_trip(&DiffRequest { from: db.clone(), to: sql.clone(), shadow: pg.clone(), options: options() });
        round_trip(&InspectRequest { source: db, shadow: ShadowSpec::Pgmem, options: Options::default() });
        round_trip(&ValidateRequest { source: sql.clone(), shadow: pg.clone() });
        round_trip(&PlanApplyRequest { target: "host=localhost".into(), desired: sql, shadow: pg, options: options() });
        round_trip(&ApplyRequest { target: "host=localhost".into(), plan: plan() });
        round_trip(&MigrateHashRequest { dir: "migrations".into() });
        round_trip(&MigrateValidateRequest { dir: "migrations".into(), shadow: None });
        round_trip(&MigrateValidateRequest { dir: "migrations".into(), shadow: Some(pg_spec()) });
        let migrations = Source::Migrations("migrations".into());
        let json = round_trip(&MigrateDiffRequest {
            dir: "migrations".into(),
            name: "add_users".into(),
            to: migrations.clone(),
            shadow: pg_spec(),
            options: options(),
        });
        assert!(json.contains(r#""to":{"kind":"migrations","data":"migrations"}"#), "{json}");
        round_trip(&MigrateNewRequest { dir: "migrations".into(), name: "x".into() });
        let req: MigrateDiffRequest =
            serde_json::from_str(r#"{"dir":"m","name":"x","to":{"kind":"sql","data":"s.sql"}}"#).expect("request");
        assert_eq!((req.shadow, req.options), (ShadowSpec::Pgmem, Options::default()));
        assert!(serde_json::from_str::<MigrateNewRequest>(r#"{"dir":"m","name":"x","x":1}"#).is_err());
    }

    fn pg_spec() -> ShadowSpec {
        ShadowSpec::Postgres("host=localhost dbname=shadow".into())
    }

    #[test]
    fn migrate_requests_default_and_refuse_unknown_fields() {
        let req: MigrateValidateRequest = serde_json::from_str(r#"{"dir":"m"}"#).expect("request");
        assert_eq!(req, MigrateValidateRequest { dir: "m".into(), shadow: None });
        let req: MigrateValidateRequest = serde_json::from_str(r#"{"dir":"m","shadow":{"kind":"pgmem"}}"#).expect("request");
        assert_eq!(req.shadow, Some(ShadowSpec::Pgmem));
        assert!(serde_json::from_str::<MigrateValidateRequest>(r#"{"dir":"m","shadows":{"kind":"pgmem"}}"#).is_err());
        assert!(serde_json::from_str::<MigrateHashRequest>(r#"{"dir":"m","x":1}"#).is_err());
    }

    #[test]
    fn checksum_errors_name_their_file() {
        let e = ApiError::from(&Error::Checksum(ChecksumError::Edited { file: "m/2_b.sql".into() }));
        assert_eq!((e.code, e.file.as_deref()), (ErrorCode::FileEdited, Some(std::path::Path::new("m/2_b.sql"))));
        let malformed = ChecksumError::Malformed { path: "m/pgmig.sum".into(), line: 3, reason: "expected <file> h1:<hash>" };
        let e = ApiError::from(&Error::Checksum(malformed));
        assert_eq!((e.code, e.line), (ErrorCode::SumMalformed, Some(3)));
        assert_eq!(e.message, "m/pgmig.sum:3: expected <file> h1:<hash>");
        let name = pgmig_migrate::Error::Name { path: "m/x.sql".into(), reason: pgmig_migrate::NameError::NoVersion };
        let e = ApiError::from(&Error::Migrate(name));
        assert_eq!((e.code, e.file.as_deref()), (ErrorCode::MigrationName, Some(std::path::Path::new("m/x.sql"))));
        assert!(is_diagnostic(&Error::Checksum(ChecksumError::SumEdited { path: "m/pgmig.sum".into() })));
    }

    #[test]
    fn responses_round_trip() {
        let changes =
            vec![Change::CreateSchema("app".into()), Change::SetNotNull { table: QualName::new("app", "t"), column: "id".into() }];
        round_trip(&DiffResponse { plan: plan(), sql: plan().to_sql(), changes: changes.clone() });
        round_trip(&InspectResponse { plan: plan(), sql: plan().to_sql(), model: Model::empty_database() });
        round_trip(&PlanApplyResponse { plan: plan(), sql: plan().to_sql(), changes });
        round_trip(&ApplyReport { batches: 2, statements: 5 });
        round_trip(&MigrateDiffResponse {
            file: Some("m/1_a.sql".into()),
            version: Some("1".into()),
            plan: plan(),
            sql: plan().to_sql(),
            changes: vec![],
        });
        round_trip(&MigrateNewResponse { file: "m/1_a.sql".into(), version: "1".into() });
        let load = ApiError {
            code: ErrorCode::Load,
            message: "b.sql:3:1: type \"nosuchtype\" does not exist (SQLSTATE 42704)".into(),
            sqlstate: Some("42704".into()),
            file: Some("b.sql".into()),
            line: Some(3),
            col: Some(1),
            apply: None,
            migration: None,
        };
        round_trip(&ValidateResponse { diagnostics: vec![load.clone()] });
        round_trip(&MigrateValidateResponse { diagnostics: vec![load.clone()], replayed: false });
        let sum = SumFile::from_lines(vec![crate::SumLine { file: "1_a.sql".into(), hash: "x".into() }]);
        let res = round_trip(&MigrateHashResponse::new("m/pgmig.sum".into(), &sum, true));
        assert!(res.contains(r#""files":[{"file":"1_a.sql","hash":"h1:x"}]"#), "{res}");
        round_trip(&load);
    }

    #[test]
    fn requests_default_their_shadow_and_options() {
        let req: DiffRequest =
            serde_json::from_str(r#"{"from":{"kind":"database","data":"host=a"},"to":{"kind":"sql","data":"s.sql"}}"#).expect("request");
        assert_eq!(req.shadow, ShadowSpec::Pgmem);
        assert_eq!(req.options, Options::default());
        let req: InspectRequest =
            serde_json::from_str(r#"{"source":{"kind":"sql","data":"s.sql"},"options":{"schema":"app"}}"#).expect("request");
        assert_eq!(req.options, Options { filter: Filter::default(), schema: Some("app".into()), ..Options::default() });
    }

    /// Request JSON is part of pgmig's interface: a change here must be deliberate.
    #[test]
    fn request_json_is_pinned() {
        let req = DiffRequest {
            from: Source::Database("host=a".into()),
            to: Source::Sql("s.sql".into()),
            shadow: ShadowSpec::Pgmem,
            options: options(),
        };
        assert_eq!(
            serde_json::to_string(&req).expect("serialize"),
            r#"{"from":{"kind":"database","data":"host=a"},"to":{"kind":"sql","data":"s.sql"},"shadow":{"kind":"pgmem"},"options":{"filter":{"include":["app"],"exclude":["app.tmp_*"]},"schema":"app"}}"#
        );
        let err = ApiError::from(&Error::NotSql);
        assert_eq!(
            serde_json::to_string(&err).expect("serialize"),
            r#"{"code":"not_sql","message":"validate takes SQL files or a directory, not a database","sqlstate":null,"file":null,"line":null,"col":null,"apply":null}"#
        );
    }

    #[test]
    fn error_codes_serialize_as_their_names() {
        use ErrorCode::*;
        for code in [
            Io,
            NotSql,
            NoSql,
            Split,
            Load,
            ImportArgs,
            ImportPlacement,
            ImportMissing,
            ImportEmpty,
            ImportCycle,
            MigrationImport,
            Conn,
            Shadow,
            Inspect,
            Plan,
            Apply,
            Pattern,
            Usage,
            MigrationName,
            SumMissing,
            SumMalformed,
            SumEdited,
            FileEdited,
            FileInserted,
            FileDeleted,
            FileNotHashed,
            FileReordered,
            Config,
            Directive,
            Revisions,
            RevisionEdited,
            HistoryMissing,
            HistoryChanged,
            OutOfOrder,
            LockTimeout,
            TxModeConflict,
            Resume,
            Migration,
        ] {
            assert_eq!(serde_json::to_string(&code).expect("serialize"), format!("\"{}\"", code.as_str()));
        }
    }

    #[test]
    fn errors_carry_their_position_and_sqlstate() {
        let server = || pgmig_conn::Error { code: Some("42704".into()), message: "type \"x\" does not exist".into(), position: Some(9) };
        let load = Error::Load { path: "b.sql".into(), pos: pgmig_sql::Pos { line: 4, col: 8 }, source: server() };
        let e = ApiError::from(&load);
        assert_eq!((e.code, e.sqlstate.as_deref()), (ErrorCode::Load, Some("42704")));
        assert_eq!((e.file.as_deref(), e.line, e.col), (Some(std::path::Path::new("b.sql")), Some(4), Some(8)));
        assert_eq!(e.message, load.to_string());
        assert!(is_diagnostic(&load));
        let apply = Error::Apply { statement_index: Some(0), statement: "SELECT 1".into(), source: server(), committed_batches: 0 };
        let e = ApiError::from(&apply);
        assert_eq!((e.code, e.sqlstate.as_deref(), e.file), (ErrorCode::Apply, Some("42704"), None));
        assert!(!is_diagnostic(&apply));
        let pattern = Filter::parse(&["view:x.v"], &[] as &[&str]).expect_err("bad pattern");
        assert_eq!(ApiError::from(&pattern).code, ErrorCode::Pattern);
    }

    #[test]
    fn apply_errors_are_structured() {
        let apply = |statement_index, statement: &str, code: Option<&str>| {
            let source = pgmig_conn::Error { code: code.map(Into::into), message: "boom".into(), position: None };
            let e = Error::Apply { statement_index, statement: statement.into(), source, committed_batches: 2 };
            serde_json::to_string(&ApiError::from(&e)).expect("serialize")
        };
        let tail = |index: &str, statement: &str, outcome: &str| {
            format!(r#""apply":{{"statement_index":{index},"statement":"{statement}","committed_batches":2,"outcome":"{outcome}"}}}}"#)
        };
        let json = apply(Some(3), "SELECT 1", Some("42P01"));
        assert!(json.contains(r#""code":"apply","#) && json.contains(r#""sqlstate":"42P01""#), "{json}");
        assert!(json.ends_with(&tail("3", "SELECT 1", "rolled_back")), "{json}");
        assert!(apply(None, "COMMIT", Some("40001")).ends_with(&tail("null", "COMMIT", "rolled_back")));
        assert!(apply(None, "COMMIT", None).ends_with(&tail("null", "COMMIT", "unknown")));
        assert!(apply(None, "BEGIN", None).ends_with(&tail("null", "BEGIN", "not_started")));
    }

    #[test]
    fn migrate_apply_requests_default_their_options() {
        let req: MigrateApplyRequest = serde_json::from_str(r#"{"target":"host=a","dir":"m"}"#).expect("request");
        assert_eq!(req.options, ApplyOptions::default());
        let req: MigrateApplyRequest =
            serde_json::from_str(r#"{"target":"host=a","dir":"m","options":{"tx_mode":"none","exec_order":"linear-skip","lock":false}}"#)
                .expect("request");
        assert_eq!(
            (req.options.tx_mode, req.options.exec_order, req.options.lock),
            (crate::TxMode::None, crate::ExecOrder::LinearSkip, false)
        );
        round_trip(&req);
        assert!(serde_json::from_str::<MigrateApplyRequest>(r#"{"target":"h","dir":"m","options":{"txmode":"none"}}"#).is_err());
        assert_eq!(ApplyOptions::default().lock_name(), "pgmig:pgmig");
    }

    #[test]
    fn migration_errors_are_structured() {
        let source = pgmig_conn::Error { code: Some("42P01".into()), message: "boom".into(), position: None };
        let e = Error::MigrationFailed {
            path: "m/2_b.sql".into(),
            pos: pgmig_sql::Pos { line: 3, col: 1 },
            statement_index: 1,
            statement: "INSERT INTO nope VALUES (1)".into(),
            source: Box::new(source),
            outcome: MigrationOutcome::FileRolledBack,
            applied_files: 1,
        };
        let json = serde_json::to_value(ApiError::from(&e)).expect("serialize");
        assert_eq!((json["code"].as_str(), json["sqlstate"].as_str(), json["line"].as_u64()), (Some("migration"), Some("42P01"), Some(3)));
        assert_eq!(json["migration"]["outcome"], "file_rolled_back");
        assert_eq!(json["migration"]["statement_index"], 1);
        assert_eq!(
            e.to_string(),
            "m/2_b.sql:3:1: boom (SQLSTATE 42P01)\nthe file's transaction was rolled back; 1 earlier file of this run stays applied"
        );
        // Other errors leave the field out.
        assert!(serde_json::to_value(ApiError::from(&Error::NotSql)).expect("serialize").get("migration").is_none());
    }

    #[test]
    fn responses_carry_the_rendered_script() {
        let json = serde_json::to_value(DiffResponse { plan: plan(), sql: plan().to_sql(), changes: vec![] }).expect("serialize");
        assert_eq!(json["sql"], "CREATE SCHEMA app;\n");
    }

    /// A typo in a request must fail, not fall back to a default that manages everything.
    #[test]
    fn requests_refuse_unknown_fields() {
        let src = r#"{"kind":"sql","data":"s.sql"}"#;
        let cases = [
            format!(r#"{{"from":{src},"to":{src},"option":{{"schema":"app"}}}}"#),
            format!(r#"{{"from":{src},"to":{src},"options":{{"scheme":"app"}}}}"#),
            format!(r#"{{"from":{src},"to":{src},"options":{{"filter":{{"includes":["app"]}}}}}}"#),
            format!(r#"{{"from":{src},"to":{src},"shadow":{{"kind":"pgmem","extra":1}}}}"#),
            format!(r#"{{"from":{{"kind":"sql","data":"s.sql","extra":1}},"to":{src}}}"#),
        ];
        for json in &cases {
            let err = serde_json::from_str::<DiffRequest>(json).expect_err(json);
            let msg = err.to_string();
            assert!(msg.contains("unknown field") || msg.contains("expected \"kind\" or \"data\""), "{json}: {msg}");
        }
        let one = |json: &str| serde_json::from_str::<InspectRequest>(json).expect_err(json);
        one(&format!(r#"{{"source":{src},"shadows":"pgmem"}}"#));
        let json = format!(r#"{{"source":{src},"shadows":"pgmem"}}"#);
        assert!(serde_json::from_str::<ValidateRequest>(&json).is_err());
        let json = format!(r#"{{"target":"h=a","desired":{src},"option":{{}}}}"#);
        assert!(serde_json::from_str::<PlanApplyRequest>(&json).is_err());
        assert!(serde_json::from_str::<ApplyRequest>(r#"{"target":"h=a","plan":{"statements":[],"commit_after":null},"x":1}"#).is_err());
        assert!(serde_json::from_str::<ApplyRequest>(r#"{"target":"h=a","plan":{"statements":[],"commit_after":null,"x":1}}"#).is_err());
    }
}
