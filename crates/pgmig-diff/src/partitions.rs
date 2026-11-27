//! Partitions: what the diff does before it compares tables (see the crate doc, "Partitioning").
//!
//! A partition's model holds only what is its own ([`pgmig_model::Table::partition_of`]). A table
//! that stops or starts being a partition is compared as the standalone table it is before
//! `DETACH PARTITION` or must be before `ATTACH PARTITION`: [`prepare`] puts those tables in
//! copies of the two models, and the diff compares the copies.

use pgmig_model::{Constraint, ConstraintKind, Model, PartitionOf, QualName, Table, inherited_constraint, inherited_index, quote_ident};

use crate::Change;

/// The models to compare and the partition changes to add to what they differ in.
pub(crate) struct Prepared {
    pub a: Model,
    pub b: Model,
    pub changes: Vec<Change>,
}

/// See the module doc. Partition changes that cannot be planned are
/// [`Change::UnsupportedPartitionChange`]; the table is then compared as it is.
pub(crate) fn prepare(a: &Model, b: &Model) -> Prepared {
    let mut out = Prepared { a: a.clone(), b: b.clone(), changes: Vec::new() };
    for (name, new) in &b.tables {
        let Some(old) = a.tables.get(name) else {
            if new.partition_of.is_some() && !new.columns.is_empty() {
                out.changes.push(unsupported(
                    name,
                    format!(
                        "a new partition cannot have columns that differ from its parent's ({})",
                        column_list(new.columns.iter().map(|c| c.name.as_str()))
                    ),
                ));
            }
            continue;
        };
        if old.partition_key != new.partition_key {
            out.changes.push(unsupported(
                name,
                format!(
                    "the partitioning cannot change from {} to {}: Postgres cannot change it in place, so the table must be dropped and created again",
                    partitioning(&old.partition_key),
                    partitioning(&new.partition_key)
                ),
            ));
        }
        match (&old.partition_of, &new.partition_of) {
            (None, None) => {}
            (Some(o), Some(n)) => kept_partition(a, b, old, new, o, n, &mut out),
            (Some(o), None) => detach(a, old, o, &mut out),
            (None, Some(n)) => attach(b, old, new, n, &mut out),
        }
    }
    out
}

fn unsupported(name: &QualName, detail: String) -> Change {
    Change::UnsupportedPartitionChange { name: name.clone(), detail }
}

fn partitioning(key: &Option<String>) -> String {
    match key {
        Some(k) => format!("PARTITION BY {k}"),
        None => "not partitioned".to_owned(),
    }
}

fn column_list<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.map(quote_ident).collect::<Vec<_>>().join(", ")
}

/// The columns `table` has in model `m` (its parent's, for a partition), by name.
fn column_names(m: &Model, table: &QualName) -> Vec<String> {
    m.table_columns(table).unwrap_or_default().into_iter().map(|c| c.name).collect()
}

/// A table that is a partition in both models: the same, re-bound (detached and attached again
/// with the new bound), or moved to another parent (unsupported).
fn kept_partition(a: &Model, b: &Model, old: &Table, new: &Table, o: &PartitionOf, n: &PartitionOf, out: &mut Prepared) {
    let name = &new.name;
    if old.columns != new.columns {
        out.changes.push(unsupported(
            name,
            "the columns of a partition that differ from its parent's cannot change: Postgres applies the parent's column changes to its partitions".to_owned(),
        ));
        return;
    }
    // A column the partition holds as its own while the parent's changes: the change would reach
    // the partition too.
    let (Some(pa), Some(pb)) = (a.table_columns(&o.parent), b.table_columns(&n.parent)) else { return };
    let changed: Vec<&str> = new
        .columns
        .iter()
        .filter(|c| {
            let (x, y) = (pa.iter().find(|p| p.name == c.name), pb.iter().find(|p| p.name == c.name));
            x.map(|x| (&x.default, x.not_null)) != y.map(|y| (&y.default, y.not_null))
        })
        .map(|c| c.name.as_str())
        .collect();
    if !changed.is_empty() {
        out.changes.push(unsupported(
            name,
            format!(
                "its own default or NOT NULL of {} would be overwritten by the change to the parent's column, which Postgres applies to its partitions",
                column_list(changed.into_iter())
            ),
        ));
        return;
    }
    if o == n {
        return;
    }
    if o.parent != n.parent {
        out.changes.push(unsupported(
            name,
            format!("a partition of {} cannot become a partition of {}; detach it and attach it by hand", o.parent, n.parent),
        ));
        return;
    }
    out.changes.push(Change::DetachPartition { table: name.clone(), parent: o.parent.clone(), columns: column_names(a, name) });
    out.changes.push(Change::AttachPartition { table: name.clone(), partition_of: n.clone(), columns: column_names(b, name) });
    if let Some(t) = out.a.tables.get_mut(name) {
        t.partition_of = Some(n.clone());
    }
}

