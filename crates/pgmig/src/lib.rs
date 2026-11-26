//! pgmig's library surface. Every CLI command is one function here, and one in [`api`] taking and
//! returning serde types (what the CLI, JSON output and language packages call).
//!
//! [`Source`] and [`ShadowSpec`] serialize adjacently tagged, like the changes of a plan:
//! `{"kind": "sql", "data": "schema/"}`, `{"kind": "database", "data": "postgres://..."}`,
//! `{"kind": "pgmem"}`, `{"kind": "postgres", "data": "host=..."}`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod api;
mod files;
pub mod filter;

pub use filter::{Filter, Pattern, PatternError, filter_model, scope_model};
pub use pgmig_conn::Conn;
pub use pgmig_diff::{Change, diff};
pub use pgmig_inspect::inspect;
pub use pgmig_model::Model;
pub use pgmig_plan::{Plan, PlanOptions, plan, plan_with};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case", deny_unknown_fields)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case", deny_unknown_fields)]
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

/// What pgmig manages, and how it prints names. Serialized as `{"filter": {...}, "schema": ...}`;
/// both may be left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    /// Which objects are managed (see [`filter`]).
    pub filter: Filter,
    /// Schema mode: manage only this schema's objects, not the schema itself, and print their names
    /// unqualified in `schema inspect` and `schema diff`; SQL the server prints (types, defaults,
    /// constraint definitions) stays schema-qualified (see [`inspect`]). `None` is database mode: every schema is
    /// managed and names are qualified. Plans that are executed ([`schema_plan_apply`]) are always
    /// qualified: an unqualified name means whatever the target's `search_path` makes it mean.
    pub schema: Option<String>,
}

impl Options {
    /// The part of `model` these options manage: [`filter_model`], then [`scope_model`] in schema
    /// mode. Applied to both sides of a diff.
    pub fn managed(&self, model: &Model) -> Model {
        let m = filter_model(model, &self.filter);
        match &self.schema {
            Some(s) => scope_model(&m, s),
            None => m,
        }
    }

    /// How printed plans name objects under these options.
    pub fn plan_options(&self) -> PlanOptions {
        PlanOptions { unqualified_schema: self.schema.clone() }
    }

    /// What a new database holds, as these options see it.
    fn empty_database(&self) -> Model {
        self.managed(&Model::empty_database())
    }
}

/// The model of a source: SQL is executed in a fresh shadow database, then inspected.
pub fn load_model(source: &Source, shadow: &ShadowSpec) -> Result<Model> {
    open_model(source, shadow).map(|(model, _)| model)
}

/// A connection holding a source's schema: its shadow database, or the database itself.
enum Loaded {
    Shadow(Shadow),
    Database(Conn),
}

impl Loaded {
    fn conn(&mut self) -> &mut Conn {
        match self {
            Loaded::Shadow(sh) => sh.conn(),
            Loaded::Database(conn) => conn,
        }
    }
}

/// [`load_model`], keeping the connection open.
fn open_model(source: &Source, shadow: &ShadowSpec) -> Result<(Model, Loaded)> {
    let mut loaded = match source {
        Source::Database(dsn) => Loaded::Database(Conn::connect(dsn)?),
        Source::Sql(path) => Loaded::Shadow(load_sql(path, shadow)?),
    };
    Ok((inspect(loaded.conn())?, loaded))
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

/// `schema diff`: the plan that turns `from` into `to`, for the objects `opts` manages.
pub fn schema_diff(from: &Source, to: &Source, shadow: &ShadowSpec, opts: &Options) -> Result<Plan> {
    diff_sources(from, to, shadow, opts, &opts.plan_options()).map(|(_, plan)| plan)
}

/// The changes that turn `from` into `to` for the objects `opts` manages, and their plan, naming
/// them as `names` says.
fn diff_sources(from: &Source, to: &Source, shadow: &ShadowSpec, opts: &Options, names: &PlanOptions) -> Result<(Vec<Change>, Plan)> {
    // `from`'s shadow is wiped before `to`'s opens: a real Postgres shadow must start empty.
    let a = opts.managed(&load_model(from, shadow)?);
    let (b, mut loaded) = open_model(to, shadow)?;
    let b = opts.managed(&b);
    let changes = diff_with_casts(&a, &b, loaded.conn());
    let plan = plan_with(&changes, names)?;
    Ok((changes, plan))
}

/// `diff(a, b)`, with each type change that has no assignment cast converting `USING` an explicit
/// cast (see [`pgmig_diff::add_casts`]). `conn` holds `b`'s schema, where the new types exist.
pub fn diff_with_casts(a: &Model, b: &Model, conn: &mut Conn) -> Vec<Change> {
    pgmig_diff::add_casts(diff(a, b), a, b, &mut |from, to| assignment_castable(conn, from, to))
}

/// Whether Postgres converts a column of type `from` to `to` without `USING`: asked of the server
/// by making that change to an empty temporary table in a transaction that is rolled back. The
/// server applies its own rules (assignment casts in `pg_cast`, binary coercion, I/O conversion to
/// string types, arrays and domains), so pgmig keeps no table of them.
///
/// A failing `ALTER` answers `false`, and so does an old type `conn` does not have (42704: a column
/// whose type is dropped), where only an explicit cast can work. When the probe cannot start for
/// any other reason (no `TEMP` privilege, a read-only transaction), the answer is `true`: the plan
/// then has no `USING`, and an impossible change fails loudly when applied instead of an explicit
/// cast silently truncating values (`varchar(40)` to `varchar(10)`).
pub fn assignment_castable(conn: &mut Conn, from: &str, to: &str) -> bool {
    let ok = match conn.batch(&format!("BEGIN; CREATE TEMPORARY TABLE pgmig_cast_probe (c {from})")) {
        Ok(()) => conn.batch(&format!("ALTER TABLE pg_temp.pgmig_cast_probe ALTER COLUMN c TYPE {to}")).is_ok(),
        Err(e) => e.code.as_deref() != Some("42704"),
    };
    // Also ends the transaction a failed statement left open.
    let _ = conn.batch("ROLLBACK");
    ok
}

/// `schema inspect`: SQL that recreates the objects `opts` manages in a new database.
pub fn schema_inspect(source: &Source, shadow: &ShadowSpec, opts: &Options) -> Result<Plan> {
    inspect_source(source, shadow, opts).map(|(_, plan)| plan)
}

/// The model of the objects `opts` manages in `source`, and the plan that recreates them.
fn inspect_source(source: &Source, shadow: &ShadowSpec, opts: &Options) -> Result<(Model, Plan)> {
    let m = opts.managed(&load_model(source, shadow)?);
    let plan = plan_with(&diff(&opts.empty_database(), &m), &opts.plan_options())?;
    Ok((m, plan))
}

/// `schema apply`, first half: the plan that turns the database at `target` into `desired`.
///
/// The plan is executed, so its names are always schema-qualified, in schema mode too: there
/// `DROP TABLE x` would drop whichever `x` the target's `search_path` finds first, which need not
/// be the managed schema's. Schema mode still limits which objects are managed.
pub fn schema_plan_apply(target: &str, desired: &Source, shadow: &ShadowSpec, opts: &Options) -> Result<Plan> {
    plan_apply_changes(target, desired, shadow, opts).map(|(_, plan)| plan)
}

/// [`schema_plan_apply`], with the changes the plan makes.
fn plan_apply_changes(target: &str, desired: &Source, shadow: &ShadowSpec, opts: &Options) -> Result<(Vec<Change>, Plan)> {
    diff_sources(&Source::Database(target.to_owned()), desired, shadow, opts, &PlanOptions::default())
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
