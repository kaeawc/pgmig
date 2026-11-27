//! Saved plans: the plan `schema apply` would run, written to a file together with the
//! fingerprints of the schema it was made from and the schema it makes, so that applying the file
//! runs exactly the SQL that was reviewed, and only on a database that is still as it was planned.
//!
//! The file is JSON (see [`SavedPlan`]). Applying it ([`apply_saved`]) fingerprints the target's
//! managed schema under the file's own options and refuses when that is not `from`. Between that
//! check and the plan's first statement another session can still change the schema: pgmig takes no
//! lock, so the check narrows the window (it is repeated right before the apply) without closing it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ApplyReport, Error, Options, Plan, Result, ShadowSpec, Skipped, Source};

/// The version of the file layout, `format` in the file. A file with another number is refused.
pub const FORMAT: u32 = 1;

/// A saved plan. Serialized as `{"format": 1, "pgmig_version": ..., "from": "h1:...", "to":
/// "h1:...", "options": {...}, "plan": {...}, "sql": "..."}`, with `"skipped"` after `to` when the
/// options' policy left changes out.
///
/// `plan` is what runs. `sql` is [`Plan::to_sql`] of it, there for whoever reads the file:
/// applying refuses a file whose `sql` is not its plan's, so editing one of them by hand cannot
/// make the reviewed text differ from the executed statements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedPlan {
    /// Always [`FORMAT`] for a file this version writes.
    pub format: u32,
    /// The version of pgmig that wrote the file. A refusal by another version says so.
    pub pgmig_version: String,
    /// The fingerprint ([`pgmig_model::fingerprint`]) of the target's managed objects when planned.
    pub from: String,
    /// The fingerprint of the desired state's managed objects.
    pub to: String,
    /// The changes the options' policy (`skip`) left out of the plan. When there are some, the
    /// result of applying is not expected to have fingerprint `to`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// What is managed and how: the plan and both fingerprints are relative to these options.
    pub options: Options,
    /// The statements to run, schema-qualified, with their transaction units.
    pub plan: Plan,
    /// `plan` as a SQL script.
    pub sql: String,
}

impl SavedPlan {
    /// Read a saved plan from JSON text: an unknown `format` is refused by name before anything
    /// else is read.
    pub fn parse(text: &str) -> Result<SavedPlan> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| Error::SavedPlan(format!("not valid JSON: {e}")))?;
        match value.get("format") {
            Some(f) if f.as_u64() == Some(u64::from(FORMAT)) => {}
            Some(f) => return Err(Error::SavedPlan(format!("format {f} is not supported (this pgmig reads format {FORMAT})"))),
            None => return Err(Error::SavedPlan("not a saved plan: it has no `format`".into())),
        }
        let saved: SavedPlan = serde_json::from_value(value).map_err(|e| Error::SavedPlan(e.to_string()))?;
        saved.check_consistent()?;
        Ok(saved)
    }

    /// Read the file at `path` ([`SavedPlan::parse`]).
    pub fn read(path: &Path) -> Result<SavedPlan> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io { path: path.to_owned(), source })?;
        SavedPlan::parse(&text).map_err(|e| match e {
            Error::SavedPlan(m) => Error::SavedPlan(format!("{}: {m}", path.display())),
            e => e,
        })
    }

    /// The file's text: pretty JSON ending in a newline.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a saved plan is JSON");
        text.push('\n');
        text
    }

    /// Whether the plan is one this version runs and `sql` is its script.
    fn check_consistent(&self) -> Result<()> {
        if self.format != FORMAT {
            return Err(Error::SavedPlan(format!("format {} is not supported (this pgmig reads format {FORMAT})", self.format)));
        }
        if self.sql != self.plan.to_sql() {
            return Err(Error::SavedPlan(format!(
                "`sql` is not the SQL of `plan`: the file was edited, and what is shown would not be what runs{}",
                self.version_note()
            )));
        }
        Ok(())
    }

    /// When another version of pgmig wrote the file, a note saying so: that version may print SQL or
    /// fingerprint a schema differently, so a refusal is no sign the file or the target changed.
    fn version_note(&self) -> String {
        let this = env!("CARGO_PKG_VERSION");
        if self.pgmig_version == this {
            String::new()
        } else {
            format!(" (the plan was made with pgmig {}, this is pgmig {this}: plan again with this version)", self.pgmig_version)
        }
    }
}