/// A partition of `a` that is a standalone table in `b`: `DETACH PARTITION`, then the table as it
/// is once detached (with its parent's columns, checks and foreign keys) is compared with `b`'s.
fn detach(a: &Model, old: &Table, o: &PartitionOf, out: &mut Prepared) {
    let name = &old.name;
    let ancestors = a.partition_ancestors(name);
    let named_by_server = |t: &&&Table| {
        !t.indexes.is_empty()
            || t.constraints
                .iter()
                .any(|c| matches!(c.kind, ConstraintKind::PrimaryKey | ConstraintKind::Unique | ConstraintKind::Exclusion))
    };
    if let Some(t) = ancestors.iter().find(named_by_server) {
        out.changes.push(unsupported(
            name,
            format!(
                "it cannot be detached from {} while {} has indexes or primary key, unique or exclusion constraints: the detached table keeps its copies of them under names Postgres chose; detach it by hand",
                o.parent, t.name
            ),
        ));
        return;
    }
    let Some(columns) = a.table_columns(name) else {
        out.changes.push(unsupported(name, format!("it cannot be detached from {}, which pgmig does not manage", o.parent)));
        return;
    };
    let mut detached = Table { columns, partition_of: None, ..old.clone() };
    for c in ancestors.iter().flat_map(|t| &t.constraints) {
        if detached.constraint(&c.name).is_none() {
            detached.constraints.push(Constraint { on_partitioned_table: old.partition_key.is_some(), ..c.clone() });
        }
    }
    detached.constraints.sort_by(|x, y| x.name.cmp(&y.name));
    out.changes.push(Change::DetachPartition { table: name.clone(), parent: o.parent.clone(), columns: column_names(a, name) });
    out.a.tables.insert(name.clone(), detached);
}

/// A standalone table of `a` that is a partition in `b`: the table is first made what `ATTACH
/// PARTITION` needs (its columns those of the parent; constraints and indexes the parent gives it
/// are kept when it has them already, as Postgres adopts them), then attached.
fn attach(b: &Model, old: &Table, new: &Table, n: &PartitionOf, out: &mut Prepared) {
    let name = &new.name;
    if !new.columns.is_empty() {
        out.changes.push(unsupported(
            name,
            format!(
                "a table attached as a partition cannot have columns that differ from its parent's ({})",
                column_list(new.columns.iter().map(|c| c.name.as_str()))
            ),
        ));
        return;
    }
    let Some(columns) = b.table_columns(name) else {
        out.changes.push(unsupported(name, format!("it cannot be attached to {}, which pgmig does not manage", n.parent)));
        return;
    };
    let ancestors = b.partition_ancestors(name);
    let mut attached = Table { columns, partition_of: None, ..new.clone() };
    for c in &old.constraints {
        if attached.constraint(&c.name).is_none() && ancestors.iter().any(|t| t.constraints.iter().any(|pc| inherited_constraint(pc, c))) {
            attached.constraints.push(c.clone());
        }
    }
    for i in &old.indexes {
        if attached.index(&i.name).is_none() && ancestors.iter().any(|t| t.indexes.iter().any(|pi| inherited_index(&t.name, pi, name, i))) {
            attached.indexes.push(i.clone());
        }
    }
    attached.constraints.sort_by(|x, y| x.name.cmp(&y.name));
    attached.indexes.sort_by(|x, y| x.name.cmp(&y.name));
    out.changes.push(Change::AttachPartition { table: name.clone(), partition_of: n.clone(), columns: column_names(b, name) });
    out.b.tables.insert(name.clone(), attached);
}
