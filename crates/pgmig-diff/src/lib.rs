//! `diff(a, b)`: the changes that turn model `a` into model `b`.
//!
//! # Enum values
//!
//! The catalog cannot say whether a value was renamed or dropped and another added, so the diff
//! follows one conservative rule. Comparing an enum's values in sort order:
//!
//! - the old values appear, in order, among the new ones: each new value is added at its position
//!   ([`Change::AddEnumValue`], left to right, `BEFORE` the first old value for a new first value
//!   and `AFTER` its predecessor otherwise);
//! - the same number of values, and exactly one position holds a different value that is not among
//!   the old ones: that value is renamed ([`Change::RenameEnumValue`]);
//! - anything else (a value removed, values reordered, a rename together with an addition) is
//!   [`Change::UnsupportedEnumChange`], which the planner refuses: Postgres cannot remove or
//!   reorder enum values in place.
//!
//! # Sequences
//!
//! Sequences are compared by type, options and `OWNED BY`, never by current value. A sequence
//! owned by a column that is dropped (or whose table is dropped) goes with it, so no
//! [`Change::DropSequence`] is emitted for it; the column or table drop lists it instead
//! (`owned_sequences`), so the planner runs what still uses it first. A `serial` column is its default plus its owned
//! sequence, both compared as they are stored. Identity columns compare `GENERATED ALWAYS` / `BY
//! DEFAULT` and their sequence's options ([`Change::AddIdentity`], [`Change::AlterIdentity`],
//! [`Change::DropIdentity`]).
//!
//! # Constraints
//!
//! Constraints are compared by kind and printed definition (`pg_get_constraintdef`) and matched
//! by name within their table. A changed constraint is dropped and added again, with three
//! exceptions:
//!
//! - a foreign key whose definition differs only in `DEFERRABLE` / `INITIALLY DEFERRED` is
//!   altered in place ([`Change::AlterConstraint`]);
//! - when a table loses exactly one constraint and gains exactly one, of the same kind and with the
//!   same definition, the constraint is renamed ([`Change::RenameConstraint`]). Any other mix of
//!   names is a drop and an add;
//! - a foreign key that stays the same but references a primary key, unique or exclusion
//!   constraint that is dropped (or dropped and added again) is dropped first and added again
//!   after: Postgres refuses to drop a constraint a foreign key uses;
//! - a foreign key that stays the same but has a column, or a referenced column, that changes type
//!   is dropped before the type change and added again after: Postgres cannot rebuild it.
//!
//! A new table carries its primary key, unique, check and exclusion constraints in
//! [`Change::CreateTable`]; its foreign keys (which may reference tables not created yet, or each
//! other) and its `NOT VALID` constraints (which `CREATE TABLE` would validate) follow as
//! [`Change::AddConstraint`]. A dropped table's constraints go with it: [`Change::DropTable`]
//! lists them, except foreign keys to other dropped tables that reference it back (directly or
//! through others), which are dropped first ([`Change::DropConstraint`]) so the drops don't
//! depend on each other in a cycle.
//!
//! # Type changes
//!
//! [`diff`] knows types only by their printed names, so it cannot tell whether Postgres converts
//! a column's values to the new type on its own (an assignment cast, as `integer` to `bigint`) or
//! needs `USING` (as `text` to `integer`). [`Change::AlterColumnType`] comes out of [`diff`] with
//! no `using`; [`add_casts`] takes the answer from the caller, who asks a server, and fills it in.
//! A type change that needs `USING` also drops the column's old default first and sets the new
//! one after (Postgres would have to convert the default too, and refuses when it cannot).
//!
//! # Serialization
//!
//! [`Change`], [`CommentTarget`] and [`EnumPosition`] are adjacently tagged: `{"kind": "<variant
//! in snake_case>", "data": <the variant's fields>}`, where `data` is an object for a variant with
//! named fields, the value for a variant with one (a newtype), and absent for one with none (`{"kind": "end"}`). Models' parts serialize as `pgmig_model` documents.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use pgmig_model::{Column, Constraint, ConstraintKind, Enum, Identity, Model, OwnedBy, QualName, Sequence, Table, quote_ident};

/// An object that can carry a comment. New kinds (constraints, indexes) add a variant here, a
/// matching `Object` in the planner, and a comparison in [`diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum CommentTarget {
    Schema(String),
    Table(QualName),
    Column {
        table: QualName,
        column: String,
    },
    /// A type that is not a table's row type (an enum).
    Type(QualName),
    Sequence(QualName),
    /// A table constraint by its table and name.
    Constraint {
        table: QualName,
        name: String,
    },
}

/// Where [`Change::AddEnumValue`] puts the new value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EnumPosition {
    Before(String),
    After(String),
    /// After the last value (only used when the enum has no values yet).
    End,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Change {
    CreateSchema(String),
    DropSchema(String),
    /// The table with the constraints created with it (see the module doc).
    CreateTable(Table),
    /// `table` is the table as it was, so the planner sees what its columns referenced, with the
    /// constraints dropped with it (not those dropped first by a [`Change::DropConstraint`]).
    /// `owned_sequences` are the sequences Postgres drops with it: those `OWNED BY` its columns
    /// that are not kept, and its identity columns' sequences.
    DropTable {
        table: Table,
        owned_sequences: Vec<QualName>,
    },
    AddColumn {
        table: QualName,
        column: Column,
    },
    /// `type_ref` and `sequences` (its default's) are the dropped column's; `owned_sequences` are
    /// the sequences Postgres drops with it (as for [`Change::DropTable`]).
    DropColumn {
        table: QualName,
        column: String,
        type_ref: Option<QualName>,
        sequences: Vec<QualName>,
        owned_sequences: Vec<QualName>,
    },
    /// `type_ref` is the new type's, `old_type_ref` the replaced one's. `using` is the `USING`
    /// expression, when the old type has no assignment cast to the new one (see [`add_casts`]).
    AlterColumnType {
        table: QualName,
        column: String,
        type_name: String,
        type_ref: Option<QualName>,
        old_type_ref: Option<QualName>,
        using: Option<String>,
    },
    SetNotNull {
        table: QualName,
        column: String,
    },
    DropNotNull {
        table: QualName,
        column: String,
    },
    /// `sequences` are the new default's ([`Column::default_sequences`]), `old_sequences` the
    /// replaced one's.
    SetDefault {
        table: QualName,
        column: String,
        default: String,
        sequences: Vec<QualName>,
        old_sequences: Vec<QualName>,
    },
    /// `sequences` are the dropped default's.
    DropDefault {
        table: QualName,
        column: String,
        sequences: Vec<QualName>,
    },
    /// `ADD GENERATED ... AS IDENTITY` on an existing column; `type_name` is the column's type
    /// once the other changes have run.
    AddIdentity {
        table: QualName,
        column: String,
        type_name: String,
        identity: Identity,
    },
    /// `SET GENERATED` and `SET <option>` for what changed. `type_changed` when the column's type
    /// changes too: Postgres may then reset the sequence's bounds, so both are set again.
    AlterIdentity {
        table: QualName,
        column: String,
        type_name: String,
        from: Identity,
        to: Identity,
        type_changed: bool,
    },
    /// `sequence` is the identity's sequence (dropped with it).
    DropIdentity {
        table: QualName,
        column: String,
        sequence: QualName,
    },
    /// Created without `OWNED BY`; a [`Change::SetSequenceOwner`] follows when it has one.
    CreateSequence(Sequence),
    DropSequence(Sequence),
    /// Type or options changed (`from` and `to` have the same name).
    AlterSequence {
        from: Sequence,
        to: Sequence,
    },
    /// `ALTER SEQUENCE ... OWNED BY`; `owned_by: None` is `OWNED BY NONE`.
    SetSequenceOwner {
        name: QualName,
        owned_by: Option<OwnedBy>,
        old: Option<OwnedBy>,
    },
    CreateEnum(Enum),
    DropEnum(Enum),
    AddEnumValue {
        name: QualName,
        value: String,
        position: EnumPosition,
    },
    RenameEnumValue {
        name: QualName,
        from: String,
        to: String,
    },
    /// The values changed in a way that cannot be planned (see the module doc); the planner
    /// reports it as an error.
    UnsupportedEnumChange {
        name: QualName,
        from: Vec<String>,
        to: Vec<String>,
    },
    /// `ALTER TABLE ... ADD CONSTRAINT`.
    AddConstraint {
        table: QualName,
        constraint: Constraint,
    },
    /// `ALTER TABLE ... DROP CONSTRAINT`; `constraint` is the constraint as it was.
    DropConstraint {
        table: QualName,
        constraint: Constraint,
    },
    /// `ALTER TABLE ... RENAME CONSTRAINT`; `to` is the renamed constraint.
    RenameConstraint {
        table: QualName,
        from: String,
        to: Constraint,
    },
    /// `ALTER TABLE ... ALTER CONSTRAINT` to the deferrability of `to` (a foreign key).
    AlterConstraint {
        table: QualName,
        to: Constraint,
    },
    /// `COMMENT ON ... IS <comment>`; `None` removes the comment. Never emitted for an object
    /// that is being dropped.
    SetComment {
        target: CommentTarget,
        comment: Option<String>,
    },
}