/// `schema plan`: the plan `schema apply` would run for `target` and `desired`, saved with the
/// fingerprints of both sides. With `verify`, it is checked first ([`crate::verify_plan`]). `target`
/// is only read.
pub fn schema_plan_saved(target: &str, desired: &Source, shadow: &ShadowSpec, opts: &Options, verify: bool) -> Result<SavedPlan> {
    let d = crate::plan_apply_changes(target, desired, shadow, opts)?;
    if verify {
        crate::verify_plan(crate::Current::Model(&d.current), &d.plan, &d.to, shadow, opts)?;
    }
    Ok(SavedPlan {
        format: FORMAT,
        pgmig_version: env!("CARGO_PKG_VERSION").to_owned(),
        from: d.from_fingerprint,
        to: pgmig_model::fingerprint(&d.to),
        skipped: d.skipped,
        options: opts.clone(),
        sql: d.plan.to_sql(),
        plan: d.plan,
    })
}

/// The fingerprint of the objects `opts` manages in the database at `target`.
fn target_fingerprint(target: &str, opts: &Options) -> Result<String> {
    crate::schema_fingerprint(&Source::Database(target.to_owned()), &ShadowSpec::default(), opts)
}

/// Refuse `saved` unless it is consistent ([`Error::SavedPlan`]) and the managed schema of `target`
/// still has the fingerprint it was planned from ([`Error::PlanStale`]). Changes nothing.
pub fn check_saved(target: &str, saved: &SavedPlan) -> Result<()> {
    saved.check_consistent()?;
    saved.options.validate()?;
    let actual = target_fingerprint(target, &saved.options)?;
    if actual != saved.from {
        return Err(Error::PlanStale { expected: saved.from.clone(), actual, note: saved.version_note() });
    }
    Ok(())
}

/// What [`apply_saved`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedApplied {
    pub report: ApplyReport,
    /// The fingerprint of the target's managed schema after the plan ran.
    pub fingerprint: String,
    /// Whether `fingerprint` was compared with the plan's `to`: not when the policy skipped changes,
    /// since the result is then expected to differ.
    pub compared: bool,
}

/// `schema apply --plan`: [`check_saved`], then run the plan ([`crate::apply`]: its units, commit
/// points and statements outside a transaction), then fingerprint the target again. When that is not
/// the plan's `to` (and the policy skipped nothing) the changes stay applied and the result is
/// [`Error::PlanDiverged`], so that a caller notices.
pub fn apply_saved(target: &str, saved: &SavedPlan) -> Result<SavedApplied> {
    check_saved(target, saved)?;
    let report = crate::apply(target, &saved.plan)?;
    let fingerprint = target_fingerprint(target, &saved.options)?;
    let compared = saved.skipped.is_empty();
    if compared && fingerprint != saved.to {
        return Err(Error::PlanDiverged { expected: saved.to.clone(), actual: fingerprint, statements: report.statements });
    }
    Ok(SavedApplied { report, fingerprint, compared })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved() -> SavedPlan {
        let plan = Plan { statements: vec!["CREATE SCHEMA app".into()], ..Plan::default() };
        SavedPlan {
            format: FORMAT,
            pgmig_version: "0".into(),
            from: "h1:a".into(),
            to: "h1:b".into(),
            skipped: Vec::new(),
            options: Options::default(),
            sql: plan.to_sql(),
            plan,
        }
    }

    #[test]
    fn round_trips_and_checks_its_sql() {
        let s = saved();
        assert_eq!(SavedPlan::parse(&s.to_json()).expect("parse"), s);
        let tampered = s.to_json().replace("CREATE SCHEMA app;", "DROP SCHEMA app;");
        assert!(matches!(SavedPlan::parse(&tampered), Err(Error::SavedPlan(m)) if m.contains("`sql` is not the SQL of `plan`")));
    }

    #[test]
    fn refuses_other_formats_by_name() {
        let other = saved().to_json().replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(SavedPlan::parse(&other), Err(Error::SavedPlan(m)) if m.contains("format 2 is not supported")));
        assert!(matches!(SavedPlan::parse("{}"), Err(Error::SavedPlan(m)) if m.contains("no `format`")));
        assert!(matches!(SavedPlan::parse("nope"), Err(Error::SavedPlan(_))));
    }
}
