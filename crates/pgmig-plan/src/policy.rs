//! A diff policy: kinds of change that are never planned ([`skip`]).
//!
//! A change of a skipped kind is left out of the plan and reported ([`Skipped`]), never dropped
//! silently. Leaving a change out can break another one, so a change that depends on a skipped
//! change is skipped too, and reported with the change it depends on ([`Skipped::because`]). A kept
//! change `c` depends on a skipped change `s` when, in the plan graph's terms ([`crate::graph`]):
//!
//! - `c` drops an object `s` releases (`s` would have stopped using it first): skipping a
//!   table's `drop_table` keeps its schema's `drop_schema` out, and skipping a foreign key's
//!   `drop_constraint` keeps the drop of the table it references out;
//! - `c` creates an object `s` drops (a constraint dropped and added again with a new definition:
//!   the add would find the old one), or drops an object `s` creates (the old one would be lost):
//!   a replacement, the drop and the add or create of one constraint (by table and name) or index
//!   (by name), is kept or skipped as a pair. A foreign key dropped and added again around the
//!   index it uses is skipped with that index's replacement, so it is never dropped and left out;
//! - `c` requires or alters an object `s` creates (a column added by a skipped `add_column`);
//!
//! and, for what the graph does not record:
//!
//! - with `drop_sequence` skipped, a `drop_column`, `drop_table` or `drop_identity` that would
//!   drop a sequence implicitly (one the column or table owns, an identity's) is skipped, reported
//!   with that sequence's `drop_sequence` as the cause;
//! - a column whose type change is skipped keeps its old type: a constraint added or altered over
//!   the column, an index created over it, and a change ordered against its type (a foreign key
//!   referencing it), is skipped;
//! - an enum value whose `add_enum_value` (or `rename_enum_value`, by the new label) is skipped
//!   does not exist: a change whose statement names it as a literal of that type
//!   (`'label'::type`, as the server prints a default or a check) is skipped. The match is on
//!   text and conservative: it can skip a change that only mentions the literal;
//!
//! and on one column:
//!
//! - a type change converting `USING`, the `drop_default` before it and the `set_default` after
//!   it (see `pgmig_diff::add_casts`) go together: skipping one skips the others;
//! - `add_identity` needs the column's default dropped and `NOT NULL` set: it is skipped with a
//!   skipped `drop_default` or `set_not_null`;
//! - an identity column cannot take a default or drop `NOT NULL`: `set_default` and
//!   `drop_not_null` are skipped with a skipped `drop_identity`;
//! - a primary key column cannot drop `NOT NULL`: `drop_not_null` is skipped with the primary
//!   key's skipped `drop_constraint`.
//!
//! The rule is applied until nothing more is skipped, so what is left always plans and applies.
//!
//! A skipped change leaves a difference between the database and the desired state, so every
//! later diff finds it again and reports it as skipped again: with `drop_table` skipped, a table
//! removed from the desired state stays in the database and in every report.

use std::collections::BTreeSet;

use pgmig_diff::{Change, ChangeKind};
use pgmig_model::{ConstraintKind, QualName, quote_ident, quote_literal};
use serde::{Deserialize, Serialize};

use crate::graph::{Deps, Object};
use crate::{PlanOptions, deps, is_unsupported, statement};

/// A change the policy left out. Serialized as `{"kind", "object", "statement", "because"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skipped {
    pub kind: ChangeKind,
    /// The object, as [`Change::object`] names it.
    pub object: String,
    /// The statement the change would have run; `None` for a change that has none (an
    /// unsupported change).
    pub statement: Option<String>,
    /// `None` when the kind is skipped; otherwise the skipped change this one depends on.
    pub because: Option<Cause>,
}

/// The skipped change another one was skipped for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cause {
    pub kind: ChangeKind,
    pub object: String,
}