/// Fills in `using` on the [`Change::AlterColumnType`]s of `diff(a, b)` whose old type has no
/// assignment cast to the new one. `assignable(from, to)` answers that for two printed type names
/// (`from` is `a`'s, `to` is `b`'s); the caller asks a server, and answers `false` when it cannot
/// tell, since `USING` is valid for any change an explicit cast can make.
///
/// Such a change converts with `USING column::type`, and the column's default is dropped before
/// it and set after (adding the [`Change::DropDefault`] and [`Change::SetDefault`] the diff left
/// out because the default did not change).
pub fn add_casts(mut changes: Vec<Change>, a: &Model, b: &Model, assignable: &mut dyn FnMut(&str, &str) -> bool) -> Vec<Change> {
    let column = |m: &Model, table: &QualName, name: &str| m.tables.get(table).and_then(|t| t.column(name)).cloned();
    let mut extra = Vec::new();
    for change in &mut changes {
        let Change::AlterColumnType { table, column: name, type_name, using, .. } = change else { continue };
        let (Some(old), Some(new)) = (column(a, table, name), column(b, table, name)) else { continue };
        if assignable(&old.type_name, type_name) {
            continue;
        }
        *using = Some(format!("{}::{type_name}", quote_ident(name)));
        if old.default.is_some() {
            extra.push((table.clone(), new, old.default_sequences));
        }
    }
    for (table, new, old_sequences) in extra {
        let here = |t: &QualName, c: &str| *t == table && c == new.name;
        if !changes.iter().any(|c| matches!(c, Change::DropDefault { table: t, column: c, .. } if here(t, c))) {
            changes.push(Change::DropDefault { table: table.clone(), column: new.name.clone(), sequences: old_sequences.clone() });
        }
        if let Some(default) = &new.default
            && !changes.iter().any(|c| matches!(c, Change::SetDefault { table: t, column: c, .. } if here(t, c)))
        {
            changes.push(Change::SetDefault {
                table: table.clone(),
                column: new.name.clone(),
                default: default.clone(),
                sequences: new.default_sequences.clone(),
                // Released by the drop above.
                old_sequences: Vec::new(),
            });
        }
    }
    changes
}

pub fn diff(a: &Model, b: &Model) -> Vec<Change> {
    let mut changes = Vec::new();

    for name in b.schemas.keys().filter(|n| !a.schemas.contains_key(*n)) {
        changes.push(Change::CreateSchema(name.clone()));
    }
    for (name, schema) in &b.schemas {
        let old = a.schemas.get(name).and_then(|s| s.comment.as_ref());
        set_comment(CommentTarget::Schema(name.clone()), old, schema.comment.as_ref(), &mut changes);
    }
    for name in a.schemas.keys().filter(|n| !b.schemas.contains_key(*n)) {
        changes.push(Change::DropSchema(name.clone()));
    }

    for (name, e) in &b.enums {
        let old = a.enums.get(name);
        match old {
            None => changes.push(Change::CreateEnum(e.clone())),
            Some(old) => diff_enum_values(old, e, &mut changes),
        }
        set_comment(CommentTarget::Type(name.clone()), old.and_then(|o| o.comment.as_ref()), e.comment.as_ref(), &mut changes);
    }
    for e in a.enums.values().filter(|e| !b.enums.contains_key(&e.name)) {
        changes.push(Change::DropEnum(e.clone()));
    }

    for (name, seq) in &b.sequences {
        let old = a.sequences.get(name);
        match old {
            None => {
                changes.push(Change::CreateSequence(seq.clone()));
                if seq.owned_by.is_some() {
                    changes.push(Change::SetSequenceOwner { name: name.clone(), owned_by: seq.owned_by.clone(), old: None });
                }
            }
            Some(old) => {
                if old.data_type != seq.data_type || old.options != seq.options {
                    changes.push(Change::AlterSequence { from: old.clone(), to: seq.clone() });
                }
                if old.owned_by != seq.owned_by {
                    changes.push(Change::SetSequenceOwner {
                        name: name.clone(),
                        owned_by: seq.owned_by.clone(),
                        old: old.owned_by.clone(),
                    });
                }
            }
        }
        set_comment(CommentTarget::Sequence(name.clone()), old.and_then(|o| o.comment.as_ref()), seq.comment.as_ref(), &mut changes);
    }
    // Dropping the owning column (or its table) drops the sequence: that drop carries it instead.
    let implicit = implicit_sequence_drops(a, b);
    for seq in a.sequences.values().filter(|s| !b.sequences.contains_key(&s.name)) {
        if !implicit.values().any(|names| names.contains(&seq.name)) {
            changes.push(Change::DropSequence(seq.clone()));
        }
    }
    let owned = |table: &QualName, column: Option<&str>| -> Vec<QualName> {
        implicit.iter().filter(|((t, c), _)| t == table && column.is_none_or(|col| col == c)).flat_map(|(_, n)| n.clone()).collect()
    };

    let dropped_first = foreign_keys_dropped_before_their_tables(a, b);
    for (name, table) in &b.tables {
        match a.tables.get(name) {
            None => {
                let mut created = table.clone();
                created.constraints.retain(created_with_table);
                changes.push(Change::CreateTable(created));
                set_comment(CommentTarget::Table(name.clone()), None, table.comment.as_ref(), &mut changes);
                for col in &table.columns {
                    set_comment(
                        CommentTarget::Column { table: name.clone(), column: col.name.clone() },
                        None,
                        col.comment.as_ref(),
                        &mut changes,
                    );
                }
            }
            Some(old) => diff_table(old, table, &|c| owned(name, Some(c)), &mut changes),
        }
    }
    for table in a.tables.values().filter(|t| !b.tables.contains_key(&t.name)) {
        for fk in table.constraints.iter().filter(|c| dropped_first.contains(&(table.name.clone(), c.name.clone()))) {
            changes.push(Change::DropConstraint { table: table.name.clone(), constraint: fk.clone() });
        }
        let mut dropped = table.clone();
        dropped.constraints.retain(|c| !dropped_first.contains(&(table.name.clone(), c.name.clone())));
        changes.push(Change::DropTable { table: dropped, owned_sequences: owned(&table.name, None) });
    }

    diff_constraints(a, b, &mut changes);
    changes
}

/// Whether a new table's constraint is written in its `CREATE TABLE` (see the module doc).
fn created_with_table(c: &Constraint) -> bool {
    c.kind != ConstraintKind::ForeignKey && c.validated
}

