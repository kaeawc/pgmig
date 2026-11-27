//! Rename hints (`-- pgmig:rename`, see [`pgmig_diff::rename`]): reading them from desired-state
//! files, and diffing with them.
//!
//! A hint is file-level: it may stand anywhere in a file of a [`crate::Source::Sql`], above a
//! statement or between statements, and is read before anything is executed, so a malformed one is
//! an error at its line ([`Error::Directive`]) in `schema validate` too. A hint in a migration file
//! has no effect (migrations are the history, not a desired state).
//!
//! # Diffing with hints
//!
//! The hints that apply to the current and the desired model ([`pgmig_diff::resolve`]) are
//! planned as renames, first. The rest of the diff is made against the current schema as it is
//! once they ran, so what a rename changes (an index definition naming the column, a foreign key
//! naming the table) is what the server prints and not something pgmig patches. That schema is
//! made by running the renames on a copy of the current model in a shadow database, built the way
//! `--verify` builds its copy ([`crate::verify::Current::Model`]): objects pgmig does not model
//! (extensions, functions, ...) are not in it, and a statement that needs one fails with
//! [`Error::RenameSetup`]. Without a hint that applies nothing is copied and the diff is as without
//! hints.

use std::path::{Path, PathBuf};

use pgmig_diff::{Change, Rename, Suggestion, resolve, suggest};
use pgmig_model::Model;
use serde::{Deserialize, Serialize};

use crate::{Error, Loaded, Options, Result, ShadowSpec, Source, diff_with_casts, files, inspect, open_model, plan, verify};

/// A rename hint in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hint {
    pub rename: Rename,
    pub path: PathBuf,
    pub pos: pgmig_sql::Pos,
}

/// A hint that did not apply, and why. Not an error: a hint can stay in the files after it ran.
/// Serialized as `{"directive", "file", "line", "column", "reason"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnusedHint {
    /// The hint as written: `-- pgmig:rename table public.old new`.
    pub directive: String,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub reason: String,
}

impl std::fmt::Display for UnusedHint {
    /// `schema.sql:3:1: unused rename hint `-- pgmig:rename table public.a b`: ...`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}: unused rename hint `{}`: {}", self.file.display(), self.line, self.column, self.directive, self.reason)
    }
}

/// The rename hints of the files, in file and line order. A hint with arguments that do not parse
/// is an [`Error::Directive`].
pub(crate) fn collect(files: &[files::Loaded]) -> Result<Vec<Hint>> {
    let mut out = Vec::new();
    for f in files {
        collect_file(&f.path, &f.file, &mut out)?;
    }
    Ok(out)
}

fn collect_file(path: &Path, file: &pgmig_sql::File, out: &mut Vec<Hint>) -> Result<()> {
    let all = file.directives.iter().chain(file.statements.iter().flat_map(|s| &s.directives));
    let mut found: Vec<&pgmig_sql::Directive> = all.filter(|d| d.name == "rename").collect();
    found.sort_by_key(|d| d.pos);
    for d in found {
        let rename = pgmig_diff::rename::parse(&d.args).map_err(|message| Error::Directive {
            path: path.to_owned(),
            pos: d.pos,
            message: format!("pgmig:rename: {message}"),
        })?;
        out.push(Hint { rename, path: path.to_owned(), pos: d.pos });
    }
    Ok(())
}

/// The rename hints in SQL text, in line order; errors name `label` as the file. For SQL that is
/// not loaded as a [`Source`] (the diff corpus).
pub fn rename_hints(label: &Path, text: &str) -> Result<Vec<Rename>> {
    let file = pgmig_sql::split(text).map_err(|source| Error::Split { path: label.to_owned(), source })?;
    let mut out = Vec::new();
    collect_file(label, &file, &mut out)?;
    Ok(out.into_iter().map(|h| h.rename).collect())
}

/// What a diff with hints found.
pub(crate) struct Hinted {
    /// The changes: the hinted renames, then the diff of what is left.
    pub changes: Vec<Change>,
    /// Hints to write for drops and adds that look like renames.
    pub suggestions: Vec<Suggestion>,
    /// Hints that did not apply.
    pub unused: Vec<UnusedHint>,
}

/// Where the desired state comes from, for diffing again once the current one has been copied.
pub(crate) struct Desired<'a> {
    pub source: &'a Source,
    pub shadow: &'a ShadowSpec,
    pub opts: &'a Options,
}

/// The changes that turn `a` into `b` (with casts, asked of `loaded`, the connection holding `b`'s
/// schema), applying the hints that apply. `current` is what a copy of `a` is made from: `a` and
/// what it needs ([`crate::clean::with_needs`]).
pub(crate) fn diff_hinted(current: &Model, a: Model, b: &Model, mut loaded: Loaded, desired: &Desired, hints: &[Hint]) -> Result<Hinted> {
    let renames: Vec<Rename> = hints.iter().map(|h| h.rename.clone()).collect();
    let resolution = resolve(&a, b, &renames);
    let mut a = a;
    let mut renamed = Vec::new();
    if !resolution.applied.is_empty() {
        renamed = resolution.applied.iter().map(|(_, c)| c.clone()).collect();
        // A real Postgres shadow holds one database at a time: `b`'s goes before the copy is made,
        // and comes back for the casts.
        drop(loaded);
        a = desired.opts.managed(&renamed_copy(current, &renamed, desired.shadow)?);
        loaded = open_model(desired.source, desired.shadow)?.1;
    }
    let mut changes = renamed;
    changes.extend(diff_with_casts(&a, b, loaded.conn()));
    let suggestions = suggest(&changes, &a);
    let unused = resolution
        .unused
        .into_iter()
        .map(|(i, reason)| {
            let h = &hints[i];
            UnusedHint { directive: h.rename.directive(), file: h.path.clone(), line: h.pos.line, column: h.pos.col, reason }
        })
        .collect();
    Ok(Hinted { changes, suggestions, unused })
}

/// `current` after `renames` ran on a copy of it in a fresh shadow database, as the catalog of that
/// copy says (the model is not patched: the server prints what a rename also changes).
pub fn renamed_copy(current: &Model, renames: &[Change], shadow: &ShadowSpec) -> Result<Model> {
    let mut sh = shadow.open()?;
    let setup = |statement: &str, source| Error::RenameSetup { statement: statement.to_owned(), source };
    verify::recreate(sh.conn(), current).map_err(|e| match e {
        Error::VerifySetup { statement, source } => setup(&statement, source),
        e => e,
    })?;
    for statement in plan(renames)?.statements {
        sh.conn().batch(&statement).map_err(|source| setup(&statement, source))?;
    }
    Ok(inspect(sh.conn())?)
}

/// What the hints say about `after` (a model of the current state), compared with `to`: the
/// renames that apply, and `after` once they ran. Used where a plan is checked against the desired
/// state (`--verify`): a plan that leaves a rename out (the policy skips `rename_table`, say)
/// leaves it to find again.
pub(crate) fn renamed_state(
    full: &Model,
    after: Model,
    to: &Model,
    renames: &[Rename],
    shadow: &ShadowSpec,
    opts: &Options,
) -> Result<(Vec<Change>, Model)> {
    let resolution = resolve(&after, to, renames);
    if resolution.applied.is_empty() {
        return Ok((Vec::new(), after));
    }
    let changes: Vec<Change> = resolution.applied.into_iter().map(|(_, c)| c).collect();
    let copied = crate::clean::with_needs(full, &after);
    let after = opts.managed(&renamed_copy(&copied, &changes, shadow)?);
    Ok((changes, after))
}