impl std::fmt::Display for Skipped {
    /// `drop_table public.old (policy)`, or `drop_schema app (because drop_table app.t is skipped)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.because {
            None => write!(f, "{} {} (policy)", self.kind, self.object),
            Some(c) => write!(f, "{} {} (because {} {} is skipped)", self.kind, self.object, c.kind, c.object),
        }
    }
}

/// The changes to plan and the ones left out: those of a kind in `kinds`, and those that depend on
/// a change left out (see the module doc). Both keep the order of `changes`; statements are
/// printed as `opts` says.
pub fn skip(changes: &[Change], kinds: &BTreeSet<ChangeKind>, opts: &PlanOptions) -> (Vec<Change>, Vec<Skipped>) {
    if kinds.is_empty() {
        return (changes.to_vec(), Vec::new());
    }
    let deps: Vec<Deps> = changes.iter().map(deps).collect();
    let statements: Vec<Option<String>> = changes.iter().map(|c| (!is_unsupported(c)).then(|| statement(c, opts))).collect();
    let cause = |j: usize| Cause { kind: changes[j].kind(), object: changes[j].object() };
    // For each change: None (kept), or Some(None) (skipped by kind), or Some(Some(cause)).
    let mut skipped: Vec<Option<Option<Cause>>> = changes.iter().map(|c| kinds.contains(&c.kind()).then_some(None)).collect();
    // A sequence Postgres drops with a column, a table or an identity is dropped all the same.
    if kinds.contains(&ChangeKind::DropSequence) {
        for (i, change) in changes.iter().enumerate() {
            if skipped[i].is_none()
                && let Some(seq) = implicit_sequence_drops(change).first()
            {
                skipped[i] = Some(Some(Cause { kind: ChangeKind::DropSequence, object: seq.to_string() }));
            }
        }
    }
    let mut work: Vec<usize> = (0..changes.len()).filter(|&i| skipped[i].is_some()).collect();
    while let Some(s) = work.pop() {
        for c in 0..changes.len() {
            if skipped[c].is_none() && depends(&changes[c], &deps[c], statements[c].as_deref(), &changes[s], &deps[s]) {
                skipped[c] = Some(Some(cause(s)));
                work.push(c);
            }
        }
    }
    let mut kept = Vec::new();
    let mut out = Vec::new();
    for ((change, skip), statement) in changes.iter().zip(skipped).zip(statements) {
        match skip {
            None => kept.push(change.clone()),
            Some(because) => out.push(Skipped { kind: change.kind(), object: change.object(), statement, because }),
        }
    }
    (kept, out)
}

/// The sequences Postgres drops with `c` without a `DROP SEQUENCE`: those a dropped column or
/// table owns, and a dropped identity's.
fn implicit_sequence_drops(c: &Change) -> &[QualName] {
    match c {
        Change::DropColumn { owned_sequences, .. } | Change::DropTable { owned_sequences, .. } => owned_sequences,
        Change::DropIdentity { sequence, .. } => std::slice::from_ref(sequence),
        _ => &[],
    }
}

