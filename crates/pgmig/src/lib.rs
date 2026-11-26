//! pgmig's library surface. Every CLI command is one function here.

use std::path::{Path, PathBuf};

mod files;

pub use pgmig_conn::Conn;
pub use pgmig_diff::{Change, diff};
pub use pgmig_inspect::inspect;
pub use pgmig_model::Model;
pub use pgmig_plan::{Plan, plan};
pub use pgmig_shadow::{PUBLIC_COMMENT as PUBLIC_SCHEMA_COMMENT_SQL, Shadow};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("validate takes SQL files or a directory, not a database")]
    NotSql,
    #[error("{path}: no .sql files")]
    NoSql { path: PathBuf },
    #[error("{path}:{source}")]
    Split { path: PathBuf, source: pgmig_sql::Error },
    /// A statement failed in the shadow database; `pos` is where the server located the error.
    #[error("{path}:{pos}: {source}")]
    Load { path: PathBuf, pos: pgmig_sql::Pos, source: pgmig_conn::Error },
    #[error("{path}:{pos}: pgmig:import needs a path")]
    ImportArgs { path: PathBuf, pos: pgmig_sql::Pos },
    #[error("{path}:{pos}: pgmig:import must come before the first statement")]
    ImportPlacement { path: PathBuf, pos: pgmig_sql::Pos },
    #[error("{path}:{pos}: cannot import {target}: no such file or directory")]
    ImportMissing { path: PathBuf, pos: pgmig_sql::Pos, target: String },
    #[error("{path}:{pos}: cannot import {target}: no .sql files")]
    ImportEmpty { path: PathBuf, pos: pgmig_sql::Pos, target: String },
    #[error("import cycle: {cycle}")]
    ImportCycle { cycle: String },
    #[error(transparent)]
    Conn(#[from] pgmig_conn::Error),
    #[error(transparent)]
    Shadow(#[from] pgmig_shadow::Error),
    #[error(transparent)]
    Inspect(#[from] pgmig_inspect::Error),
    #[error(transparent)]
    Plan(#[from] pgmig_plan::Error),
    /// A statement of an applied plan failed; its transaction was rolled back.
    #[error("{}", apply_message(.statement_index, .statement, .source, .committed_batches))]
    Apply {
        /// 0-based index into [`Plan::statements`]; `None` when the `COMMIT` itself failed.
        statement_index: Option<usize>,
        /// The failing statement (or `COMMIT`).
        statement: String,
        /// The server's error, verbatim with its SQLSTATE.
        source: pgmig_conn::Error,
        /// Batches committed before the failing one: their changes stay applied.
        committed_batches: usize,
    },
}

fn apply_message(index: &Option<usize>, statement: &str, source: &pgmig_conn::Error, committed: &usize) -> String {
    let what = match index {
        Some(i) => format!("statement {} failed", i + 1),
        None => format!("{statement} failed"),
    };
    let batch = match (index, statement) {
        // A COMMIT that lost the connection may or may not have committed.
        (None, "COMMIT") if source.code.is_none() => {
            "the connection failed during COMMIT, so this batch may or may not be applied; plan again to see what is left"
        }
        (None, "BEGIN") => "this batch did not start",
        _ => "its transaction was rolled back",
    };
    let committed = match committed {
        0 => "nothing was committed".to_owned(),
        1 => "1 earlier batch was committed and stays applied".to_owned(),
        n => format!("{n} earlier batches were committed and stay applied"),
    };
    format!("{what}: {statement}\n{source}\n{batch}; {committed}")
}

pub type Result<T> = std::result::Result<T, Error>;

/// A schema state: SQL files describing it, or a live database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A `.sql` file, or a directory of them loaded in lexical order, after the files they
    /// `-- pgmig:import`.
    Sql(PathBuf),
    /// A connection string (URL or key/value).
    Database(String),
}

impl Source {
    /// URLs and key/value connection strings are databases; anything else is a path.
    pub fn parse(s: &str) -> Source {
        if s.starts_with("postgres://") || s.starts_with("postgresql://") || s.contains('=') {
            Source::Database(s.to_owned())
        } else {
            Source::Sql(PathBuf::from(s))
        }
    }
}

/// Where desired-state SQL is executed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ShadowSpec {
    #[default]
    Pgmem,
    /// An empty real Postgres database.
    Postgres(String),
}

impl ShadowSpec {
    pub fn parse(s: &str) -> ShadowSpec {
        if s == "pgmem" { ShadowSpec::Pgmem } else { ShadowSpec::Postgres(s.to_owned()) }
    }

    pub fn open(&self) -> Result<Shadow> {
        Ok(match self {
            ShadowSpec::Pgmem => Shadow::pgmem()?,
            ShadowSpec::Postgres(dsn) => Shadow::postgres(dsn)?,
        })
    }
}

/// The model of a source: SQL is executed in a fresh shadow database, then inspected.
pub fn load_model(source: &Source, shadow: &ShadowSpec) -> Result<Model> {
    match source {
        Source::Database(dsn) => Ok(inspect(&mut Conn::connect(dsn)?)?),
        Source::Sql(path) => {
            let mut sh = load_sql(path, shadow)?;
            Ok(inspect(sh.conn())?)
        }
    }
}

/// Execute the SQL files at `path` in a fresh shadow database, one statement at a time so an
/// error points at its statement.
fn load_sql(path: &Path, shadow: &ShadowSpec) -> Result<Shadow> {
    let mut sh = shadow.open()?;
    for loaded in files::load_files(path)? {
        let file = loaded.path;
        for stmt in &loaded.file.statements {
            sh.conn().batch(&stmt.sql).map_err(|source| Error::Load {
                path: file.clone(),
                pos: source.position.map_or(stmt.pos, |n| stmt.locate(n)),
                source,
            })?;
        }
    }
    Ok(sh)
}

/// `schema validate`: load SQL files into the shadow database and report the first error.
/// A database source has nothing to load and is rejected.
pub fn schema_validate(source: &Source, shadow: &ShadowSpec) -> Result<()> {
    match source {
        Source::Database(_) => Err(Error::NotSql),
        Source::Sql(path) => load_sql(path, shadow).map(drop),
    }
}

/// `schema diff`: the plan that turns `from` into `to`.
pub fn schema_diff(from: &Source, to: &Source, shadow: &ShadowSpec) -> Result<Plan> {
    let a = load_model(from, shadow)?;
    let b = load_model(to, shadow)?;
    Ok(plan(&diff(&a, &b))?)
}

/// `schema inspect`: SQL that recreates the source's schema in a new database.
pub fn schema_inspect(source: &Source, shadow: &ShadowSpec) -> Result<Plan> {
    let m = load_model(source, shadow)?;
    Ok(plan(&diff(&Model::empty_database(), &m))?)
}

/// `schema apply`, first half: the plan that turns the database at `target` into `desired`.
pub fn schema_plan_apply(target: &str, desired: &Source, shadow: &ShadowSpec) -> Result<Plan> {
    schema_diff(&Source::Database(target.to_owned()), desired, shadow)
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Transactions committed (one per [`Plan::batches`] entry).
    pub batches: usize,
    pub statements: usize,
}

/// `schema apply`, second half: run `plan` against the database at `target`.
///
/// Each batch of the plan (see [`Plan::batches`]) runs in its own transaction, so a plan with one
/// batch is applied entirely or not at all. On failure the failing batch is rolled back and
/// [`Error::Apply`] names the statement; earlier batches stay committed.
///
/// Statements Postgres refuses inside a transaction block (`CREATE INDEX CONCURRENTLY`, say) are
/// not planned yet. When they are, they need batches of their own that run without `BEGIN`; this
/// is where to add that, driven by a marker on the plan rather than by matching SQL text.
pub fn apply(target: &str, plan: &Plan) -> Result<ApplyReport> {
    let mut conn = Conn::connect(target)?;
    let mut report = ApplyReport::default();
    for range in batch_ranges(plan) {
        let fail = |conn: &mut Conn, statement_index, statement: &str, source| {
            // The server has aborted the transaction (or ended it, when COMMIT failed); a ROLLBACK
            // error would only hide the one that matters.
            let _ = conn.batch("ROLLBACK");
            Error::Apply { statement_index, statement: statement.to_owned(), source, committed_batches: report.batches }
        };
        if let Err(e) = conn.batch("BEGIN") {
            return Err(Error::Apply {
                statement_index: None,
                statement: "BEGIN".to_owned(),
                source: e,
                committed_batches: report.batches,
            });
        }
        for i in range.clone() {
            let statement = &plan.statements[i];
            if let Err(e) = conn.batch(statement) {
                return Err(fail(&mut conn, Some(i), statement, e));
            }
        }
        if let Err(e) = conn.batch("COMMIT") {
            return Err(fail(&mut conn, None, "COMMIT", e));
        }
        report.batches += 1;
        report.statements += range.len();
    }
    Ok(report)
}

/// The statement indices of each [`Plan::batches`] entry, in order. Empty for an empty plan.
fn batch_ranges(plan: &Plan) -> Vec<std::ops::Range<usize>> {
    let n = plan.statements.len();
    match plan.commit_after {
        _ if n == 0 => vec![],
        Some(i) if i + 1 < n => vec![0..i + 1, i + 1..n],
        _ => std::iter::once(0..n).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_messages_say_what_is_applied() {
        let lost = pgmig_conn::Error { code: None, message: "connection closed".into(), position: None };
        let server = pgmig_conn::Error { code: Some("23503".into()), message: "deferred fk".into(), position: None };
        assert!(apply_message(&None, "COMMIT", &lost, &0).contains("may or may not be applied"));
        assert!(apply_message(&None, "COMMIT", &server, &0).contains("rolled back; nothing was committed"));
        assert!(apply_message(&None, "BEGIN", &lost, &1).contains("did not start; 1 earlier batch was committed"));
        assert!(apply_message(&Some(2), "SELECT 1", &server, &0).starts_with("statement 3 failed: SELECT 1"));
    }

    #[test]
    fn source_parse() {
        assert_eq!(Source::parse("schema.sql"), Source::Sql("schema.sql".into()));
        assert_eq!(Source::parse("postgres://localhost/db"), Source::Database("postgres://localhost/db".into()));
        assert_eq!(Source::parse("host=localhost user=me"), Source::Database("host=localhost user=me".into()));
    }

    #[test]
    fn batch_ranges_match_batches() {
        let statements = |n: usize| (0..n).map(|i| format!("SELECT {i}")).collect::<Vec<_>>();
        for (n, commit_after) in [(0, None), (3, None), (3, Some(0)), (3, Some(2)), (1, Some(0))] {
            let plan = Plan { statements: statements(n), commit_after };
            let ranges = batch_ranges(&plan);
            let scripts: Vec<String> =
                ranges.iter().map(|r| plan.statements[r.clone()].iter().map(|s| format!("{s};\n")).collect()).collect();
            if n == 0 {
                assert!(ranges.is_empty());
            } else {
                assert_eq!(scripts, plan.batches(), "n={n} commit_after={commit_after:?}");
            }
        }
    }
}
