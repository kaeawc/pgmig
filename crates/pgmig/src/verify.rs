//! `--verify`: before a plan runs for real, apply it in a fresh shadow database to a copy of the
//! current state and check that the result is the desired state.
//!
//! The copy is schema only: no rows. A change that fails or misbehaves only because of the data
//! (`SET NOT NULL` over NULLs, a unique index over duplicates, a `USING` cast a value does not
//! survive) is not caught. How the copy is made depends on the current state ([`Current`]):
//!
//! - A migration directory (`migrate diff`) is replayed, every file executed as written, so the
//!   copy is faithful: extensions, functions, views and anything else the files create are there.
//! - A live database (`schema apply`) can only be rebuilt from what pgmig models: the plan from a
//!   new database to a model of the objects the options manage, plus what they need (the closure
//!   `schema clean` keeps: schemas, foreign-key target tables, enums, domains, composites and
//!   sequences they use, transitively) and the unmanaged tables that hold a foreign key to a
//!   managed table, with what those need. Other unmanaged objects are not copied, so one that uses
//!   something pgmig does not model cannot break setup; and dropping a managed table an unmanaged
//!   table references fails verification as it fails for real. Objects pgmig does not model
//!   (extensions, functions, views, ...) are not copied; a statement that needs one fails setup
//!   ([`Error::VerifySetup`]).
//!
//! Then the plan under test runs as it would for real: a [`Plan`] unit by unit, a transactional one
//! in its own transaction and a non-transactional statement (`CREATE INDEX CONCURRENTLY`) outside
//! any (as [`crate::apply`]); a migration file as `migrate apply` runs it, in one
//! transaction committed at each `-- pgmig:commit`, or statement by statement with
//! `-- pgmig:txmode none`. The result, as the options manage it, is inspected and diffed against
//! the desired state. Changes the policy skips ([`Options::skip`]) are expected to remain, with
//! those that depend on them; any other change left is an error. The shadow is wiped (dropped)
//! before returning, whatever the outcome.
//!
//! Only the shadow database is changed, but statements that act outside any database
//! (`CREATE ROLE`, `ALTER SYSTEM`, ...) in an edited plan or in a migration really run there, on
//! the shadow's server: on a real Postgres shadow they take effect on that server.

use std::path::PathBuf;

use crate::{Change, Conn, Error, MigrationDir, Model, Options, Plan, PlanOptions, Result, Shadow, ShadowSpec, diff, inspect, plan_with};

/// The current state a plan under test starts from, copied into the shadow database.
#[derive(Debug, Clone, Copy)]
pub enum Current<'a> {
    /// A live database's model, rebuilt from what pgmig models: the managed objects, what they
    /// need and the unmanaged tables referencing them ([`crate::clean::with_needs`]).
    Model(&'a Model),
    /// A migration directory, replayed file by file; `None` is a directory that does not exist
    /// yet (an empty state).
    Migrations(Option<&'a MigrationDir>),
}

/// Statements run together: in one transaction, or each committing on its own.
struct Unit<'a> {
    /// Each statement with its 0-based index in the plan or file.
    statements: Vec<(usize, &'a str)>,
    transaction: bool,
}

/// Check that `plan`, applied to a copy of `current`, reaches `to` (the desired state's model, as
/// `opts` manages it) but for the changes `opts` skips.
pub fn verify_plan(current: Current, plan: &Plan, to: &Model, shadow: &ShadowSpec, opts: &Options) -> Result<()> {
    let units = plan_units(plan);
    verify_units(current, &units, to, shadow, opts)
}

/// The units of `plan` as verification runs them.
fn plan_units(plan: &Plan) -> Vec<Unit<'_>> {
    plan.units()
        .into_iter()
        .map(|u| Unit { statements: u.statements.map(|i| (i, plan.statements[i].as_str())).collect(), transaction: u.transactional })
        .collect()
}

/// [`verify_plan`] for the text of a migration file (what `--edit` saved): split, and run as
/// `migrate apply` runs a file, honouring its `-- pgmig:txmode` and `-- pgmig:commit` directives.
/// Errors in the text name it `label`.
pub fn verify_migration_sql(current: Current, sql: &str, label: &str, to: &Model, shadow: &ShadowSpec, opts: &Options) -> Result<()> {
    let path = PathBuf::from(label);
    // Assertions are not run: the shadow has no data.
    let (file, _) = crate::migrate_apply::split_migration_text(&path, sql)?;
    let (no_tx, commits) = crate::migrate_apply::check_directives(&path, &file)?;
    let all: Vec<(usize, &str)> = file.statements.iter().enumerate().map(|(i, s)| (i, s.sql.as_str())).collect();
    let units: Vec<Unit> = crate::migrate_apply::segments(no_tx, &commits, all.len())
        .into_iter()
        .map(|seg| Unit { statements: all[seg.range].to_vec(), transaction: seg.transaction })
        .collect();
    verify_units(current, &units, to, shadow, opts)
}

fn verify_units(current: Current, units: &[Unit], to: &Model, shadow: &ShadowSpec, opts: &Options) -> Result<()> {
    // Dropped on return: the shadow is wiped whatever happens.
    let mut sh = copy(current, shadow)?;
    let conn = sh.conn();
    for unit in units {
        run_unit(conn, unit)?;
    }
    let after = opts.managed(&inspect(conn)?);
    let names = PlanOptions::default();
    let (remaining, _expected) = opts.apply_policy(&diff(&after, to), &names);
    if remaining.is_empty() { Ok(()) } else { Err(Error::VerifyFailed { remaining }) }
}

/// A fresh shadow holding `current`.
fn copy(current: Current, shadow: &ShadowSpec) -> Result<Shadow> {
    match current {
        Current::Migrations(dir) => crate::replay_or_empty(dir, shadow),
        Current::Model(from) => {
            let mut sh = shadow.open()?;
            recreate(sh.conn(), from)?;
            Ok(sh)
        }
    }
}

/// Make the empty shadow hold `from`: the plan from a new database.
fn recreate(conn: &mut Conn, from: &Model) -> Result<()> {
    let setup = |statement: &str, source| Error::VerifySetup { statement: statement.to_owned(), source };
    let plan = plan_with(&diff(&Model::empty_database(), from), &PlanOptions::default())?;
    for unit in plan_units(&plan) {
        let transaction = unit.transaction;
        let mut statements = transaction
            .then_some("BEGIN")
            .into_iter()
            .chain(unit.statements.iter().map(|(_, s)| *s))
            .chain(transaction.then_some("COMMIT"));
        if let Some((statement, e)) = statements.find_map(|s| conn.batch(s).err().map(|e| (s, e))) {
            if transaction {
                let _ = conn.batch("ROLLBACK");
            }
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
        "verification failed: applied to a copy of the current state in the shadow database, the plan leaves {n} change{} to make:{list}\nthe target was not changed and no migration was written",
        if n == 1 { "" } else { "s" }
    )
}

/// A plan statement that failed in the shadow database.
pub(crate) fn statement_message(index: &Option<usize>, statement: &str, source: &pgmig_conn::Error) -> String {
    let what = match index {
        Some(i) => format!("statement {} failed in the shadow database: {statement}", i + 1),
        None => format!("{statement} failed in the shadow database"),
    };
    format!("verification failed: {what}\n{source}\nthe target was not changed and no migration was written")
}