/// Whether kept change `c` (printed as `statement`) cannot run without skipped change `s`.
fn depends(c: &Change, cd: &Deps, statement: Option<&str>, s: &Change, sd: &Deps) -> bool {
    let meets = |a: &[Object], b: &[Object]| a.iter().any(|o| b.contains(o));
    if meets(&cd.drops, &sd.releases)
        || meets(&cd.creates, &sd.drops)
        || meets(&cd.drops, &sd.creates)
        || meets(&cd.requires, &sd.creates)
        || meets(&cd.alters, &sd.creates)
    {
        return true;
    }
    match s {
        // A column keeping its old type: what is defined for the new one waits with the change.
        Change::AlterColumnType { table, column, .. } => {
            let column_type = Object::ColumnType(table.clone(), column.clone());
            let col = Object::Column(table.clone(), column.clone());
            let uses = |o: &Object| cd.alters.contains(o) || cd.requires.contains(o);
            let constraint = matches!(c, Change::AddConstraint { .. } | Change::AlterConstraint { .. } | Change::CreateIndex { .. });
            if (!matches!(c, Change::DropConstraint { .. }) && uses(&column_type)) || (constraint && uses(&col)) {
                return true;
            }
        }
        // An enum value not added (or not renamed): what names it as a literal waits with it.
        Change::AddEnumValue { name, value: label, .. } | Change::RenameEnumValue { name, to: label, .. }
            if statement.is_some_and(|st| mentions_label(st, name, label)) =>
        {
            return true;
        }
        _ => {}
    }
    let (Some(at), Some(sat)) = (column_of(c), column_of(s)) else {
        return primary_key_columns(s).is_some_and(|(t, cols)| match c {
            Change::DropNotNull { table, column } => table == t && cols.contains(column),
            _ => false,
        });
    };
    if at != sat {
        return false;
    }
    use Change::*;
    match (s, c) {
        // A type change converting USING, with the default dropped before and set after.
        (AlterColumnType { using: Some(_), .. }, DropDefault { .. } | SetDefault { .. }) => true,
        (DropDefault { .. } | SetDefault { .. }, AlterColumnType { using: Some(_), .. }) => true,
        (DropDefault { .. } | SetNotNull { .. }, AddIdentity { .. }) => true,
        (DropIdentity { .. }, SetDefault { .. } | DropNotNull { .. }) => true,
        _ => false,
    }
}

/// Whether `statement` holds `label` as a literal of enum `name`, the way the server prints it
/// (`'label'::name` or `'label'::schema.name`). Conservative: a statement can match without
/// needing the value (the text inside a comment or a string), and it is then skipped too; an
/// array literal (`'{label}'::name[]`) does not match.
fn mentions_label(statement: &str, name: &QualName, label: &str) -> bool {
    let literal = format!("{}::", quote_literal(label));
    let bare = quote_ident(&name.name);
    let qualified = format!("{}.{bare}", quote_ident(&name.schema));
    statement.match_indices(&literal).any(|(i, _)| {
        let rest = &statement[i + literal.len()..];
        [&bare, &qualified].iter().any(|n| {
            rest.strip_prefix(n.as_str()).is_some_and(|after| !after.starts_with(|ch: char| ch.is_alphanumeric() || ch == '_' || ch == '$'))
        })
    })
}

/// The column a column change is about.
fn column_of(c: &Change) -> Option<(&QualName, &str)> {
    match c {
        Change::AlterColumnType { table, column, .. }
        | Change::SetNotNull { table, column }
        | Change::DropNotNull { table, column }
        | Change::SetDefault { table, column, .. }
        | Change::DropDefault { table, column, .. }
        | Change::AddIdentity { table, column, .. }
        | Change::AlterIdentity { table, column, .. }
        | Change::DropIdentity { table, column, .. } => Some((table, column)),
        _ => None,
    }
}

