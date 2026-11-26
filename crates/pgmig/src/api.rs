//! The commands as serde types: one request and one response per command, and [`ApiError`] for
//! failures. The CLI, JSON output and language packages call these; each wraps the function of the
//! same name at the crate root, which does the work.
//!
//! Requests may leave out `shadow` (pgmem) and `options` (manage everything, database mode).
//! Responses hold everything a caller prints: the plan, and the changes or model it was made from.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{ApplyReport, Change, Error, Model, Options, Plan, ShadowSpec, Source, filter::PatternError};

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
            ErrorCode::Conn => "conn",
            ErrorCode::Shadow => "shadow",
            ErrorCode::Inspect => "inspect",
            ErrorCode::Plan => "plan",
            ErrorCode::Apply => "apply",
            ErrorCode::Pattern => "pattern",
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
        ApiError { code, message: e.to_string(), sqlstate: None, file: None, line: None, col: None, apply: None }
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
        }
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
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
        | Error::ImportCycle { .. } => true,
        Error::NotSql | Error::Conn(_) | Error::Shadow(_) | Error::Inspect(_) | Error::Plan(_) | Error::Apply { .. } => false,
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
        Options { filter: Filter::parse(&["app"], &["app.tmp_*"]).expect("patterns"), schema: Some("app".into()) }
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
    }

    #[test]
    fn responses_round_trip() {
        let changes =
            vec![Change::CreateSchema("app".into()), Change::SetNotNull { table: QualName::new("app", "t"), column: "id".into() }];
        round_trip(&DiffResponse { plan: plan(), sql: plan().to_sql(), changes: changes.clone() });
        round_trip(&InspectResponse { plan: plan(), sql: plan().to_sql(), model: Model::empty_database() });
        round_trip(&PlanApplyResponse { plan: plan(), sql: plan().to_sql(), changes });
        round_trip(&ApplyReport { batches: 2, statements: 5 });
        let load = ApiError {
            code: ErrorCode::Load,
            message: "b.sql:3:1: type \"nosuchtype\" does not exist (SQLSTATE 42704)".into(),
            sqlstate: Some("42704".into()),
            file: Some("b.sql".into()),
            line: Some(3),
            col: Some(1),
            apply: None,
        };
        round_trip(&ValidateResponse { diagnostics: vec![load.clone()] });
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
        assert_eq!(req.options, Options { filter: Filter::default(), schema: Some("app".into()) });
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
            Conn,
            Shadow,
            Inspect,
            Plan,
            Apply,
            Pattern,
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