/// The foreign keys `(table, name)` of tables `b` drops that reference another dropped table
/// which references the first one back, directly or through other dropped tables.
fn foreign_keys_dropped_before_their_tables(a: &Model, b: &Model) -> BTreeSet<(QualName, String)> {
    let dropped: Vec<&Table> = a.tables.values().filter(|t| !b.tables.contains_key(&t.name)).collect();
    let is_dropped = |n: &QualName| dropped.iter().any(|t| &t.name == n);
    let refs = |t: &Table| -> Vec<QualName> {
        t.constraints.iter().filter_map(|c| c.references.as_ref()).map(|r| r.table.clone()).filter(|r| is_dropped(r)).collect()
    };
    let reaches = |from: &QualName, to: &QualName| {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from.clone()];
        while let Some(at) = stack.pop() {
            if &at == to {
                return true;
            }
            if seen.insert(at.clone())
                && let Some(t) = a.tables.get(&at)
            {
                stack.extend(refs(t));
            }
        }
        false
    };
    let mut out = BTreeSet::new();
    for t in &dropped {
        for c in &t.constraints {
            if let Some(r) = &c.references
                && r.table != t.name
                && is_dropped(&r.table)
                && reaches(&r.table, &t.name)
            {
                out.insert((t.name.clone(), c.name.clone()));
            }
        }
    }
    out
}