/// The table and columns of a dropped primary key.
fn primary_key_columns(c: &Change) -> Option<(&QualName, &[String])> {
    match c {
        Change::DropConstraint { table, constraint } if constraint.kind == ConstraintKind::PrimaryKey => Some((table, &constraint.columns)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgmig_model::{Column, Constraint, Table};

    fn col(name: &str) -> Column {
        Column {
            name: name.into(),
            type_name: "integer".into(),
            type_ref: None,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: Vec::new(),
            identity: None,
        }
    }

    fn table(schema: &str, name: &str) -> Table {
        Table { name: QualName::new(schema, name), columns: vec![col("id")], comment: None, constraints: vec![], indexes: vec![] }
    }

    fn kinds(k: &[ChangeKind]) -> BTreeSet<ChangeKind> {
        k.iter().copied().collect()
    }

    #[test]
    fn nothing_skipped_without_kinds() {
        let changes = vec![Change::DropSchema("app".into())];
        assert_eq!(skip(&changes, &BTreeSet::new(), &PlanOptions::default()), (changes, vec![]));
    }

    #[test]
    fn a_skipped_table_keeps_its_schema() {
        let changes = vec![
            Change::DropSchema("app".into()),
            Change::DropTable { table: table("app", "t"), owned_sequences: vec![] },
            Change::DropTable { table: table("public", "u"), owned_sequences: vec![] },
        ];
        let (kept, skipped) = skip(&changes, &kinds(&[ChangeKind::DropTable]), &PlanOptions::default());
        assert!(kept.is_empty(), "{kept:?}");
        let shown: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            ["drop_schema app (because drop_table app.t is skipped)", "drop_table app.t (policy)", "drop_table public.u (policy)"]
        );
        assert_eq!(skipped[1].statement.as_deref(), Some("DROP TABLE app.t"));
    }

    #[test]
    fn a_constraint_redefined_is_skipped_with_its_drop() {
        let t = QualName::new("public", "t");
        let check = |def: &str| Constraint {
            name: "c_check".into(),
            kind: ConstraintKind::Check,
            definition: def.into(),
            columns: vec!["id".into()],
            references: None,
            deferrable: false,
            deferred: false,
            validated: true,
            comment: None,
            index_definition: None,
            on_partitioned_table: false,
        };
        let changes = vec![
            Change::DropConstraint { table: t.clone(), constraint: check("CHECK (id > 0)") },
            Change::AddConstraint { table: t.clone(), constraint: check("CHECK (id > 1)") },
        ];
        let (kept, skipped) = skip(&changes, &kinds(&[ChangeKind::DropConstraint]), &PlanOptions::default());
        assert!(kept.is_empty());
        assert_eq!(skipped[1].because, Some(Cause { kind: ChangeKind::DropConstraint, object: "public.t.c_check".into() }));
    }

    #[test]
    fn a_type_change_using_a_cast_goes_with_its_default() {
        let t = QualName::new("public", "t");
        let changes = vec![
            Change::DropDefault { table: t.clone(), column: "c".into(), sequences: vec![] },
            Change::AlterColumnType {
                table: t.clone(),
                column: "c".into(),
                type_name: "integer".into(),
                type_ref: None,
                old_type_ref: None,
                using: Some("c::integer".into()),
            },
            Change::SetDefault { table: t.clone(), column: "c".into(), default: "1".into(), sequences: vec![], old_sequences: vec![] },
            Change::DropNotNull { table: t, column: "d".into() },
        ];
        let (kept, skipped) = skip(&changes, &kinds(&[ChangeKind::DropDefault]), &PlanOptions::default());
        assert_eq!(kept, changes[3..]);
        assert_eq!(skipped.len(), 3);
    }

    #[test]
    fn an_enum_literal_is_found_by_its_type() {
        let mood = QualName::new("public", "mood");
        assert!(mentions_label("ALTER TABLE t ALTER COLUMN m SET DEFAULT 'b'::mood", &mood, "b"));
        assert!(mentions_label("CHECK (m <> 'b'::public.mood)", &mood, "b"));
        assert!(mentions_label("DEFAULT 'it''s'::mood", &mood, "it's"));
        assert!(!mentions_label("DEFAULT 'b'::moods", &mood, "b"));
        assert!(!mentions_label("DEFAULT 'b'::text", &mood, "b"));
        assert!(!mentions_label("DEFAULT 'ab'::mood", &mood, "b"));
    }

    #[test]
    fn a_skipped_drop_sequence_keeps_the_column_owning_one() {
        let t = QualName::new("public", "t");
        let seq = QualName::new("public", "t_n_seq");
        let changes = vec![
            Change::DropColumn {
                table: t.clone(),
                column: "n".into(),
                type_ref: None,
                sequences: vec![seq.clone()],
                owned_sequences: vec![seq.clone()],
            },
            Change::DropIdentity { table: t.clone(), column: "id".into(), sequence: QualName::new("public", "t_id_seq") },
            Change::DropNotNull { table: t.clone(), column: "id".into() },
            Change::DropColumn { table: t, column: "x".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
        ];
        let (kept, skipped) = skip(&changes, &kinds(&[ChangeKind::DropSequence]), &PlanOptions::default());
        assert_eq!(kept, changes[3..]);
        let shown: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "drop_column public.t.n (because drop_sequence public.t_n_seq is skipped)",
                "drop_identity public.t.id (because drop_sequence public.t_id_seq is skipped)",
                "drop_not_null public.t.id (because drop_identity public.t.id is skipped)",
            ]
        );
    }
    #[test]
    fn a_skipped_drop_index_keeps_its_replacement_and_the_columns_it_uses() {
        let t = QualName::new("public", "t");
        let index = |definition: &str| pgmig_model::Index {
            name: "t_idx".into(),
            definition: definition.into(),
            unique: false,
            columns: vec!["x".into()],
            comment: None,
            on_partitioned_table: false,
        };
        let changes = vec![
            Change::DropIndex { table: t.clone(), index: index("CREATE INDEX t_idx ON public.t USING btree (x)") },
            Change::CreateIndex { table: t.clone(), index: index("CREATE INDEX t_idx ON public.t USING hash (x)") },
            Change::DropColumn { table: t.clone(), column: "x".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
            Change::DropColumn { table: t, column: "y".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
        ];
        let (kept, skipped) = skip(&changes, &kinds(&[ChangeKind::DropIndex]), &PlanOptions::default());
        assert_eq!(kept, changes[3..]);
        let shown: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "drop_index public.t_idx (policy)",
                "create_index public.t_idx (because drop_index public.t_idx is skipped)",
                "drop_column public.t.x (because drop_index public.t_idx is skipped)",
            ]
        );
        assert_eq!(skipped[0].statement.as_deref(), Some("DROP INDEX public.t_idx"));
    }

    #[test]
    fn a_foreign_key_around_a_replaced_index_is_kept_with_it() {
        let p = QualName::new("public", "p");
        let c = QualName::new("public", "c");
        let index = |definition: &str| pgmig_model::Index {
            name: "p_id_idx".into(),
            definition: definition.into(),
            unique: true,
            columns: vec!["id".into()],
            comment: None,
            on_partitioned_table: false,
        };
        let fkey = Constraint {
            name: "c_pid_fkey".into(),
            kind: ConstraintKind::ForeignKey,
            definition: "FOREIGN KEY (pid) REFERENCES public.p(id)".into(),
            columns: vec!["pid".into()],
            references: Some(pgmig_model::References { table: p.clone(), columns: vec!["id".into()], constraint: Some("p_id_idx".into()) }),
            deferrable: false,
            deferred: false,
            validated: true,
            comment: None,
            index_definition: None,
            on_partitioned_table: false,
        };
        let changes = vec![
            Change::DropConstraint { table: c.clone(), constraint: fkey.clone() },
            Change::DropIndex { table: p.clone(), index: index("CREATE UNIQUE INDEX p_id_idx ON public.p USING btree (id)") },
            Change::CreateIndex { table: p, index: index("CREATE UNIQUE INDEX p_id_idx ON public.p USING btree (id) NULLS NOT DISTINCT") },
            Change::AddConstraint { table: c, constraint: fkey },
        ];
        for kind in [ChangeKind::DropIndex, ChangeKind::CreateIndex, ChangeKind::DropConstraint, ChangeKind::AddConstraint] {
            let (kept, skipped) = skip(&changes, &kinds(&[kind]), &PlanOptions::default());
            assert!(kept.is_empty(), "{kind}: {kept:?}");
            assert_eq!(skipped.len(), 4, "{kind}");
            assert!(skipped.iter().filter(|s| s.because.is_none()).all(|s| s.kind == kind), "{kind}: {skipped:?}");
        }
    }
}
