//! `drift check`: compare a live database with what it should be, and print the plan that brings it
//! back.
//!
//! What it should be is either a migration directory at the version the database's revisions table
//! records, or any [`Source`]: desired-state SQL, a migration directory replayed to its newest file,
//! or another database. With a migration directory, the directory is replayed in a fresh shadow
//! database exactly as far as the history says it was applied, in the order the files ran (the rows'
//! execution times, the version breaking ties; version order when a row has no time, as `migrate
//! apply --exec-order non-linear` can run an older file last): every file with a
//! complete row (baselines included: their file is the state they recorded), and of a file that
//! stopped part way only the statements its row records as applied. Files without a row are pending:
//! listed, not replayed, not drift. The history is checked as `migrate status` checks it; each
//! problem (an applied file edited or missing, a failed or interrupted file, a statement left in
//! flight, a row edited by hand, a file out of order, a directory that does not match its sum file)
//! is reported with how to recover, and counts as drift.
//!
//! The live database and the expected state are compared as `schema diff` compares them, for the
//! objects the options manage, under the diff policy (skipped changes are reported and are not
//! drift). The plan turns the live database into the expected state; it will be executed, so it is
//! always schema-qualified. Changes the planner cannot plan are listed in
//! [`DriftCheck::unsupported`] instead of failing the command; the plan is then empty.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::migrate_status::{StatusIssue, read_status};
use crate::{
    Change, Conn, Options, Plan, Result, Shadow, ShadowSpec, Skipped, Source, check_no_imports, diff_with_casts, inspect, migrate_apply,
    open_model, plan_with, split_migration,
};

/// What a live database is compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// A migration directory, at the version the database's revisions table records.
    Applied(PathBuf),
    /// Any state: SQL files, a migration directory replayed to its newest file, another database.
    Source(Source),
}

/// What [`drift_check`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftCheck {
    /// The database is not as expected: there are changes to make (not counting those the policy
    /// skips), or the history has problems.
    pub drifted: bool,
    /// The changes that turn the live database into the expected state.
    pub changes: Vec<Change>,
    /// The changes the policy (`skip`) left out of the plan.
    #[serde(default)]
    pub skipped: Vec<Skipped>,
    /// The plan that turns the live database into the expected state (schema-qualified). Empty
    /// when some change cannot be planned ([`DriftCheck::unsupported`]).
    pub plan: Plan,
    /// `plan` rendered as a SQL script ([`Plan::to_sql`]).
    pub sql: String,
    /// One message per change the planner cannot plan; they are in `changes` too.
    #[serde(default)]
    pub unsupported: Vec<String>,
    /// With a migration directory: what its history says. Null for any other expected state.
    pub history: Option<DriftHistory>,
}

/// The history part of [`DriftCheck`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftHistory {
    pub dir: PathBuf,
    /// The revisions table; null when the database has none (nothing applied: the expected state is
    /// an empty database).
    pub revisions_table: Option<String>,
    /// The newest version recorded as complete; null when none is.
    pub current: Option<String>,
    /// The files replayed to build the expected state, in order.
    pub replayed: Vec<String>,
    /// The files of these that are replayed only in part: those their row records as applied.
    #[serde(default)]
    pub partial: Vec<String>,
    /// The files `migrate apply` would run, not counting those replayed in part. Not drift.
    pub pending: Vec<String>,
    /// Problems with the history, each with how to recover (as `migrate status` reports them). Each
    /// counts as drift.
    pub issues: Vec<StatusIssue>,
    /// What to know before running the plan: the history has problems, or has no revisions table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// `drift check`: compare the database at `target` with `expected`, for the objects `opts` manages.
/// Only reads the target.
pub fn drift_check(target: &str, expected: &Expected, shadow: &ShadowSpec, opts: &Options) -> Result<DriftCheck> {
    opts.validate()?;
    let live = {
        let mut conn = Conn::connect(target)?;
        opts.managed(&inspect(&mut conn)?)
    };
    let (history, (model, mut conn)) = match expected {
        Expected::Source(source) => (None, open_model(source, shadow)?),
        Expected::Applied(dir) => {
            let (history, mut sh) = replay_applied(target, dir, shadow, &opts.revisions_schema)?;
            let model = inspect(sh.conn())?;
            (Some(history), (model, crate::Loaded::Shadow(sh)))
        }
    };
    let model = opts.managed(&model);
    let names = opts.qualified_plan_options();
    let (changes, skipped) = opts.apply_policy(&diff_with_casts(&live, &model, conn.conn()), &names);
    drop(conn);
    let (plan, unsupported) = match plan_with(&changes, &names) {
        Ok(plan) => (plan, Vec::new()),
        Err(pgmig_plan::Error::Unsupported(messages)) => (Plan::default(), messages),
        Err(e) => return Err(e.into()),
    };
    let drifted = !changes.is_empty() || history.as_ref().is_some_and(|h| !h.issues.is_empty());
    Ok(DriftCheck { drifted, sql: plan.to_sql(), changes, skipped, plan, unsupported, history })
}

/// The directory at `dir` replayed in a fresh shadow database as far as the revisions table of the
/// database at `target` records it applied, and the history as `migrate status` reads it.
fn replay_applied(target: &str, dir: &Path, shadow: &ShadowSpec, revisions_schema: &str) -> Result<(DriftHistory, Shadow)> {
    let (status, d, rows) = read_status(target, dir, revisions_schema)?;
    check_no_imports(&d)?;
    let mut sh = shadow.open()?;
    let (mut replayed, mut partial) = (Vec::new(), Vec::new());
    // The files with a row, in the order they ran.
    let mut ran: Vec<_> = d
        .files()
        .iter()
        .filter_map(|f| rows.iter().find(|s| pgmig_migrate::compare_versions(&s.revision.version, &f.version).is_eq()).map(|row| (f, row)))
        .collect();
    if !ran.is_empty() {
        let mut conn = pgmig_conn::Conn::connect(target)?;
        if let Some(table) = pgmig_migrate::revisions::Table::open(&mut conn, revisions_schema)? {
            let order = table.versions_by_execution(&mut conn)?;
            let position = |v: &str| order.iter().position(|o| pgmig_migrate::compare_versions(o, v).is_eq()).unwrap_or(usize::MAX);
            // Stable: ties keep version (directory) order.
            ran.sort_by_key(|(_, row)| position(&row.revision.version));
        }
    }
    for (f, row) in ran {
        let r = &row.revision;
        let mut file = split_migration(&d, f)?;
        if !r.is_complete() {
            if r.applied == 0 {
                continue;
            }
            file.statements.truncate(r.applied as usize);
            partial.push(f.name.clone());
        }
        migrate_apply::run_file(sh.conn(), &d.path.join(&f.name), &file)?;
        replayed.push(f.name.clone());
    }
    let mut warnings = Vec::new();
    if status.revisions_table.is_none() {
        warnings.push(format!("no revisions table in {revisions_schema}: the expected state is an empty database"));
    }
    if !status.issues.is_empty() {
        warnings.push(
            "the history has problems (listed above); the plan below brings the database to the directory as it is now and may drop \
             objects: resolve the problems first"
                .into(),
        );
    }
    let pending = status.pending.into_iter().filter(|f| !partial.contains(f)).collect();
    let history = DriftHistory {
        dir: status.dir,
        revisions_table: status.revisions_table,
        current: status.current,
        replayed,
        partial,
        pending,
        issues: status.issues,
        warnings,
    };
    Ok((history, sh))
}