/// What becomes of one constraint of a table both models have.
enum Step<'a> {
    Keep(&'a Constraint, &'a Constraint),
    Alter(&'a Constraint, &'a Constraint),
    Replace(&'a Constraint, &'a Constraint),
    Rename(&'a Constraint, &'a Constraint),
    Drop(&'a Constraint),
    Add(&'a Constraint),
}

fn constraint_steps<'a>(a: &'a Table, b: &'a Table) -> Vec<Step<'a>> {
    let mut steps = Vec::new();
    let mut added = Vec::new();
    for c in &b.constraints {
        match a.constraint(&c.name) {
            Some(o) if o.kind == c.kind && o.definition == c.definition => steps.push(Step::Keep(o, c)),
            Some(o)
                if o.kind == ConstraintKind::ForeignKey
                    && c.kind == ConstraintKind::ForeignKey
                    && o.definition_without_deferrability() == c.definition_without_deferrability() =>
            {
                steps.push(Step::Alter(o, c))
            }
            Some(o) => steps.push(Step::Replace(o, c)),
            None => added.push(c),
        }
    }
    let gone: Vec<&Constraint> = a.constraints.iter().filter(|o| b.constraint(&o.name).is_none()).collect();
    match (&gone[..], &added[..]) {
        ([o], [c]) if o.kind == c.kind && o.definition == c.definition => steps.push(Step::Rename(o, c)),
        _ => {
            steps.extend(gone.into_iter().map(Step::Drop));
            steps.extend(added.into_iter().map(Step::Add));
        }
    }
    steps
}

/// Constraint changes: the constraints of new tables not created with them, and those of tables
/// both models have (see the module doc).
fn diff_constraints(a: &Model, b: &Model, changes: &mut Vec<Change>) {
    let target = |t: &QualName, c: &Constraint| CommentTarget::Constraint { table: t.clone(), name: c.name.clone() };
    let kept: Vec<(&Table, &Table)> = b.tables.values().filter_map(|t| Some((a.tables.get(&t.name)?, t))).collect();
    let steps: Vec<(&QualName, Vec<Step>)> = kept.iter().map(|(old, new)| (&new.name, constraint_steps(old, new))).collect();
    // Constraints of `a` that stop existing under their name: a foreign key using one must go too.
    let mut removed = BTreeSet::new();
    for (table, steps) in &steps {
        for step in steps {
            if let Step::Replace(o, _) | Step::Drop(o) = step {
                removed.insert(((*table).clone(), o.name.clone()));
            }
        }
    }
    let uses_removed = |o: &Constraint| {
        o.references.as_ref().is_some_and(|r| r.constraint.as_ref().is_some_and(|c| removed.contains(&(r.table.clone(), c.clone()))))
    };
    // Columns whose type changes: Postgres cannot rebuild a foreign key over one (42804), so the
    // key is dropped before the type change and added after it.
    let retyped = |table: &QualName, column: &str| {
        let (Some(old), Some(new)) = (a.tables.get(table), b.tables.get(table)) else { return false };
        matches!((old.column(column), new.column(column)), (Some(o), Some(n)) if o.type_name != n.type_name)
    };
    let uses_retyped = |table: &QualName, o: &Constraint| {
        o.kind == ConstraintKind::ForeignKey
            && (o.columns.iter().any(|c| retyped(table, c))
                || o.references.as_ref().is_some_and(|r| r.columns.iter().any(|c| retyped(&r.table, c))))
    };

    for (name, table) in &b.tables {
        if a.tables.contains_key(name) {
            continue;
        }
        for c in &table.constraints {
            if !created_with_table(c) {
                changes.push(Change::AddConstraint { table: name.clone(), constraint: c.clone() });
            }
            set_comment(target(name, c), None, c.comment.as_ref(), changes);
        }
    }
    for (table, steps) in steps {
        for step in steps {
            let step = match step {
                Step::Keep(o, c) | Step::Alter(o, c) if uses_removed(o) || uses_retyped(table, o) => Step::Replace(o, c),
                other => other,
            };
            match step {
                Step::Keep(o, c) => set_comment(target(table, c), o.comment.as_ref(), c.comment.as_ref(), changes),
                Step::Alter(o, c) => {
                    changes.push(Change::AlterConstraint { table: table.clone(), to: c.clone() });
                    set_comment(target(table, c), o.comment.as_ref(), c.comment.as_ref(), changes);
                }
                Step::Rename(o, c) => {
                    changes.push(Change::RenameConstraint { table: table.clone(), from: o.name.clone(), to: c.clone() });
                    set_comment(target(table, c), o.comment.as_ref(), c.comment.as_ref(), changes);
                }
                Step::Replace(o, c) => {
                    changes.push(Change::DropConstraint { table: table.clone(), constraint: o.clone() });
                    changes.push(Change::AddConstraint { table: table.clone(), constraint: c.clone() });
                    set_comment(target(table, c), None, c.comment.as_ref(), changes);
                }
                Step::Drop(o) => changes.push(Change::DropConstraint { table: table.clone(), constraint: o.clone() }),
                Step::Add(c) => {
                    changes.push(Change::AddConstraint { table: table.clone(), constraint: c.clone() });
                    set_comment(target(table, c), None, c.comment.as_ref(), changes);
                }
            }
        }
    }
}

/// The sequences Postgres drops implicitly with each column of `a` that `b` drops (directly or
/// with its table): the standalone sequences `OWNED BY` it that `b` does not keep, and its
/// identity's sequence.
fn implicit_sequence_drops(a: &Model, b: &Model) -> BTreeMap<(QualName, String), Vec<QualName>> {
    let dropped = |table: &QualName, column: &str| b.tables.get(table).is_none_or(|t| t.column(column).is_none());
    let mut out: BTreeMap<(QualName, String), Vec<QualName>> = BTreeMap::new();
    for seq in a.sequences.values().filter(|s| !b.sequences.contains_key(&s.name)) {
        if let Some(o) = &seq.owned_by
            && dropped(&o.table, &o.column)
        {
            out.entry((o.table.clone(), o.column.clone())).or_default().push(seq.name.clone());
        }
    }
    for t in a.tables.values() {
        for c in t.columns.iter().filter(|c| dropped(&t.name, &c.name)) {
            if let Some(identity) = &c.identity {
                out.entry((t.name.clone(), c.name.clone())).or_default().push(identity.sequence_name(&t.name, &c.name));
            }
        }
    }
    out
}

/// Emit a `SetComment` when the comment differs (`old` is `None` for a new object).
fn set_comment(target: CommentTarget, old: Option<&String>, new: Option<&String>, changes: &mut Vec<Change>) {
    if old != new {
        changes.push(Change::SetComment { target, comment: new.cloned() });
    }
}

fn diff_enum_values(a: &Enum, b: &Enum, changes: &mut Vec<Change>) {
    let (old, new) = (&a.labels, &b.labels);
    if old == new {
        return;
    }
    let name = || b.name.clone();
    let kept: Vec<&String> = new.iter().filter(|l| old.contains(l)).collect();
    if kept.len() == old.len() && kept.iter().zip(old).all(|(k, o)| *k == o) {
        for (i, value) in new.iter().enumerate().filter(|(_, l)| !old.contains(l)) {
            let position = match (i, old.first()) {
                (0, Some(first)) => EnumPosition::Before(first.clone()),
                (0, None) => EnumPosition::End,
                _ => EnumPosition::After(new[i - 1].clone()),
            };
            changes.push(Change::AddEnumValue { name: name(), value: value.clone(), position });
        }
        return;
    }
    let differing: Vec<usize> = (0..old.len().min(new.len())).filter(|&i| old[i] != new[i]).collect();
    if old.len() == new.len()
        && let [i] = differing[..]
        && !old.contains(&new[i])
    {
        changes.push(Change::RenameEnumValue { name: name(), from: old[i].clone(), to: new[i].clone() });
        return;
    }
    changes.push(Change::UnsupportedEnumChange { name: name(), from: old.clone(), to: new.clone() });
}

/// `owned` gives the sequences dropped with a column of `a` (see [`implicit_sequence_drops`]).
fn diff_table(a: &Table, b: &Table, owned: &dyn Fn(&str) -> Vec<QualName>, changes: &mut Vec<Change>) {
    let table = &b.name;
    set_comment(CommentTarget::Table(table.clone()), a.comment.as_ref(), b.comment.as_ref(), changes);
    for col in &b.columns {
        let Some(old) = a.column(&col.name) else {
            changes.push(Change::AddColumn { table: table.clone(), column: col.clone() });
            set_comment(CommentTarget::Column { table: table.clone(), column: col.name.clone() }, None, col.comment.as_ref(), changes);
            continue;
        };
        let column = || col.name.clone();
        if old.type_name != col.type_name {
            changes.push(Change::AlterColumnType {
                table: table.clone(),
                column: column(),
                type_name: col.type_name.clone(),
                type_ref: col.type_ref.clone(),
                old_type_ref: old.type_ref.clone(),
                using: None,
            });
        }
        match (&old.default, &col.default) {
            (_, Some(new)) if old.default.as_ref() != Some(new) => {
                changes.push(Change::SetDefault {
                    table: table.clone(),
                    column: column(),
                    default: new.clone(),
                    sequences: col.default_sequences.clone(),
                    old_sequences: old.default_sequences.clone(),
                });
            }
            (Some(_), None) => {
                changes.push(Change::DropDefault { table: table.clone(), column: column(), sequences: old.default_sequences.clone() })
            }
            _ => {}
        }
        match (&old.identity, &col.identity) {
            (None, Some(new)) => changes.push(Change::AddIdentity {
                table: table.clone(),
                column: column(),
                type_name: col.type_name.clone(),
                identity: new.clone(),
            }),
            (Some(from), None) => changes.push(Change::DropIdentity {
                table: table.clone(),
                column: column(),
                sequence: from.sequence_name(table, &col.name),
            }),
            (Some(from), Some(to)) if from != to || old.type_name != col.type_name => changes.push(Change::AlterIdentity {
                table: table.clone(),
                column: column(),
                type_name: col.type_name.clone(),
                from: from.clone(),
                to: to.clone(),
                type_changed: old.type_name != col.type_name,
            }),
            _ => {}
        }
        set_comment(CommentTarget::Column { table: table.clone(), column: column() }, old.comment.as_ref(), col.comment.as_ref(), changes);
        match (old.not_null, col.not_null) {
            (false, true) => changes.push(Change::SetNotNull { table: table.clone(), column: column() }),
            (true, false) => changes.push(Change::DropNotNull { table: table.clone(), column: column() }),
            _ => {}
        }
    }
    for old in a.columns.iter().filter(|c| b.column(&c.name).is_none()) {
        changes.push(Change::DropColumn {
            table: table.clone(),
            column: old.name.clone(),
            type_ref: old.type_ref.clone(),
            sequences: old.default_sequences.clone(),
            owned_sequences: owned(&old.name),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, type_name: &str) -> Column {
        Column {
            name: name.into(),
            type_name: type_name.into(),
            type_ref: None,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: Vec::new(),
            identity: None,
        }
    }

    fn model(columns: Vec<Column>) -> Model {
        let mut m = Model::empty_database();
        let name = QualName::new("public", "t");
        m.tables.insert(name.clone(), Table { name, columns, comment: None, constraints: vec![] });
        m
    }

    #[test]
    fn type_changes_without_an_assignment_cast_convert_using_one_and_move_the_default() {
        let with_default = |name: &str, type_name: &str, default: &str| Column { default: Some(default.into()), ..col(name, type_name) };
        let a = model(vec![with_default("n", "text", "'1'::text"), with_default("m", "text", "f()"), col("k", "bigint")]);
        let b = model(vec![with_default("n", "integer", "1"), with_default("m", "integer", "f()"), col("k", "integer")]);
        let mut asked = Vec::new();
        let changes = add_casts(diff(&a, &b), &a, &b, &mut |from, to| {
            asked.push(format!("{from} -> {to}"));
            from == "bigint"
        });
        assert_eq!(asked, ["text -> integer", "text -> integer", "bigint -> integer"]);
        let t = QualName::new("public", "t");
        let alter = |column: &str, using: Option<&str>| Change::AlterColumnType {
            table: t.clone(),
            column: column.into(),
            type_name: "integer".into(),
            type_ref: None,
            old_type_ref: None,
            using: using.map(Into::into),
        };
        let set = |column: &str, default: &str| Change::SetDefault {
            table: t.clone(),
            column: column.into(),
            default: default.into(),
            sequences: vec![],
            old_sequences: vec![],
        };
        let drop = |column: &str| Change::DropDefault { table: t.clone(), column: column.into(), sequences: vec![] };
        assert_eq!(
            changes,
            [
                alter("n", Some("n::integer")),
                set("n", "1"),
                alter("m", Some("m::integer")),
                alter("k", None),
                drop("n"),
                // Unchanged, so not in the diff; set again after the type change.
                drop("m"),
                set("m", "f()"),
            ]
        );
    }

    #[test]
    fn identical_models_have_no_changes() {
        let m = model(vec![col("id", "integer")]);
        assert!(diff(&m, &m).is_empty());
    }

    #[test]
    fn column_order_is_ignored() {
        let a = model(vec![col("a", "integer"), col("b", "text")]);
        let b = model(vec![col("b", "text"), col("a", "integer")]);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn column_changes() {
        let a = model(vec![col("id", "integer"), col("gone", "text")]);
        let mut id = col("id", "bigint");
        id.not_null = true;
        let b = model(vec![id, col("new", "text")]);
        let t = QualName::new("public", "t");
        assert_eq!(
            diff(&a, &b),
            vec![
                Change::AlterColumnType {
                    table: t.clone(),
                    column: "id".into(),
                    type_name: "bigint".into(),
                    type_ref: None,
                    old_type_ref: None,
                    using: None
                },
                Change::SetNotNull { table: t.clone(), column: "id".into() },
                Change::AddColumn { table: t.clone(), column: col("new", "text") },
                Change::DropColumn { table: t, column: "gone".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
            ]
        );
    }

    fn commented(comment: Option<&str>) -> Model {
        let mut m = Model::empty_database();
        m.schemas.insert("app".into(), pgmig_model::Schema { name: "app".into(), comment: comment.map(Into::into) });
        let mut c = col("id", "integer");
        c.comment = comment.map(Into::into);
        let name = QualName::new("public", "t");
        m.tables.insert(name.clone(), Table { name, columns: vec![c], comment: comment.map(Into::into), constraints: vec![] });
        m
    }

    fn set(target: CommentTarget, comment: Option<&str>) -> Change {
        Change::SetComment { target, comment: comment.map(Into::into) }
    }

    fn targets() -> [CommentTarget; 3] {
        let t = QualName::new("public", "t");
        [CommentTarget::Schema("app".into()), CommentTarget::Table(t.clone()), CommentTarget::Column { table: t, column: "id".into() }]
    }

    #[test]
    fn comments_are_set_changed_and_removed() {
        for (from, to) in [(None, Some("a")), (Some("a"), Some("b")), (Some("a"), None)] {
            let expected: Vec<Change> =
                [targets()[0].clone(), targets()[1].clone(), targets()[2].clone()].into_iter().map(|t| set(t, to)).collect();
            assert_eq!(diff(&commented(from), &commented(to)), expected, "{from:?} -> {to:?}");
        }
        let m = commented(Some("same"));
        assert!(diff(&m, &m).is_empty());
    }

    #[test]
    fn new_objects_carry_their_comments_and_dropped_ones_need_none() {
        let mut with = commented(Some("c"));
        with.schemas.remove("public");
        let base = Model::default();
        let created = diff(&base, &with);
        assert!(created.iter().any(|c| matches!(c, Change::CreateTable(_))));
        for t in targets() {
            assert!(created.contains(&set(t, Some("c"))));
        }
        let dropped = diff(&with, &base);
        assert!(!dropped.iter().any(|c| matches!(c, Change::SetComment { .. })), "{dropped:?}");
    }

    #[test]
    fn a_comment_added_with_a_new_column_follows_the_column() {
        let a = model(vec![col("id", "integer")]);
        let mut n = col("note", "text");
        n.comment = Some("hi".into());
        let b = model(vec![col("id", "integer"), n.clone()]);
        let t = QualName::new("public", "t");
        assert_eq!(
            diff(&a, &b),
            vec![
                Change::AddColumn { table: t.clone(), column: n },
                set(CommentTarget::Column { table: t, column: "note".into() }, Some("hi"))
            ]
        );
    }

    #[test]
    fn the_public_schemas_default_comment_is_not_a_difference() {
        assert!(diff(&Model::empty_database(), &Model::empty_database()).is_empty());
    }

    fn enums(labels: &[&str]) -> Model {
        let mut m = Model::empty_database();
        let name = QualName::new("public", "mood");
        m.enums.insert(name.clone(), Enum { name, labels: labels.iter().map(|l| l.to_string()).collect(), comment: None });
        m
    }

    fn mood() -> QualName {
        QualName::new("public", "mood")
    }

    fn add(value: &str, position: EnumPosition) -> Change {
        Change::AddEnumValue { name: mood(), value: value.into(), position }
    }

    #[test]
    fn enums_are_created_and_dropped() {
        let m = enums(&["a"]);
        let e = m.enums[&mood()].clone();
        assert_eq!(diff(&Model::empty_database(), &m), vec![Change::CreateEnum(e.clone())]);
        assert_eq!(diff(&m, &Model::empty_database()), vec![Change::DropEnum(e)]);
        assert!(diff(&m, &m).is_empty());
    }

    #[test]
    fn new_values_are_added_at_their_positions_left_to_right() {
        use EnumPosition::{After, Before, End};
        assert_eq!(
            diff(&enums(&["b", "d"]), &enums(&["a0", "a1", "b", "c", "d", "e"])),
            vec![add("a0", Before("b".into())), add("a1", After("a0".into())), add("c", After("b".into())), add("e", After("d".into())),]
        );
        assert_eq!(diff(&enums(&[]), &enums(&["x", "y"])), vec![add("x", End), add("y", After("x".into()))]);
    }

    #[test]
    fn one_value_replaced_in_place_is_a_rename() {
        assert_eq!(
            diff(&enums(&["a", "b", "c"]), &enums(&["a", "x", "c"])),
            vec![Change::RenameEnumValue { name: mood(), from: "b".into(), to: "x".into() }]
        );
    }

    #[test]
    fn removals_reorders_and_mixed_changes_are_unsupported() {
        for (from, to) in [
            (&["a", "b"][..], &["a"][..]),
            (&["a", "b"], &["b", "a"]),
            (&["a", "b", "c"], &["a", "x", "y"]),
            (&["a", "b"], &["x", "b", "c"]),
            (&["a", "b", "c"], &["a", "c", "x"]),
        ] {
            let unsupported = Change::UnsupportedEnumChange {
                name: mood(),
                from: from.iter().map(|l| l.to_string()).collect(),
                to: to.iter().map(|l| l.to_string()).collect(),
            };
            assert_eq!(diff(&enums(from), &enums(to)), vec![unsupported], "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn enum_comments_are_diffed() {
        let mut b = enums(&["a"]);
        b.enums.get_mut(&mood()).expect("mood").comment = Some("c".into());
        assert_eq!(diff(&enums(&["a"]), &b), vec![set(CommentTarget::Type(mood()), Some("c"))]);
        let created = diff(&Model::empty_database(), &b);
        assert_eq!(created[1], set(CommentTarget::Type(mood()), Some("c")));
        assert!(!diff(&b, &Model::empty_database()).iter().any(|c| matches!(c, Change::SetComment { .. })));
    }

    fn opts(start: i64, max: i64) -> pgmig_model::SequenceOptions {
        pgmig_model::SequenceOptions { start, increment: 1, min: 1, max, cache: 1, cycle: false }
    }

    fn seq(name: &str, owned_by: Option<&str>) -> Sequence {
        Sequence {
            name: QualName::new("public", name),
            data_type: "integer".into(),
            options: opts(1, 2147483647),
            owned_by: owned_by.map(|c| OwnedBy { table: QualName::new("public", "t"), column: c.into() }),
            comment: None,
        }
    }

    fn with_sequences(columns: Vec<Column>, sequences: Vec<Sequence>) -> Model {
        let mut m = model(columns);
        m.sequences = sequences.into_iter().map(|s| (s.name.clone(), s)).collect();
        m
    }

    #[test]
    fn sequences_are_created_with_their_owner_altered_and_dropped() {
        let a = with_sequences(vec![col("id", "integer")], vec![seq("changed", None), seq("gone", None), seq("moved", Some("id"))]);
        let mut changed = seq("changed", None);
        changed.data_type = "bigint".into();
        changed.options.max = i64::MAX;
        let b = with_sequences(vec![col("id", "integer")], vec![changed.clone(), seq("moved", None), seq("new", Some("id"))]);
        let owner = Some(OwnedBy { table: QualName::new("public", "t"), column: "id".into() });
        assert_eq!(
            diff(&a, &b),
            vec![
                Change::AlterSequence { from: seq("changed", None), to: changed },
                Change::SetSequenceOwner { name: QualName::new("public", "moved"), owned_by: None, old: owner.clone() },
                Change::CreateSequence(seq("new", Some("id"))),
                Change::SetSequenceOwner { name: QualName::new("public", "new"), owned_by: owner, old: None },
                Change::DropSequence(seq("gone", None)),
            ]
        );
        assert!(diff(&b, &b).is_empty());
    }

    #[test]
    fn a_sequence_goes_with_the_column_or_table_that_owns_it() {
        let a = with_sequences(vec![col("id", "integer"), col("x", "integer")], vec![seq("id_seq", Some("id")), seq("x_seq", Some("x"))]);
        // Column x is dropped (and its sequence with it); the table keeps id but not its sequence.
        let b = model(vec![col("id", "integer")]);
        let changes = diff(&a, &b);
        assert!(changes.contains(&Change::DropSequence(seq("id_seq", Some("id")))), "{changes:?}");
        assert!(!changes.contains(&Change::DropSequence(seq("x_seq", Some("x")))), "{changes:?}");
        let mut none = Model::empty_database();
        none.tables.clear();
        let dropped = diff(&a, &none);
        assert!(!dropped.iter().any(|c| matches!(c, Change::DropSequence(_))), "{dropped:?}");
    }

    #[test]
    fn column_and_table_drops_carry_the_sequences_postgres_drops_with_them() {
        use pgmig_model::Generated::Always;
        let t = QualName::new("public", "t");
        let named = |name: &str| Some(Identity { sequence: Some(QualName::new("public", name)), ..identity(Always, 1).expect("identity") });
        let gen_col = Column { identity: named("t_gen_seq1"), not_null: true, ..col("gen", "integer") };
        let a = with_sequences(
            vec![col("id", "integer"), col("x", "integer"), col("kept", "integer"), gen_col],
            vec![seq("x_seq", Some("x")), seq("kept_seq", Some("x"))],
        );
        // `kept_seq` survives (without an owner), so it is not dropped with x.
        let b = with_sequences(vec![col("id", "integer")], vec![seq("kept_seq", None)]);
        let changes = diff(&a, &b);
        let drop = |c: &str, owned: &[&str]| Change::DropColumn {
            table: t.clone(),
            column: c.into(),
            type_ref: None,
            sequences: vec![],
            owned_sequences: owned.iter().map(|n| QualName::new("public", *n)).collect(),
        };
        assert!(changes.contains(&drop("x", &["x_seq"])), "{changes:?}");
        assert!(changes.contains(&drop("kept", &[])), "{changes:?}");
        assert!(changes.contains(&drop("gen", &["t_gen_seq1"])), "{changes:?}");

        let mut none = Model::empty_database();
        none.tables.clear();
        let dropped = diff(&a, &none);
        let Some(Change::DropTable { owned_sequences, .. }) = dropped.iter().find(|c| matches!(c, Change::DropTable { .. })) else {
            panic!("no DropTable in {dropped:?}")
        };
        let mut names: Vec<&str> = owned_sequences.iter().map(|n| n.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["kept_seq", "t_gen_seq1", "x_seq"]);
    }

    #[test]
    fn identity_sequence_names_never_make_a_difference() {
        use pgmig_model::Generated::Always;
        let with = |name: Option<&str>| {
            let identity = Identity { sequence: name.map(|n| QualName::new("public", n)), ..identity(Always, 1).expect("identity") };
            model(vec![Column { identity: Some(identity), not_null: true, ..col("id", "integer") }])
        };
        assert!(diff(&with(Some("t_id_seq")), &with(Some("t_id_seq1"))).is_empty());
        assert!(diff(&with(None), &with(Some("t_id_seq1"))).is_empty());
    }

    fn identity(generated: pgmig_model::Generated, start: i64) -> Option<Identity> {
        Some(Identity { generated, options: opts(start, 2147483647), sequence: None })
    }

    #[test]
    fn identity_columns_are_added_altered_and_dropped() {
        use pgmig_model::Generated::{Always, ByDefault};
        let t = QualName::new("public", "t");
        let with = |name: &str, type_name: &str, id: Option<Identity>| Column { identity: id, not_null: true, ..col(name, type_name) };
        let a = model(vec![
            with("add", "integer", None),
            with("drop", "integer", identity(Always, 1)),
            with("flip", "integer", identity(Always, 1)),
            with("same", "integer", identity(Always, 1)),
            with("retyped", "integer", identity(Always, 1)),
        ]);
        let b = model(vec![
            with("add", "integer", identity(ByDefault, 1)),
            with("drop", "integer", None),
            with("flip", "integer", identity(ByDefault, 5)),
            with("same", "integer", identity(Always, 1)),
            with("retyped", "bigint", identity(Always, 1)),
        ]);
        let changes = diff(&a, &b);
        assert!(changes.contains(&Change::AddIdentity {
            table: t.clone(),
            column: "add".into(),
            type_name: "integer".into(),
            identity: identity(ByDefault, 1).expect("identity"),
        }));
        assert!(changes.contains(&Change::DropIdentity {
            table: t.clone(),
            column: "drop".into(),
            sequence: QualName::new("public", "t_drop_seq")
        }));
        assert!(changes.contains(&Change::AlterIdentity {
            table: t.clone(),
            column: "flip".into(),
            type_name: "integer".into(),
            from: identity(Always, 1).expect("identity"),
            to: identity(ByDefault, 5).expect("identity"),
            type_changed: false,
        }));
        assert!(changes.iter().any(|c| matches!(c, Change::AlterIdentity { column, type_changed: true, .. } if column == "retyped")));
        assert!(!changes.iter().any(|c| matches!(c, Change::AlterIdentity { column, .. } if column == "same")));
    }

    #[test]
    fn defaults_carry_the_sequences_they_use() {
        let t = QualName::new("public", "t");
        let s = QualName::new("public", "s");
        let serial = Column { default: Some("nextval('s'::regclass)".into()), default_sequences: vec![s.clone()], ..col("id", "integer") };
        assert_eq!(
            diff(&model(vec![serial.clone()]), &model(vec![col("id", "integer")])),
            vec![Change::DropDefault { table: t.clone(), column: "id".into(), sequences: vec![s.clone()] }]
        );
        assert_eq!(
            diff(&model(vec![col("id", "integer")]), &model(vec![serial])),
            vec![Change::SetDefault {
                table: t,
                column: "id".into(),
                default: "nextval('s'::regclass)".into(),
                sequences: vec![s],
                old_sequences: vec![]
            }]
        );
    }

    fn check(name: &str, definition: &str) -> Constraint {
        Constraint {
            name: name.into(),
            kind: ConstraintKind::Check,
            definition: definition.into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: vec!["id".into()],
            references: None,
            comment: None,
        }
    }

    fn fk(name: &str, to: &str, uses: &str, suffix: &str) -> Constraint {
        Constraint {
            kind: ConstraintKind::ForeignKey,
            definition: format!("FOREIGN KEY (id) REFERENCES {to}(id){suffix}"),
            deferrable: suffix.contains("DEFERRABLE"),
            deferred: suffix.contains("INITIALLY DEFERRED"),
            references: Some(pgmig_model::References {
                table: QualName::new("public", to),
                columns: vec!["id".into()],
                constraint: Some(uses.into()),
            }),
            ..check(name, "")
        }
    }

    fn unique(name: &str, definition: &str) -> Constraint {
        Constraint { kind: ConstraintKind::Unique, ..check(name, definition) }
    }

    /// Tables with an `id` column and the given constraints.
    fn tables(tables: &[(&str, Vec<Constraint>)]) -> Model {
        let mut m = Model::empty_database();
        for (name, constraints) in tables {
            let name = QualName::new("public", *name);
            m.tables
                .insert(name.clone(), Table { name, columns: vec![col("id", "integer")], comment: None, constraints: constraints.clone() });
        }
        m
    }

    fn t(name: &str) -> QualName {
        QualName::new("public", name)
    }

    #[test]
    fn one_constraint_swapped_for_an_identical_one_is_a_rename() {
        let a = tables(&[("t", vec![check("old", "CHECK ((id > 0))")])]);
        let mut renamed = check("new", "CHECK ((id > 0))");
        renamed.comment = Some("c".into());
        let b = tables(&[("t", vec![renamed.clone()])]);
        assert_eq!(
            diff(&a, &b),
            vec![
                Change::RenameConstraint { table: t("t"), from: "old".into(), to: renamed },
                set(CommentTarget::Constraint { table: t("t"), name: "new".into() }, Some("c")),
            ]
        );
        // A different definition, or more than one name changing, is a drop and an add.
        let other = tables(&[("t", vec![check("new", "CHECK ((id > 1))")])]);
        assert_eq!(
            diff(&a, &other),
            vec![
                Change::DropConstraint { table: t("t"), constraint: check("old", "CHECK ((id > 0))") },
                Change::AddConstraint { table: t("t"), constraint: check("new", "CHECK ((id > 1))") },
            ]
        );
        let two = tables(&[("t", vec![check("x", "CHECK ((id > 0))"), check("y", "CHECK ((id < 9))")])]);
        let two_renamed = tables(&[("t", vec![check("x2", "CHECK ((id > 0))"), check("y2", "CHECK ((id < 9))")])]);
        let changes = diff(&two, &two_renamed);
        assert_eq!(changes.iter().filter(|c| matches!(c, Change::DropConstraint { .. })).count(), 2, "{changes:?}");
        assert_eq!(changes.iter().filter(|c| matches!(c, Change::AddConstraint { .. })).count(), 2, "{changes:?}");
    }

    #[test]
    fn a_changed_definition_is_dropped_and_added_with_its_comment() {
        let mut old = check("c", "CHECK ((id > 0))");
        old.comment = Some("same".into());
        let mut new = check("c", "CHECK ((id > 1))");
        new.comment = Some("same".into());
        assert_eq!(
            diff(&tables(&[("t", vec![old.clone()])]), &tables(&[("t", vec![new.clone()])])),
            vec![
                Change::DropConstraint { table: t("t"), constraint: old },
                Change::AddConstraint { table: t("t"), constraint: new },
                set(CommentTarget::Constraint { table: t("t"), name: "c".into() }, Some("same")),
            ]
        );
    }

    #[test]
    fn only_deferrability_changing_alters_a_foreign_key_in_place() {
        let p = ("p", vec![unique("p_id_key", "UNIQUE (id)")]);
        let a = tables(&[p.clone(), ("t", vec![fk("f", "p", "p_id_key", ""), fk("g", "p", "p_id_key", " DEFERRABLE")])]);
        let to_f = fk("f", "p", "p_id_key", " DEFERRABLE INITIALLY DEFERRED");
        let to_g = fk("g", "p", "p_id_key", " ON DELETE CASCADE DEFERRABLE");
        let b = tables(&[p, ("t", vec![to_f.clone(), to_g.clone()])]);
        assert_eq!(
            diff(&a, &b),
            vec![
                Change::AlterConstraint { table: t("t"), to: to_f },
                Change::DropConstraint { table: t("t"), constraint: fk("g", "p", "p_id_key", " DEFERRABLE") },
                Change::AddConstraint { table: t("t"), constraint: to_g },
            ]
        );
    }

    #[test]
    fn a_foreign_key_over_a_retyped_column_is_dropped_and_added_again() {
        let f = fk("f", "p", "p_pkey", "");
        let pk = Constraint { kind: ConstraintKind::PrimaryKey, ..check("p_pkey", "PRIMARY KEY (id)") };
        let a = tables(&[("p", vec![pk.clone()]), ("t", vec![f.clone()])]);
        let retype = |m: &mut Model, table: &str| m.tables.get_mut(&t(table)).expect("table").columns[0].type_name = "text".into();
        let alter = |table: &str| Change::AlterColumnType {
            table: t(table),
            column: "id".into(),
            type_name: "text".into(),
            type_ref: None,
            old_type_ref: None,
            using: None,
        };
        let fk_changes =
            [Change::DropConstraint { table: t("t"), constraint: f.clone() }, Change::AddConstraint { table: t("t"), constraint: f }];
        // The referenced column, the referencing column, and both: the key is replaced each time,
        // the primary key stays.
        for tables_retyped in [vec!["p"], vec!["t"], vec!["p", "t"]] {
            let mut b = a.clone();
            for table in &tables_retyped {
                retype(&mut b, table);
            }
            let mut expected: Vec<Change> = tables_retyped.iter().map(|table| alter(table)).collect();
            expected.extend(fk_changes.clone());
            assert_eq!(diff(&a, &b), expected, "{tables_retyped:?}");
        }
    }

    #[test]
    fn a_foreign_key_using_a_replaced_constraint_is_added_again() {
        let f = fk("f", "p", "p_id_key", "");
        let a = tables(&[("p", vec![unique("p_id_key", "UNIQUE (id)")]), ("t", vec![f.clone()])]);
        let b = tables(&[("p", vec![unique("p_id_key", "UNIQUE NULLS NOT DISTINCT (id)")]), ("t", vec![f.clone()])]);
        assert_eq!(
            diff(&a, &b),
            vec![
                Change::DropConstraint { table: t("p"), constraint: unique("p_id_key", "UNIQUE (id)") },
                Change::AddConstraint { table: t("p"), constraint: unique("p_id_key", "UNIQUE NULLS NOT DISTINCT (id)") },
                Change::DropConstraint { table: t("t"), constraint: f.clone() },
                Change::AddConstraint { table: t("t"), constraint: f },
            ]
        );
    }

    #[test]
    fn a_new_table_creates_its_keys_and_checks_and_adds_foreign_keys_after() {
        let pk = Constraint { kind: ConstraintKind::PrimaryKey, ..check("t_pkey", "PRIMARY KEY (id)") };
        let unvalidated = Constraint { validated: false, ..check("later", "CHECK ((id > 0)) NOT VALID") };
        let mut f = fk("t_id_fkey", "t", "t_pkey", "");
        f.comment = Some("self".into());
        let b = tables(&[("t", vec![pk.clone(), unvalidated.clone(), f.clone()])]);
        let changes = diff(&Model::empty_database(), &b);
        let Some(Change::CreateTable(created)) = changes.first() else { panic!("{changes:?}") };
        assert_eq!(created.constraints, vec![pk]);
        assert_eq!(
            changes[1..],
            [
                Change::AddConstraint { table: t("t"), constraint: unvalidated },
                Change::AddConstraint { table: t("t"), constraint: f },
                set(CommentTarget::Constraint { table: t("t"), name: "t_id_fkey".into() }, Some("self")),
            ]
        );
    }

    #[test]
    fn dropped_tables_drop_their_constraints_except_foreign_keys_in_a_cycle() {
        let pk = |table: &str| Constraint { kind: ConstraintKind::PrimaryKey, ..check(&format!("{table}_pkey"), "PRIMARY KEY (id)") };
        let a = tables(&[
            ("parent", vec![pk("parent")]),
            ("child", vec![fk("child_fkey", "parent", "parent_pkey", "")]),
            ("m1", vec![pk("m1"), fk("m1_fkey", "m2", "m2_pkey", "")]),
            ("m2", vec![pk("m2"), fk("m2_fkey", "m1", "m1_pkey", "")]),
            ("tree", vec![pk("tree"), fk("tree_fkey", "tree", "tree_pkey", "")]),
        ]);
        let changes = diff(&a, &Model::empty_database());
        let explicit: Vec<&str> = changes
            .iter()
            .filter_map(|c| match c {
                Change::DropConstraint { constraint, .. } => Some(constraint.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(explicit, ["m1_fkey", "m2_fkey"]);
        let with_table = |name: &str| {
            changes.iter().find_map(|c| match c {
                Change::DropTable { table, .. } if table.name.name == name => {
                    Some(table.constraints.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
                }
                _ => None,
            })
        };
        assert_eq!(with_table("child"), Some(vec!["child_fkey".to_owned()]));
        assert_eq!(with_table("m1"), Some(vec!["m1_pkey".to_owned()]));
        assert_eq!(with_table("tree"), Some(vec!["tree_pkey".to_owned(), "tree_fkey".to_owned()]));
    }

    #[test]
    fn constraint_comments_are_diffed_and_unchanged_constraints_are_left_alone() {
        let with = |comment: Option<&str>| {
            let mut c = check("c", "CHECK ((id > 0))");
            c.comment = comment.map(Into::into);
            tables(&[("t", vec![c])])
        };
        assert!(diff(&with(None), &with(None)).is_empty());
        assert_eq!(diff(&with(Some("a")), &with(None)), vec![set(CommentTarget::Constraint { table: t("t"), name: "c".into() }, None)]);
        assert_eq!(
            diff(&with(None), &with(Some("b"))),
            vec![set(CommentTarget::Constraint { table: t("t"), name: "c".into() }, Some("b"))]
        );
    }

    /// One change of every variant; [`variant`] fails to compile when a variant is added.
    fn every_change() -> Vec<Change> {
        use pgmig_model::{Generated, SequenceOptions};
        let t = QualName::new("public", "t");
        let s = QualName::new("public", "s");
        let e = QualName::new("public", "e");
        let ints = SequenceOptions::defaults("integer", 1).expect("integer");
        let identity = Identity { generated: Generated::Always, options: ints, sequence: Some(QualName::new("public", "t_id_seq")) };
        let constraint = Constraint {
            name: "t_pkey".into(),
            kind: ConstraintKind::PrimaryKey,
            definition: "PRIMARY KEY (id)".into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: vec!["id".into()],
            references: None,
            comment: None,
        };
        let table = Table {
            name: t.clone(),
            columns: vec![col("id", "integer")],
            comment: Some("t".into()),
            constraints: vec![constraint.clone()],
        };
        let owned = OwnedBy { table: t.clone(), column: "id".into() };
        let sequence =
            Sequence { name: s.clone(), data_type: "bigint".into(), options: ints, owned_by: Some(owned.clone()), comment: None };
        let enum_ = Enum { name: e.clone(), labels: vec!["a".into()], comment: None };
        let id = || "id".to_owned();
        vec![
            Change::CreateSchema("app".into()),
            Change::DropSchema("old".into()),
            Change::CreateTable(table.clone()),
            Change::DropTable { table, owned_sequences: vec![s.clone()] },
            Change::AddColumn { table: t.clone(), column: col("n", "text") },
            Change::DropColumn {
                table: t.clone(),
                column: id(),
                type_ref: Some(e.clone()),
                sequences: vec![s.clone()],
                owned_sequences: vec![],
            },
            Change::AlterColumnType {
                table: t.clone(),
                column: id(),
                type_name: "bigint".into(),
                type_ref: None,
                old_type_ref: Some(e.clone()),
                using: Some("id::bigint".into()),
            },
            Change::SetNotNull { table: t.clone(), column: id() },
            Change::DropNotNull { table: t.clone(), column: id() },
            Change::SetDefault { table: t.clone(), column: id(), default: "0".into(), sequences: vec![], old_sequences: vec![s.clone()] },
            Change::DropDefault { table: t.clone(), column: id(), sequences: vec![s.clone()] },
            Change::AddIdentity { table: t.clone(), column: id(), type_name: "integer".into(), identity: identity.clone() },
            Change::AlterIdentity {
                table: t.clone(),
                column: id(),
                type_name: "bigint".into(),
                from: identity.clone(),
                to: Identity { generated: Generated::ByDefault, ..identity },
                type_changed: true,
            },
            Change::DropIdentity { table: t.clone(), column: id(), sequence: s.clone() },
            Change::CreateSequence(sequence.clone()),
            Change::DropSequence(sequence.clone()),
            Change::AlterSequence { from: sequence.clone(), to: Sequence { data_type: "integer".into(), ..sequence } },
            Change::SetSequenceOwner { name: s.clone(), owned_by: None, old: Some(owned) },
            Change::CreateEnum(enum_.clone()),
            Change::DropEnum(enum_),
            Change::AddEnumValue { name: e.clone(), value: "b".into(), position: EnumPosition::After("a".into()) },
            Change::AddEnumValue { name: e.clone(), value: "0".into(), position: EnumPosition::Before("a".into()) },
            Change::AddEnumValue { name: e.clone(), value: "z".into(), position: EnumPosition::End },
            Change::RenameEnumValue { name: e.clone(), from: "a".into(), to: "b".into() },
            Change::UnsupportedEnumChange { name: e.clone(), from: vec!["a".into(), "b".into()], to: vec!["b".into(), "a".into()] },
            Change::AddConstraint { table: t.clone(), constraint: constraint.clone() },
            Change::DropConstraint { table: t.clone(), constraint: constraint.clone() },
            Change::RenameConstraint { table: t.clone(), from: "old".into(), to: constraint.clone() },
            Change::AlterConstraint { table: t.clone(), to: constraint },
            Change::SetComment { target: CommentTarget::Schema("app".into()), comment: Some("x".into()) },
            Change::SetComment { target: CommentTarget::Table(t.clone()), comment: None },
            Change::SetComment { target: CommentTarget::Column { table: t.clone(), column: id() }, comment: Some("c".into()) },
            Change::SetComment { target: CommentTarget::Type(e), comment: None },
            Change::SetComment { target: CommentTarget::Sequence(s), comment: None },
            Change::SetComment { target: CommentTarget::Constraint { table: t, name: "t_pkey".into() }, comment: None },
        ]
    }

    /// The variant's index in the declaration, by an exhaustive match.
    fn variant(c: &Change) -> usize {
        match c {
            Change::CreateSchema(_) => 0,
            Change::DropSchema(_) => 1,
            Change::CreateTable(_) => 2,
            Change::DropTable { .. } => 3,
            Change::AddColumn { .. } => 4,
            Change::DropColumn { .. } => 5,
            Change::AlterColumnType { .. } => 6,
            Change::SetNotNull { .. } => 7,
            Change::DropNotNull { .. } => 8,
            Change::SetDefault { .. } => 9,
            Change::DropDefault { .. } => 10,
            Change::AddIdentity { .. } => 11,
            Change::AlterIdentity { .. } => 12,
            Change::DropIdentity { .. } => 13,
            Change::CreateSequence(_) => 14,
            Change::DropSequence(_) => 15,
            Change::AlterSequence { .. } => 16,
            Change::SetSequenceOwner { .. } => 17,
            Change::CreateEnum(_) => 18,
            Change::DropEnum(_) => 19,
            Change::AddEnumValue { .. } => 20,
            Change::RenameEnumValue { .. } => 21,
            Change::UnsupportedEnumChange { .. } => 22,
            Change::AddConstraint { .. } => 23,
            Change::DropConstraint { .. } => 24,
            Change::RenameConstraint { .. } => 25,
            Change::AlterConstraint { .. } => 26,
            Change::SetComment { .. } => 27,
        }
    }

    #[test]
    fn every_change_round_trips_through_json() {
        let changes = every_change();
        let covered: BTreeSet<usize> = changes.iter().map(variant).collect();
        assert_eq!(covered, (0..28).collect(), "a sample for every variant");
        for c in &changes {
            let json = serde_json::to_string(c).expect("serialize");
            let back: Change = serde_json::from_str(&json).expect(&json);
            assert_eq!(&back, c, "{json}");
            assert_eq!(serde_json::to_string(&back).expect("serialize"), json);
        }
    }

    /// The JSON of a change is part of pgmig's interface: a change here must be deliberate.
    #[test]
    fn change_json_is_pinned() {
        let t = QualName::new("public", "t");
        let json = |c: &Change| serde_json::to_string(c).expect("serialize");
        assert_eq!(json(&Change::CreateSchema("app".into())), r#"{"kind":"create_schema","data":"app"}"#);
        assert_eq!(
            json(&Change::SetNotNull { table: t.clone(), column: "id".into() }),
            r#"{"kind":"set_not_null","data":{"table":{"schema":"public","name":"t"},"column":"id"}}"#
        );
        assert_eq!(
            json(&Change::AddEnumValue { name: t.clone(), value: "b".into(), position: EnumPosition::End }),
            r#"{"kind":"add_enum_value","data":{"name":{"schema":"public","name":"t"},"value":"b","position":{"kind":"end"}}}"#
        );
        assert_eq!(
            json(&Change::SetComment { target: CommentTarget::Column { table: t, column: "id".into() }, comment: None }),
            r#"{"kind":"set_comment","data":{"target":{"kind":"column","data":{"table":{"schema":"public","name":"t"},"column":"id"}},"comment":null}}"#
        );
    }
}
