//! `--verify`: before a plan runs for real, apply it in a fresh shadow database to a copy of the
//! current state and check that the result is the desired state.
//!
//! The copy is made the way `migrate baseline` writes a schema: the plan from a new database to the
//! current state's model, for the objects the options manage. Then the plan under test runs as it
//! would for real: a [`Plan`] batch by batch, each in its own transaction (as [`crate::apply`]); a
//! migration file as `migrate apply` runs it, in one transaction committed at each
//! `-- pgmig:commit`, or statement by statement with `-- pgmig:txmode none`. The result is
//! inspected and diffed against the desired state. Changes the policy skips ([`Options::skip`])
//! are expected to remain, with those that depend on them; any other change left is an error. The
//! shadow is wiped (dropped) before returning, whatever the outcome.

use std::path::PathBuf;

use crate::{Change, Conn, Error, Model, Options, Plan, PlanOptions, Result, ShadowSpec, diff, inspect, plan_with};

/// Statements run together: in one transaction, or each committing on its own.
struct Unit<'a> {
    /// Each statement with its 0-based index in the plan or file.
    statements: Vec<(usize, &'a str)>,
    transaction: bool,
}

/// Check that `plan`, applied to `from` (the current state's model, as `opts` manages it),
/// reaches `to` (the desired state's, likewise) but for the changes `opts` skips.
pub fn verify_plan(from: &Model, plan: &Plan, to: &Model, shadow: &ShadowSpec, opts: &Options) -> Result<()> {
    let units: Vec<Unit> = crate::batch_ranges(plan)
        .into_iter()
        .map(|r| Unit { statements: r.map(|i| (i, plan.statements[i].as_str())).collect(), transaction: true })
        .collect();
    verify_units(from, &units, to, shadow, opts)
}

/// [`verify_plan`] for the text of a migration file (what `--edit` saved): split, and run as
/// `migrate apply` runs a file, honouring its `-- pgmig:txmode` and `-- pgmig:commit` directives.
/// Errors in the text name it `label`.
pub fn verify_migration_sql(from: &Model, sql: &str, label: &str, to: &Model, shadow: &ShadowSpec, opts: &Options) -> Result<()> {
    let path = PathBuf::from(label);
    let file = pgmig_sql::split(sql).map_err(|source| Error::Split { path: path.clone(), source })?;
    let (no_tx, commits) = crate::migrate_apply::check_directives(&path, &file)?;
    let all: Vec<(usize, &str)> = file.statements.iter().enumerate().map(|(i, s)| (i, s.sql.as_str())).collect();
    let units = if no_tx {
        vec![Unit { statements: all, transaction: false }]
    } else {
        let mut units = Vec::new();
        let mut start = 0;
        for end in commits.iter().map(|(i, _)| *i).chain(std::iter::once(all.len())) {
            if end > start {
                units.push(Unit { statements: all[start..end].to_vec(), transaction: true });
            }
            start = end;
        }
        units
    };
    verify_units(from, &units, to, shadow, opts)
}

fn verify_units(from: &Model, units: &[Unit], to: &Model, shadow: &ShadowSpec, opts: &Options) -> Result<()> {
    // Dropped on return: the shadow is wiped whatever happens.
    let mut sh = shadow.open()?;
    let conn = sh.conn();
    recreate(conn, from, opts)?;
    for unit in units {
        run_unit(conn, unit)?;
    }
    let after = opts.managed(&inspect(conn)?);
    let names = PlanOptions::default();
    let (remaining, _expected) = opts.apply_policy(&diff(&after, to), &names);
    if remaining.is_empty() { Ok(()) } else { Err(Error::VerifyFailed { remaining }) }
}

/// Make the empty shadow hold `from`: in schema mode its schema first (a schema-mode model leaves
/// the schema out), then the plan from a new database.
fn recreate(conn: &mut Conn, from: &Model, opts: &Options) -> Result<()> {
    let setup = |statement: &str, source| Error::VerifySetup { statement: statement.to_owned(), source };
    if let Some(s) = &opts.schema {
        let sql = format!("CREATE SCHEMA IF NOT EXISTS {}", pgmig_model::quote_ident(s));
        conn.batch(&sql).map_err(|e| setup(&sql, e))?;
    }
    let plan = plan_with(&diff(&opts.empty_database(), from), &PlanOptions::default())?;
    for range in crate::batch_ranges(&plan) {
        let mut statements = std::iter::once("BEGIN").chain(range.map(|i| plan.statements[i].as_str())).chain(std::iter::once("COMMIT"));
        if let Some((statement, e)) = statements.find_map(|s| conn.batch(s).err().map(|e| (s, e))) {
            let _ = conn.batch("ROLLBACK");
            return Err(setup(statement, e));
        }
    }
    Ok(())
}

fn run_unit(conn: &mut Conn, unit: &Unit) -> Result<()> {
    let fail = |conn: &mut Conn, statement_index, statement: &str, source| {
        if unit.transaction {
            let _ = conn.batch("ROLLBACK");
        }
        Error::VerifyStatement { statement_index, statement: statement.to_owned(), source }
    };
    if unit.transaction {
        conn.batch("BEGIN").map_err(|e| fail(conn, None, "BEGIN", e))?;
    }
    for (i, statement) in &unit.statements {
        if let Err(e) = conn.batch(statement) {
            return Err(fail(conn, Some(*i), statement, e));
        }
    }
    if unit.transaction {
        conn.batch("COMMIT").map_err(|e| fail(conn, None, "COMMIT", e))?;
    }
    Ok(())
}

/// The remaining changes as messages list them: `add_column public.t.c`.
pub(crate) fn remaining_message(remaining: &[Change]) -> String {
    let n = remaining.len();
    let list: String = remaining.iter().map(|c| format!("\n  {} {}", c.kind(), c.object())).collect();
    format!(
        "verification failed: applied to a copy of the current state in the shadow database, the plan leaves {n} change{} to make:{list}\nnothing was applied or written",
        if n == 1 { "" } else { "s" }
    )
}

/// A plan statement that failed in the shadow database.
pub(crate) fn statement_message(index: &Option<usize>, statement: &str, source: &pgmig_conn::Error) -> String {
    let what = match index {
        Some(i) => format!("statement {} failed in the shadow database: {statement}", i + 1),
        None => format!("{statement} failed in the shadow database"),
    };
    format!("verification failed: {what}\n{source}\nnothing was applied or written")
}
