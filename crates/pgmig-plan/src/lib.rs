//! Turns [`Change`]s into ordered SQL.
//!
//! Each change declares the objects it creates, drops and references ([`graph::Deps`]); the
//! statements are ordered by that dependency graph, so creates run after what they reference,
//! drops run before what they reference, and an alter runs after the creates it needs and before
//! the drops it unblocks. Unrelated statements keep a fixed phase order (create schemas, create
//! tables, alter columns, drop columns, tables, schemas) and then the diff's order. See
//! [`graph`] for the rules and for how cycles are reported.
//!
//! # Transactions
//!
//! `ALTER TYPE ... ADD VALUE` may run in a transaction, but the new value cannot be used (as a
//! column default, say) until that transaction commits. So the plan puts every `ADD VALUE` first
//! and, when other statements follow, marks a commit point after them ([`Plan::commit_after`]):
//! [`Plan::batches`] are the scripts to run in separate transactions, and [`Plan::to_sql`] writes
//! the point as a comment and the `-- pgmig:commit` directive ([`COMMIT_MARKER`]), which
//! `migrate apply` commits at when it runs a file `migrate diff` wrote. pgmig does not check which
//! later statements use a new value; any statement after an `ADD VALUE` waits for the commit.
//!
//! # Sequences
//!
//! `CREATE SEQUENCE` and identity columns print only the options that differ from what Postgres
//! would pick for the type and direction ([`SequenceOptions::defaults`]); `ALTER SEQUENCE` and
//! `ALTER COLUMN ... SET` print the options that changed (and both bounds when the type changes,
//! since Postgres resets bounds that were the old type's limits). `OWNED BY` is set after the
//! owning table exists, so a `serial` column plans as `CREATE SEQUENCE`, `CREATE TABLE` with its
//! `nextval` default, then `ALTER SEQUENCE ... OWNED BY`. Plans never `RESTART` a sequence: a
//! sequence whose current value falls outside new bounds fails to apply.
//!
//! # Constraints
//!
//! A new table's primary key, unique, check and exclusion constraints are written in its `CREATE
//! TABLE`; foreign keys are always `ALTER TABLE ... ADD CONSTRAINT` steps after the tables they
//! join (and the constraint whose index they use), so tables that reference each other plan
//! without a cycle. A foreign key is dropped before the table or constraint it references; a
//! constraint is dropped before the columns it uses change or go, and added after they are set.
//!
//! # Indexes
//!
//! `CREATE INDEX` is the statement the server printed for the index (schema-qualified, whatever
//! [`PlanOptions`] says), run after the table and the columns it uses exist and have settled.
//! `DROP INDEX` runs before the columns it uses change or go, and before a constraint or index of
//! the same name is created. A table's indexes are dropped with it. Indexes are created without
//! `CONCURRENTLY`: the plan runs in a transaction.
//!
//! # Objects outside the changes
//!
//! A change may reference an object no change creates or drops: one that already exists and is
//! left alone, or one pgmig does not manage (filtered out, or outside the schema in scope). Such a
//! reference orders nothing; the plan assumes the object exists.
//!
//! Changes Postgres cannot make in place (removing or reordering enum values) fail the plan with
//! [`Error::Unsupported`].
//!
//! # Policy
//!
//! [`skip`] leaves chosen kinds of change out before planning, with the changes that depend on
//! them (see [`policy`]).

pub mod graph;
pub mod policy;

pub use policy::{Cause, Skipped, skip};

use graph::{Deps, Node, Object};
use pgmig_diff::{Change, CommentTarget, EnumPosition};
use pgmig_model::{
    Column, Constraint, ConstraintKind, Generated, Identity, OwnedBy, QualName, SequenceOptions, quote_ident, quote_literal,
};
use serde::{Deserialize, Serialize};

/// Serialized as `{"statements": [...], "commit_after": <index or null>}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub statements: Vec<String>,
    /// The index of the last statement that must be committed before the statements after it run
    /// (see the module doc); `None` when the whole plan can run in one transaction.
    pub commit_after: Option<usize>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// The plan as a SQL script: one statement per entry, each ending in `;`, with
    /// [`COMMIT_MARKER`] at the commit point.
    pub fn to_sql(&self) -> String {
        self.batches().join(COMMIT_MARKER)
    }

    /// The plan as scripts that must each be committed before the next one runs.
    pub fn batches(&self) -> Vec<String> {
        let script = |statements: &[String]| statements.iter().map(|s| format!("{s};\n")).collect::<String>();
        match self.commit_after {
            Some(i) if i + 1 < self.statements.len() => {
                vec![script(&self.statements[..=i]), script(&self.statements[i + 1..])]
            }
            _ => vec![script(&self.statements)],
        }
    }
}

/// Marks [`Plan::commit_after`] in [`Plan::to_sql`]: a comment for the reader, then the
/// `-- pgmig:commit` directive. `migrate apply` keys on the directive, never on the comment.
pub const COMMIT_MARKER: &str = "-- New enum values cannot be used in the transaction that adds them.\n-- pgmig:commit\n";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Statements that each need the next one to run first (and the last needs the first).
    #[error("the changes depend on each other in a cycle: {}", .0.join("; "))]
    Cycle(Vec<String>),
    /// Changes that cannot be planned, one message each.
    #[error("unsupported changes: {}", .0.join("; "))]
    Unsupported(Vec<String>),
}

/// How a plan prints object names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Objects in this schema are printed unqualified (`CREATE TABLE t`, not `public.t`): the plan
    /// is meant to run with the schema first on the `search_path`. Names in other schemas, and
    /// SQL the server printed (types, defaults, constraint definitions), are left as they are:
    /// inspection prints those schema-qualified (`'a'::public.mood`, `REFERENCES public.b(id)`).
    pub unqualified_schema: Option<String>,
}

impl PlanOptions {
    /// `name` as statements print it.
    pub fn name(&self, name: &QualName) -> String {
        match &self.unqualified_schema {
            Some(s) if *s == name.schema => quote_ident(&name.name),
            _ => name.to_string(),
        }
    }
}

/// [`plan_with`] with qualified names.
pub fn plan(changes: &[Change]) -> Result<Plan, Error> {
    plan_with(changes, &PlanOptions::default())
}

pub fn plan_with(changes: &[Change], opts: &PlanOptions) -> Result<Plan, Error> {
    let unsupported: Vec<String> = changes.iter().filter_map(unsupported).collect();
    if !unsupported.is_empty() {
        return Err(Error::Unsupported(unsupported));
    }
    let (adds, rest): (Vec<&Change>, Vec<&Change>) = changes.iter().partition(|c| matches!(c, Change::AddEnumValue { .. }));
    let mut statements = sorted(&adds, opts)?;
    let commit_after = (!statements.is_empty() && !rest.is_empty()).then(|| statements.len() - 1);
    statements.extend(sorted(&rest, opts)?);
    Ok(Plan { statements, commit_after })
}

fn sorted(changes: &[&Change], opts: &PlanOptions) -> Result<Vec<String>, Error> {
    let nodes: Vec<Node> = changes.iter().map(|c| Node { phase: phase(c), deps: deps(c) }).collect();
    match graph::sort(&nodes) {
        Ok(order) => Ok(order.into_iter().map(|i| statement(changes[i], opts)).collect()),
        Err(graph::Cycle(steps)) => Err(Error::Cycle(steps.into_iter().map(|i| statement(changes[i], opts)).collect())),
    }
}

fn unsupported(change: &Change) -> Option<String> {
    let Change::UnsupportedEnumChange { name, from, to } = change else { return None };
    let list = |labels: &[String]| labels.iter().map(|l| quote_literal(l)).collect::<Vec<_>>().join(", ");
    Some(format!(
        "enum {name}: values ({}) cannot become ({}); only adding values and renaming one value in place are supported, not removing or reordering values",
        list(from),
        list(to)
    ))
}

/// The tie-break between statements the graph does not order.
fn phase(change: &Change) -> u8 {
    match change {
        // Planned on their own, before everything else (see the module doc).
        Change::AddEnumValue { .. } | Change::UnsupportedEnumChange { .. } => 0,
        Change::CreateSchema(_) => 0,
        Change::CreateEnum(_) | Change::CreateSequence(_) => 1,
        // Before defaults that print the new name are set.
        Change::RenameEnumValue { .. } | Change::AlterSequence { .. } => 2,
        Change::CreateTable(_) => 3,
        Change::AddColumn { .. } => 4,
        // A column cannot have a default and an identity at once. A constraint goes before the
        // columns it uses change (a primary key column cannot drop NOT NULL).
        Change::DropIdentity { .. } | Change::DropDefault { .. } | Change::DropConstraint { .. } | Change::DropIndex { .. } => 5,
        Change::AlterColumnType { .. } => 6,
        Change::SetDefault { .. } => 7,
        Change::DropNotNull { .. } | Change::SetNotNull { .. } => 8,
        // After the type, the dropped default and NOT NULL that adding an identity needs.
        Change::AddIdentity { .. } | Change::AlterIdentity { .. } => 9,
        // After the columns they use have settled.
        Change::SetSequenceOwner { .. }
        | Change::AddConstraint { .. }
        | Change::RenameConstraint { .. }
        | Change::AlterConstraint { .. }
        | Change::CreateIndex { .. } => 10,
        Change::DropColumn { .. } => 11,
        Change::DropTable { .. } => 12,
        Change::DropSequence(_) => 13,
        Change::DropEnum(_) => 14,
        Change::DropSchema(_) => 15,
        // After the object it describes exists and has settled.
        Change::SetComment { .. } => 16,
    }
}

fn deps(change: &Change) -> Deps {
    let mut d = object_deps(change);
    // Tables, sequences and indexes share the relation namespace (see [`Object::Relation`]).
    let relations = |objects: &[Object]| -> Vec<Object> {
        objects
            .iter()
            .filter_map(|o| match o {
                Object::Table(q) | Object::Sequence(q) | Object::Index(q) => Some(Object::Relation(q.clone())),
                _ => None,
            })
            .collect()
    };
    let (created, dropped) = (relations(&d.creates), relations(&d.drops));
    d.creates.extend(created);
    d.drops.extend(dropped);
    d
}

fn object_deps(change: &Change) -> Deps {
    let column = |table: &QualName, column: &str| Object::Column(table.clone(), column.to_owned());
    let in_table = |table: &QualName| Object::Table(table.clone());
    match change {
        Change::CreateSchema(name) => Deps { creates: vec![Object::Schema(name.clone())], ..Deps::default() },
        Change::DropSchema(name) => Deps { drops: vec![Object::Schema(name.clone())], ..Deps::default() },
        Change::CreateTable(t) => {
            let mut requires = vec![Object::Schema(t.name.schema.clone())];
            requires.extend(types(t.columns.iter().map(|c| &c.type_ref)));
            requires.extend(sequences(t.columns.iter().flat_map(|c| &c.default_sequences)));
            let mut creates = vec![Object::Table(t.name.clone()), Object::Type(t.name.clone())];
            creates.extend(t.columns.iter().filter_map(|c| Some(identity_sequence(&t.name, &c.name, c.identity.as_ref()?))));
            creates.extend(t.constraints.iter().map(|c| constraint(&t.name, &c.name)));
            creates.extend(table_indexes(t));
            Deps { creates, requires, ..Deps::default() }
        }
        Change::DropTable { table: t, owned_sequences } => {
            let mut releases = vec![Object::Schema(t.name.schema.clone())];
            releases.extend(types(t.columns.iter().map(|c| &c.type_ref)));
            releases.extend(sequences(t.columns.iter().flat_map(|c| &c.default_sequences)));
            // With the table go the sequences its columns own and its identity sequences.
            let mut drops = vec![Object::Table(t.name.clone()), Object::Type(t.name.clone())];
            drops.extend(sequences(owned_sequences));
            // With the table go its constraints, and its foreign keys stop using what they reference.
            drops.extend(t.constraints.iter().map(|c| constraint(&t.name, &c.name)));
            drops.extend(table_indexes(t));
            releases.extend(t.constraints.iter().flat_map(referenced).filter(|o| !is_of_table(o, &t.name)));
            Deps { drops, releases, ..Deps::default() }
        }
        Change::CreateSequence(s) => Deps {
            creates: vec![Object::Sequence(s.name.clone())],
            requires: vec![Object::Schema(s.name.schema.clone())],
            ..Deps::default()
        },
        Change::DropSequence(s) => {
            Deps { drops: vec![Object::Sequence(s.name.clone())], releases: vec![Object::Schema(s.name.schema.clone())], ..Deps::default() }
        }
        Change::AlterSequence { to, .. } => Deps { alters: vec![Object::Sequence(to.name.clone())], ..Deps::default() },
        Change::SetSequenceOwner { name, owned_by, old } => {
            let owner = |o: &Option<OwnedBy>| match o {
                Some(o) => vec![in_table(&o.table), column(&o.table, &o.column)],
                None => Vec::new(),
            };
            Deps {
                alters: vec![Object::Sequence(name.clone())],
                requires: [vec![Object::Sequence(name.clone())], owner(owned_by)].concat(),
                releases: owner(old),
                ..Deps::default()
            }
        }
        Change::AddIdentity { table, column: c, identity, .. } => Deps {
            // The implicit sequence takes the name a dropped sequence may still hold.
            creates: vec![identity_sequence(table, c, identity)],
            alters: vec![column(table, c)],
            requires: vec![in_table(table)],
            ..Deps::default()
        },
        Change::DropIdentity { table, column: c, sequence } => Deps {
            drops: vec![Object::Sequence(sequence.clone())],
            alters: vec![column(table, c)],
            requires: vec![in_table(table)],
            ..Deps::default()
        },
        Change::AlterIdentity { table, column: c, .. } => {
            Deps { alters: vec![column(table, c)], requires: vec![in_table(table)], ..Deps::default() }
        }
        Change::CreateEnum(e) => {
            Deps { creates: vec![Object::Type(e.name.clone())], requires: vec![Object::Schema(e.name.schema.clone())], ..Deps::default() }
        }
        Change::DropEnum(e) => {
            Deps { drops: vec![Object::Type(e.name.clone())], releases: vec![Object::Schema(e.name.schema.clone())], ..Deps::default() }
        }
        Change::AddEnumValue { name, .. } | Change::RenameEnumValue { name, .. } => {
            Deps { alters: vec![Object::Type(name.clone())], ..Deps::default() }
        }
        Change::UnsupportedEnumChange { .. } => Deps::default(),
        Change::AddColumn { table, column: c } => {
            let mut requires = vec![in_table(table)];
            requires.extend(types([&c.type_ref]));
            requires.extend(sequences(&c.default_sequences));
            let mut creates = vec![column(table, &c.name)];
            if let Some(identity) = &c.identity {
                creates.push(identity_sequence(table, &c.name, identity));
            }
            Deps { creates, requires, ..Deps::default() }
        }
        Change::DropColumn { table, column: c, type_ref, sequences: used, owned_sequences } => {
            let mut releases = types([type_ref]);
            releases.extend(sequences(used));
            // Postgres drops the sequences the column owns with it.
            let drops = [vec![column(table, c)], sequences(owned_sequences)].concat();
            Deps { drops, requires: vec![in_table(table)], releases, ..Deps::default() }
        }
        Change::AlterColumnType { table, column: c, type_ref, old_type_ref, .. } => {
            let mut requires = vec![in_table(table)];
            requires.extend(types([type_ref]));
            let releases = if type_ref == old_type_ref { Vec::new() } else { types([old_type_ref]) };
            Deps { alters: vec![column(table, c), Object::ColumnType(table.clone(), c.clone())], requires, releases, ..Deps::default() }
        }
        Change::AddConstraint { table, constraint: c } => {
            let columns: Vec<Object> = c.columns.iter().map(|col| column(table, col)).collect();
            Deps {
                creates: [vec![constraint(table, &c.name)], constraint_index(table, c, &c.name)].concat(),
                // Added once its columns, and those a foreign key references, have their final type
                // and nullability.
                alters: [columns.clone(), referenced_column_types(c)].concat(),
                requires: [vec![in_table(table)], columns, referenced(c)].concat(),
                ..Deps::default()
            }
        }
        Change::DropConstraint { table, constraint: c } => {
            let columns: Vec<Object> = c.columns.iter().map(|col| column(table, col)).collect();
            Deps {
                drops: [vec![constraint(table, &c.name)], constraint_index(table, c, &c.name)].concat(),
                // Dropped before the columns it uses, or a foreign key references, change type.
                alters: [columns.clone(), referenced_column_types(c)].concat(),
                releases: [vec![in_table(table)], columns, referenced(c)].concat(),
                ..Deps::default()
            }
        }
        Change::RenameConstraint { table, from, to } => Deps {
            // The constraint's index is renamed with it.
            drops: [vec![constraint(table, from)], constraint_index(table, to, from)].concat(),
            creates: [vec![constraint(table, &to.name)], constraint_index(table, to, &to.name)].concat(),
            requires: vec![in_table(table)],
            ..Deps::default()
        },
        Change::AlterConstraint { table, to } => {
            Deps { alters: vec![constraint(table, &to.name)], requires: vec![in_table(table)], ..Deps::default() }
        }
        Change::CreateIndex { table, index } => {
            let columns = index.columns.iter().map(|col| column(table, col));
            Deps {
                creates: vec![index_object(table, &index.name)],
                requires: std::iter::once(in_table(table)).chain(columns).collect(),
                ..Deps::default()
            }
        }
        Change::DropIndex { table, index } => {
            let columns = index.columns.iter().map(|col| column(table, col));
            Deps {
                drops: vec![index_object(table, &index.name)],
                releases: std::iter::once(in_table(table)).chain(columns).collect(),
                ..Deps::default()
            }
        }
        Change::SetComment { target, .. } => {
            // A comment only waits for the object (and a column's table) to exist.
            let requires = match target {
                CommentTarget::Schema(s) => vec![Object::Schema(s.clone())],
                CommentTarget::Table(t) => vec![in_table(t)],
                CommentTarget::Column { table: t, column: c } => vec![in_table(t), column(t, c)],
                CommentTarget::Type(t) => vec![Object::Type(t.clone())],
                CommentTarget::Sequence(s) => vec![Object::Sequence(s.clone())],
                CommentTarget::Constraint { table: t, name: c } => vec![in_table(t), constraint(t, c)],
                CommentTarget::Index { table: t, name: i } => vec![in_table(t), index_object(t, i)],
            };
            Deps { requires, ..Deps::default() }
        }
        Change::SetDefault { table, column: c, sequences: used, old_sequences, .. } => Deps {
            alters: vec![column(table, c)],
            requires: [vec![in_table(table)], sequences(used)].concat(),
            releases: sequences(old_sequences.iter().filter(|s| !used.contains(s))),
            ..Deps::default()
        },
        Change::DropDefault { table, column: c, sequences: used } => {
            Deps { alters: vec![column(table, c)], requires: vec![in_table(table)], releases: sequences(used), ..Deps::default() }
        }
        Change::SetNotNull { table, column: c } | Change::DropNotNull { table, column: c } => {
            Deps { alters: vec![column(table, c)], requires: vec![in_table(table)], ..Deps::default() }
        }
    }
}

fn constraint(table: &QualName, name: &str) -> Object {
    Object::Constraint(table.clone(), name.to_owned())
}

/// An index in `table`'s schema.
fn index_object(table: &QualName, name: &str) -> Object {
    Object::Index(QualName::new(table.schema.clone(), name))
}

/// The index a primary key, unique or exclusion constraint owns, under `name`; none for others.
fn constraint_index(table: &QualName, c: &Constraint, name: &str) -> Vec<Object> {
    match c.kind {
        ConstraintKind::PrimaryKey | ConstraintKind::Unique | ConstraintKind::Exclusion => vec![index_object(table, name)],
        ConstraintKind::Check | ConstraintKind::ForeignKey => Vec::new(),
    }
}

/// A table's indexes: its own and its constraints'.
fn table_indexes(t: &pgmig_model::Table) -> Vec<Object> {
    let mut out: Vec<Object> = t.constraints.iter().flat_map(|c| constraint_index(&t.name, c, &c.name)).collect();
    out.extend(t.indexes.iter().map(|i| index_object(&t.name, &i.name)));
    out
}

/// What a foreign key references: the table, its columns and the constraint or index whose index
/// it uses (by name, both). Empty for other constraints.
fn referenced(c: &Constraint) -> Vec<Object> {
    let Some(r) = &c.references else { return Vec::new() };
    let mut out = vec![Object::Table(r.table.clone())];
    out.extend(r.columns.iter().map(|col| Object::Column(r.table.clone(), col.clone())));
    out.extend(r.constraint.iter().map(|name| constraint(&r.table, name)));
    out.extend(r.constraint.iter().map(|name| index_object(&r.table, name)));
    out
}

/// The types of the columns a foreign key references: it is dropped before they change and added
/// after (see [`Object::ColumnType`]).
fn referenced_column_types(c: &Constraint) -> Vec<Object> {
    let Some(r) = &c.references else { return Vec::new() };
    r.columns.iter().map(|col| Object::ColumnType(r.table.clone(), col.clone())).collect()
}

/// Whether `o` is `table` or one of its columns or constraints.
fn is_of_table(o: &Object, table: &QualName) -> bool {
    match o {
        Object::Table(t) | Object::Column(t, _) | Object::Constraint(t, _) => t == table,
        _ => false,
    }
}

/// The user-defined types among column type references.
fn types<'a>(refs: impl IntoIterator<Item = &'a Option<QualName>>) -> Vec<Object> {
    refs.into_iter().flatten().map(|t| Object::Type(t.clone())).collect()
}

fn sequences<'a>(names: impl IntoIterator<Item = &'a QualName>) -> Vec<Object> {
    names.into_iter().map(|s| Object::Sequence(s.clone())).collect()
}

/// An identity column's sequence, by the name the catalog gave it ([`Identity::sequence_name`]).
/// Only used to order the identity against a drop or create of a sequence holding that name.
fn identity_sequence(table: &QualName, column: &str, identity: &Identity) -> Object {
    Object::Sequence(identity.sequence_name(table, column))
}

/// `CREATE SEQUENCE` / identity options that differ from Postgres' defaults for the type and
/// direction; every option when the type has no known defaults.
fn creation_options(data_type: &str, o: &SequenceOptions) -> Vec<String> {
    let Some(d) = SequenceOptions::defaults(data_type, o.increment) else {
        return option_clauses(None, o, true);
    };
    let mut out = Vec::new();
    if o.increment != 1 {
        out.push(format!("INCREMENT BY {}", o.increment));
    }
    if o.min != d.min {
        out.push(format!("MINVALUE {}", o.min));
    }
    if o.max != d.max {
        out.push(format!("MAXVALUE {}", o.max));
    }
    // Postgres starts at the minimum (ascending) or the maximum (descending) given.
    if o.start != if o.increment > 0 { o.min } else { o.max } {
        out.push(format!("START WITH {}", o.start));
    }
    if o.cache != d.cache {
        out.push(format!("CACHE {}", o.cache));
    }
    if o.cycle {
        out.push("CYCLE".to_owned());
    }
    out
}

/// The options that differ between `old` and `new` (all of them without `old`); `bounds` forces
/// `MINVALUE` and `MAXVALUE`.
fn option_clauses(old: Option<&SequenceOptions>, new: &SequenceOptions, bounds: bool) -> Vec<String> {
    let changed = |f: fn(&SequenceOptions) -> i64| old.is_none_or(|o| f(o) != f(new));
    let mut out = Vec::new();
    if changed(|o| o.increment) {
        out.push(format!("INCREMENT BY {}", new.increment));
    }
    if bounds || changed(|o| o.min) {
        out.push(format!("MINVALUE {}", new.min));
    }
    if bounds || changed(|o| o.max) {
        out.push(format!("MAXVALUE {}", new.max));
    }
    if changed(|o| o.start) {
        out.push(format!("START WITH {}", new.start));
    }
    if changed(|o| o.cache) {
        out.push(format!("CACHE {}", new.cache));
    }
    if old.is_none_or(|o| o.cycle != new.cycle) {
        out.push(if new.cycle { "CYCLE" } else { "NO CYCLE" }.to_owned());
    }
    out
}

fn generated(g: Generated) -> &'static str {
    match g {
        Generated::Always => "ALWAYS",
        Generated::ByDefault => "BY DEFAULT",
    }
}

/// `GENERATED ... AS IDENTITY [(options)]` for a column of `type_name`.
fn identity_clause(type_name: &str, identity: &Identity) -> String {
    let options = creation_options(type_name, &identity.options);
    let mut s = format!("GENERATED {} AS IDENTITY", generated(identity.generated));
    if !options.is_empty() {
        s.push_str(&format!(" ({})", options.join(" ")));
    }
    s
}

fn statement(change: &Change, opts: &PlanOptions) -> String {
    let n = |q: &QualName| opts.name(q);
    match change {
        Change::CreateSchema(name) => format!("CREATE SCHEMA {}", quote_ident(name)),
        Change::DropSchema(name) => format!("DROP SCHEMA {}", quote_ident(name)),
        Change::CreateTable(t) => {
            let mut cols: Vec<String> = t.columns.iter().map(|c| format!("    {}", column_def(c))).collect();
            cols.extend(t.constraints.iter().map(|c| format!("    {}", constraint_def(c))));
            if cols.is_empty() {
                format!("CREATE TABLE {} ()", n(&t.name))
            } else {
                format!("CREATE TABLE {} (\n{}\n)", n(&t.name), cols.join(",\n"))
            }
        }
        Change::DropTable { table: t, .. } => format!("DROP TABLE {}", n(&t.name)),
        Change::CreateEnum(e) => {
            let labels: Vec<String> = e.labels.iter().map(|l| quote_literal(l)).collect();
            format!("CREATE TYPE {} AS ENUM ({})", n(&e.name), labels.join(", "))
        }
        Change::DropEnum(e) => format!("DROP TYPE {}", n(&e.name)),
        Change::CreateSequence(seq) => {
            let mut s = format!("CREATE SEQUENCE {}", n(&seq.name));
            if seq.data_type != "bigint" {
                s.push_str(&format!(" AS {}", seq.data_type));
            }
            for o in creation_options(&seq.data_type, &seq.options) {
                s.push(' ');
                s.push_str(&o);
            }
            s
        }
        Change::DropSequence(seq) => format!("DROP SEQUENCE {}", n(&seq.name)),
        Change::AlterSequence { from, to } => {
            let retyped = from.data_type != to.data_type;
            let mut s = format!("ALTER SEQUENCE {}", n(&to.name));
            if retyped {
                s.push_str(&format!(" AS {}", to.data_type));
            }
            for o in option_clauses(Some(&from.options), &to.options, retyped) {
                s.push(' ');
                s.push_str(&o);
            }
            s
        }
        Change::SetSequenceOwner { name, owned_by, .. } => match owned_by {
            Some(o) => format!("ALTER SEQUENCE {} OWNED BY {}.{}", n(name), n(&o.table), quote_ident(&o.column)),
            None => format!("ALTER SEQUENCE {} OWNED BY NONE", n(name)),
        },
        Change::AddIdentity { table, column, type_name, identity } => {
            format!("ALTER TABLE {} ALTER COLUMN {} ADD {}", n(table), quote_ident(column), identity_clause(type_name, identity))
        }
        Change::DropIdentity { table, column, .. } => {
            format!("ALTER TABLE {} ALTER COLUMN {} DROP IDENTITY", n(table), quote_ident(column))
        }
        Change::AlterIdentity { table, column, from, to, type_changed, .. } => {
            let mut parts = Vec::new();
            if from.generated != to.generated {
                parts.push(format!("SET GENERATED {}", generated(to.generated)));
            }
            parts.extend(option_clauses(Some(&from.options), &to.options, *type_changed).into_iter().map(|o| format!("SET {o}")));
            format!("ALTER TABLE {} ALTER COLUMN {} {}", n(table), quote_ident(column), parts.join(" "))
        }
        Change::AddEnumValue { name, value, position } => {
            let at = match position {
                EnumPosition::Before(l) => format!(" BEFORE {}", quote_literal(l)),
                EnumPosition::After(l) => format!(" AFTER {}", quote_literal(l)),
                EnumPosition::End => String::new(),
            };
            format!("ALTER TYPE {} ADD VALUE {}{at}", n(name), quote_literal(value))
        }
        Change::RenameEnumValue { name, from, to } => {
            // The catalog cannot tell a rename from removing one value and adding another, so say
            // what this does to stored data.
            format!(
                "-- renames a value: rows holding {} will hold {}\nALTER TYPE {} RENAME VALUE {} TO {}",
                quote_literal(from),
                quote_literal(to),
                n(name),
                quote_literal(from),
                quote_literal(to)
            )
        }
        Change::UnsupportedEnumChange { name, .. } => format!("-- unsupported change to enum {}", n(name)),
        Change::AddColumn { table, column } => format!("ALTER TABLE {} ADD COLUMN {}", n(table), column_def(column)),
        Change::DropColumn { table, column, .. } => format!("ALTER TABLE {} DROP COLUMN {}", n(table), quote_ident(column)),
        Change::AlterColumnType { table, column, type_name, using, .. } => {
            let using = using.as_ref().map(|u| format!(" USING {u}")).unwrap_or_default();
            format!("ALTER TABLE {} ALTER COLUMN {} TYPE {type_name}{using}", n(table), quote_ident(column))
        }
        Change::SetNotNull { table, column } => format!("ALTER TABLE {} ALTER COLUMN {} SET NOT NULL", n(table), quote_ident(column)),
        Change::DropNotNull { table, column } => format!("ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL", n(table), quote_ident(column)),
        Change::SetDefault { table, column, default, .. } => {
            format!("ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {default}", n(table), quote_ident(column))
        }
        Change::DropDefault { table, column, .. } => format!("ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT", n(table), quote_ident(column)),
        Change::AddConstraint { table, constraint } => format!("ALTER TABLE {} ADD {}", n(table), constraint_def(constraint)),
        Change::DropConstraint { table, constraint } => {
            format!("ALTER TABLE {} DROP CONSTRAINT {}", n(table), quote_ident(&constraint.name))
        }
        Change::RenameConstraint { table, from, to } => {
            format!("ALTER TABLE {} RENAME CONSTRAINT {} TO {}", n(table), quote_ident(from), quote_ident(&to.name))
        }
        Change::AlterConstraint { table, to } => {
            let timing = match (to.deferrable, to.deferred) {
                (false, _) => "NOT DEFERRABLE",
                (true, false) => "DEFERRABLE INITIALLY IMMEDIATE",
                (true, true) => "DEFERRABLE INITIALLY DEFERRED",
            };
            format!("ALTER TABLE {} ALTER CONSTRAINT {} {timing}", n(table), quote_ident(&to.name))
        }
        Change::CreateIndex { index, .. } => index.definition.clone(),
        Change::DropIndex { table, index } => format!("DROP INDEX {}", n(&QualName::new(table.schema.clone(), index.name.clone()))),
        Change::SetComment { target, comment } => {
            let object = match target {
                CommentTarget::Schema(s) => format!("SCHEMA {}", quote_ident(s)),
                CommentTarget::Table(t) => format!("TABLE {}", n(t)),
                CommentTarget::Column { table: t, column: c } => format!("COLUMN {}.{}", n(t), quote_ident(c)),
                CommentTarget::Type(t) => format!("TYPE {}", n(t)),
                CommentTarget::Sequence(s) => format!("SEQUENCE {}", n(s)),
                CommentTarget::Constraint { table: t, name: c } => format!("CONSTRAINT {} ON {}", quote_ident(c), n(t)),
                CommentTarget::Index { table: t, name: i } => format!("INDEX {}", n(&QualName::new(t.schema.clone(), i.clone()))),
            };
            format!("COMMENT ON {object} IS {}", comment.as_deref().map_or("NULL".to_owned(), quote_literal))
        }
    }
}

/// `CONSTRAINT <name> <definition>`, as written in `CREATE TABLE` and `ADD`.
fn constraint_def(c: &Constraint) -> String {
    format!("CONSTRAINT {} {}", quote_ident(&c.name), c.definition)
}

fn column_def(c: &Column) -> String {
    let mut s = format!("{} {}", quote_ident(&c.name), c.type_name);
    if let Some(identity) = &c.identity {
        s.push(' ');
        s.push_str(&identity_clause(&c.type_name, identity));
    }
    if let Some(d) = &c.default {
        s.push_str(" DEFAULT ");
        s.push_str(d);
    }
    if c.not_null {
        s.push_str(" NOT NULL");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgmig_model::{Enum, Sequence, Table};

    /// The JSON of a plan is part of pgmig's interface: a change here must be deliberate.
    #[test]
    fn plan_json_is_pinned_and_round_trips() {
        let plan = Plan { statements: vec!["ALTER TYPE e ADD VALUE 'b'".into(), "CREATE TABLE t (c e)".into()], commit_after: Some(0) };
        let json = serde_json::to_string(&plan).expect("serialize");
        assert_eq!(json, r#"{"statements":["ALTER TYPE e ADD VALUE 'b'","CREATE TABLE t (c e)"],"commit_after":0}"#);
        assert_eq!(serde_json::from_str::<Plan>(&json).expect("deserialize"), plan);
        let empty = Plan::default();
        assert_eq!(serde_json::to_string(&empty).expect("serialize"), r#"{"statements":[],"commit_after":null}"#);
        assert_eq!(serde_json::from_str::<Plan>(r#"{"statements":[],"commit_after":null}"#).expect("deserialize"), empty);
    }

    fn col(name: &str, type_ref: Option<QualName>) -> Column {
        let type_name = type_ref.as_ref().map_or("integer".to_owned(), |t| t.name.clone());
        Column {
            name: name.into(),
            type_name,
            type_ref,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: Vec::new(),
            identity: None,
        }
    }

    fn table(schema: &str, name: &str, columns: Vec<Column>) -> Table {
        Table { name: QualName::new(schema, name), columns, comment: None, constraints: vec![], indexes: vec![] }
    }

    fn row_type(schema: &str, name: &str) -> Option<QualName> {
        Some(QualName::new(schema, name))
    }

    fn drop_table(table: Table) -> Change {
        Change::DropTable { table, owned_sequences: vec![] }
    }

    fn sql(changes: &[Change]) -> Vec<String> {
        plan(changes).expect("plan").statements
    }

    #[test]
    fn schema_is_created_before_its_table_and_dropped_after() {
        let t = table("app", "t", vec![col("id", None)]);
        let created = sql(&[Change::CreateTable(t.clone()), Change::CreateSchema("app".into())]);
        assert_eq!(created, ["CREATE SCHEMA app", "CREATE TABLE app.t (\n    id integer\n)"]);
        let dropped = sql(&[Change::DropSchema("app".into()), drop_table(t)]);
        assert_eq!(dropped, ["DROP TABLE app.t", "DROP SCHEMA app"]);
    }

    #[test]
    fn a_table_using_another_tables_row_type_is_created_after_it_and_dropped_before_it() {
        // Name order (and the order the diff emits) is the wrong one in both directions.
        let path = table("public", "apath", vec![col("start", row_type("public", "zpoint"))]);
        let point = table("public", "zpoint", vec![col("x", None)]);
        let created = sql(&[Change::CreateTable(path.clone()), Change::CreateTable(point.clone())]);
        assert_eq!(created, ["CREATE TABLE public.zpoint (\n    x integer\n)", "CREATE TABLE public.apath (\n    start zpoint\n)"]);

        let point = table("public", "apoint", vec![col("x", None)]);
        let path = table("public", "zpath", vec![col("start", row_type("public", "apoint"))]);
        let dropped = sql(&[drop_table(point), drop_table(path)]);
        assert_eq!(dropped, ["DROP TABLE public.zpath", "DROP TABLE public.apoint"]);
    }

    #[test]
    fn column_changes_wait_for_the_types_they_use() {
        let t = QualName::new("public", "t");
        let new = table("public", "znew", vec![col("x", None)]);
        let old = table("public", "aold", vec![col("x", None)]);
        let changes = [
            drop_table(old),
            Change::DropColumn {
                table: t.clone(),
                column: "gone".into(),
                type_ref: row_type("public", "aold"),
                sequences: vec![],
                owned_sequences: vec![],
            },
            Change::AlterColumnType {
                table: t.clone(),
                column: "c".into(),
                type_name: "znew".into(),
                type_ref: row_type("public", "znew"),
                old_type_ref: row_type("public", "aold"),
                using: None,
            },
            Change::AddColumn { table: t, column: col("added", row_type("public", "znew")) },
            Change::CreateTable(new),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE TABLE public.znew (\n    x integer\n)",
                "ALTER TABLE public.t ADD COLUMN added znew",
                "ALTER TABLE public.t ALTER COLUMN c TYPE znew",
                "ALTER TABLE public.t DROP COLUMN gone",
                "DROP TABLE public.aold",
            ]
        );
    }

    #[test]
    fn a_type_change_with_using_runs_between_dropping_and_setting_the_default() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault { table: t.clone(), column: "n".into(), default: "1".into(), sequences: vec![], old_sequences: vec![] },
            Change::AlterColumnType {
                table: t.clone(),
                column: "n".into(),
                type_name: "integer".into(),
                type_ref: None,
                old_type_ref: None,
                using: Some("n::integer".into()),
            },
            Change::DropDefault { table: t, column: "n".into(), sequences: vec![] },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ALTER COLUMN n DROP DEFAULT",
                "ALTER TABLE public.t ALTER COLUMN n TYPE integer USING n::integer",
                "ALTER TABLE public.t ALTER COLUMN n SET DEFAULT 1",
            ]
        );
    }

    #[test]
    fn unrelated_statements_keep_phase_then_diff_order() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::DropSchema("old".into()),
            Change::SetNotNull { table: t.clone(), column: "a".into() },
            Change::CreateTable(table("public", "b", vec![])),
            Change::DropDefault { table: t.clone(), column: "a".into(), sequences: vec![] },
            Change::CreateTable(table("public", "a", vec![])),
            Change::CreateSchema("new".into()),
        ];
        let expected = [
            "CREATE SCHEMA new",
            "CREATE TABLE public.b ()",
            "CREATE TABLE public.a ()",
            "ALTER TABLE public.t ALTER COLUMN a DROP DEFAULT",
            "ALTER TABLE public.t ALTER COLUMN a SET NOT NULL",
            "DROP SCHEMA old",
        ];
        assert_eq!(sql(&changes), expected);
        assert_eq!(sql(&changes), expected, "planning twice gives the same order");
    }

    #[test]
    fn a_cycle_is_reported_with_its_statements() {
        let a = table("public", "a", vec![col("b", row_type("public", "b"))]);
        let b = table("public", "b", vec![col("a", row_type("public", "a"))]);
        let err = plan(&[Change::CreateTable(a), Change::CreateTable(b)]).expect_err("cycle");
        let Error::Cycle(statements) = &err else { panic!("expected a cycle: {err}") };
        assert_eq!(statements.len(), 2);
        assert!(err.to_string().starts_with("the changes depend on each other in a cycle: CREATE TABLE"), "{err}");
    }

    #[test]
    fn comments_follow_the_objects_they_describe_and_are_quoted() {
        let t = QualName::new("app", "t");
        let mut created = table("app", "t", vec![col("id", None)]);
        created.comment = Some("it's".into());
        let changes = [
            Change::SetComment { target: CommentTarget::Column { table: t.clone(), column: "id".into() }, comment: Some("a\\b\nc".into()) },
            Change::SetComment { target: CommentTarget::Table(t.clone()), comment: Some("it's".into()) },
            Change::SetComment { target: CommentTarget::Schema("app".into()), comment: None },
            Change::CreateTable(created),
            Change::CreateSchema("app".into()),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE SCHEMA app",
                "CREATE TABLE app.t (\n    id integer\n)",
                "COMMENT ON COLUMN app.t.id IS E'a\\\\b\nc'",
                "COMMENT ON TABLE app.t IS 'it''s'",
                "COMMENT ON SCHEMA app IS NULL",
            ]
        );
    }

    fn mood(labels: &[&str]) -> Enum {
        Enum { name: QualName::new("app", "mood"), labels: labels.iter().map(|l| l.to_string()).collect(), comment: None }
    }

    fn add(value: &str, position: EnumPosition) -> Change {
        Change::AddEnumValue { name: QualName::new("app", "mood"), value: value.into(), position }
    }

    #[test]
    fn an_enum_is_created_before_its_columns_and_dropped_after_them() {
        let m = row_type("app", "mood");
        let t = QualName::new("public", "t");
        let users = table("public", "users", vec![col("m", m.clone())]);
        let created = sql(&[
            Change::AddColumn { table: t.clone(), column: col("m", m.clone()) },
            Change::CreateTable(users.clone()),
            Change::SetComment { target: CommentTarget::Type(QualName::new("app", "mood")), comment: Some("it's".into()) },
            Change::CreateEnum(mood(&["a", "it's"])),
            Change::CreateSchema("app".into()),
        ]);
        assert_eq!(
            created,
            [
                "CREATE SCHEMA app",
                "CREATE TYPE app.mood AS ENUM ('a', 'it''s')",
                "CREATE TABLE public.users (\n    m mood\n)",
                "ALTER TABLE public.t ADD COLUMN m mood",
                "COMMENT ON TYPE app.mood IS 'it''s'",
            ]
        );
        let dropped = sql(&[
            Change::DropSchema("app".into()),
            Change::DropEnum(mood(&["a"])),
            drop_table(users),
            Change::DropColumn { table: t, column: "m".into(), type_ref: m, sequences: vec![], owned_sequences: vec![] },
        ]);
        assert_eq!(dropped, ["ALTER TABLE public.t DROP COLUMN m", "DROP TABLE public.users", "DROP TYPE app.mood", "DROP SCHEMA app"]);
    }

    #[test]
    fn added_values_come_first_in_diff_order_and_are_committed_before_the_rest() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault {
                table: t.clone(),
                column: "m".into(),
                default: "'b'::app.mood".into(),
                sequences: vec![],
                old_sequences: vec![],
            },
            add("a", EnumPosition::Before("c".into())),
            Change::CreateSchema("other".into()),
            add("b", EnumPosition::After("a".into())),
            add("z", EnumPosition::End),
        ];
        let p = plan(&changes).expect("plan");
        assert_eq!(
            p.statements,
            [
                "ALTER TYPE app.mood ADD VALUE 'a' BEFORE 'c'",
                "ALTER TYPE app.mood ADD VALUE 'b' AFTER 'a'",
                "ALTER TYPE app.mood ADD VALUE 'z'",
                "CREATE SCHEMA other",
                "ALTER TABLE public.t ALTER COLUMN m SET DEFAULT 'b'::app.mood",
            ]
        );
        assert_eq!(p.commit_after, Some(2));
        assert_eq!(p.batches().len(), 2);
        assert!(p.batches()[1].starts_with("CREATE SCHEMA other;\n"), "{:?}", p.batches());
        assert_eq!(p.to_sql(), p.batches().join(COMMIT_MARKER));

        let only_adds = plan(&changes[1..2]).expect("plan");
        assert_eq!(only_adds.commit_after, None);
        assert_eq!(only_adds.to_sql(), "ALTER TYPE app.mood ADD VALUE 'a' BEFORE 'c';\n");
    }

    #[test]
    fn a_renamed_value_runs_before_defaults_that_use_it() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault {
                table: t,
                column: "m".into(),
                default: "'new'::app.mood".into(),
                sequences: vec![],
                old_sequences: vec![],
            },
            Change::RenameEnumValue { name: QualName::new("app", "mood"), from: "it's".into(), to: "new".into() },
        ];
        let p = plan(&changes).expect("plan");
        assert_eq!(
            p.statements,
            [
                "-- renames a value: rows holding 'it''s' will hold 'new'\nALTER TYPE app.mood RENAME VALUE 'it''s' TO 'new'",
                "ALTER TABLE public.t ALTER COLUMN m SET DEFAULT 'new'::app.mood"
            ]
        );
        assert_eq!(p.commit_after, None);
    }

    #[test]
    fn removing_or_reordering_values_is_an_unsupported_change_error() {
        let changes = [
            Change::CreateSchema("app".into()),
            Change::UnsupportedEnumChange { name: QualName::new("app", "mood"), from: vec!["a".into(), "b".into()], to: vec!["b".into()] },
        ];
        let err = plan(&changes).expect_err("unsupported");
        assert_eq!(
            err.to_string(),
            "unsupported changes: enum app.mood: values ('a', 'b') cannot become ('b'); only adding values and renaming one value \
             in place are supported, not removing or reordering values"
        );
    }

    fn opts(data_type: &str, increment: i64) -> SequenceOptions {
        SequenceOptions::defaults(data_type, increment).expect("integer type")
    }

    fn sequence(name: &str, data_type: &str, options: SequenceOptions) -> Sequence {
        Sequence { name: QualName::new("public", name), data_type: data_type.into(), options, owned_by: None, comment: None }
    }

    #[test]
    fn create_sequence_prints_only_options_that_differ_from_the_defaults() {
        let create = |data_type: &str, options| sql(&[Change::CreateSequence(sequence("s", data_type, options))]);
        assert_eq!(create("bigint", opts("bigint", 1)), ["CREATE SEQUENCE public.s"]);
        assert_eq!(create("integer", opts("integer", -1)), ["CREATE SEQUENCE public.s AS integer INCREMENT BY -1"]);
        let custom = SequenceOptions { start: 5, increment: 2, min: 3, max: 100, cache: 4, cycle: true };
        assert_eq!(
            create("smallint", custom),
            ["CREATE SEQUENCE public.s AS smallint INCREMENT BY 2 MINVALUE 3 MAXVALUE 100 START WITH 5 CACHE 4 CYCLE"]
        );
        // The start follows the minimum given.
        assert_eq!(create("bigint", SequenceOptions { start: 3, min: 3, ..opts("bigint", 1) }), ["CREATE SEQUENCE public.s MINVALUE 3"]);
    }

    #[test]
    fn alter_sequence_prints_changed_options_and_both_bounds_on_a_new_type() {
        let from = sequence("s", "smallint", opts("smallint", 1));
        let to = sequence("s", "smallint", SequenceOptions { cache: 10, cycle: true, ..opts("smallint", 1) });
        assert_eq!(sql(&[Change::AlterSequence { from: from.clone(), to }]), ["ALTER SEQUENCE public.s CACHE 10 CYCLE"]);
        let to = sequence("s", "integer", opts("integer", 1));
        assert_eq!(
            sql(&[Change::AlterSequence { from: from.clone(), to }]),
            ["ALTER SEQUENCE public.s AS integer MINVALUE 1 MAXVALUE 2147483647"]
        );
        let back = sequence("s", "smallint", SequenceOptions { cycle: false, ..opts("smallint", 1) });
        let cycling = sequence("s", "smallint", SequenceOptions { cycle: true, ..opts("smallint", 1) });
        assert_eq!(sql(&[Change::AlterSequence { from: cycling, to: back }]), ["ALTER SEQUENCE public.s NO CYCLE"]);
    }

    fn serial_column(seq: &str) -> Column {
        Column {
            default: Some(format!("nextval('{seq}'::regclass)")),
            default_sequences: vec![QualName::new("public", seq)],
            not_null: true,
            ..col("id", None)
        }
    }

    #[test]
    fn a_serial_table_creates_its_sequence_first_and_owns_it_last() {
        let t = QualName::new("public", "t");
        let owned = Some(OwnedBy { table: t.clone(), column: "id".into() });
        let changes = [
            Change::SetSequenceOwner { name: QualName::new("public", "t_id_seq"), owned_by: owned, old: None },
            Change::CreateTable(table("public", "t", vec![serial_column("t_id_seq")])),
            Change::CreateSequence(sequence("t_id_seq", "integer", opts("integer", 1))),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE SEQUENCE public.t_id_seq AS integer",
                "CREATE TABLE public.t (\n    id integer DEFAULT nextval('t_id_seq'::regclass) NOT NULL\n)",
                "ALTER SEQUENCE public.t_id_seq OWNED BY public.t.id",
            ]
        );
    }

    #[test]
    fn serial_to_identity_drops_the_default_then_the_sequence_then_adds_the_identity() {
        let t = QualName::new("public", "t");
        let seq = QualName::new("public", "t_id_seq");
        let identity = Identity { generated: Generated::Always, options: opts("integer", 1), sequence: None };
        let changes = [
            Change::AddIdentity { table: t.clone(), column: "id".into(), type_name: "integer".into(), identity },
            Change::DropSequence(sequence("t_id_seq", "integer", opts("integer", 1))),
            Change::DropDefault { table: t.clone(), column: "id".into(), sequences: vec![seq] },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ALTER COLUMN id DROP DEFAULT",
                "DROP SEQUENCE public.t_id_seq",
                "ALTER TABLE public.t ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY",
            ]
        );
    }

    #[test]
    fn identity_to_serial_drops_the_identity_before_creating_the_sequence() {
        let t = QualName::new("public", "t");
        let seq = QualName::new("public", "t_id_seq");
        let changes = [
            Change::CreateSequence(sequence("t_id_seq", "integer", opts("integer", 1))),
            Change::SetDefault {
                table: t.clone(),
                column: "id".into(),
                default: "nextval('t_id_seq'::regclass)".into(),
                sequences: vec![seq.clone()],
                old_sequences: vec![],
            },
            Change::SetSequenceOwner { name: seq, owned_by: Some(OwnedBy { table: t.clone(), column: "id".into() }), old: None },
            Change::DropIdentity { table: t, column: "id".into(), sequence: QualName::new("public", "t_id_seq") },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ALTER COLUMN id DROP IDENTITY",
                "CREATE SEQUENCE public.t_id_seq AS integer",
                "ALTER TABLE public.t ALTER COLUMN id SET DEFAULT nextval('t_id_seq'::regclass)",
                "ALTER SEQUENCE public.t_id_seq OWNED BY public.t.id",
            ]
        );
    }

    #[test]
    fn identity_changes_set_what_changed() {
        let t = QualName::new("public", "t");
        let always = Identity { generated: Generated::Always, options: opts("integer", 1), sequence: None };
        let by_default =
            Identity { generated: Generated::ByDefault, options: SequenceOptions { start: 7, ..opts("integer", 1) }, sequence: None };
        let alter = |from: &Identity, to: &Identity, type_changed| Change::AlterIdentity {
            table: t.clone(),
            column: "id".into(),
            type_name: "bigint".into(),
            from: from.clone(),
            to: to.clone(),
            type_changed,
        };
        assert_eq!(
            sql(&[alter(&always, &by_default, false)]),
            ["ALTER TABLE public.t ALTER COLUMN id SET GENERATED BY DEFAULT SET START WITH 7"]
        );
        let wide = Identity { options: opts("bigint", 1), ..always.clone() };
        assert_eq!(
            sql(&[alter(&always, &wide, true)]),
            ["ALTER TABLE public.t ALTER COLUMN id SET MINVALUE 1 SET MAXVALUE 9223372036854775807"]
        );
        let mut c = col("id", None);
        c.identity = Some(Identity { options: SequenceOptions { start: 10, increment: 5, ..opts("integer", 5) }, ..always });
        c.not_null = true;
        assert_eq!(
            sql(&[Change::AddColumn { table: t.clone(), column: c }]),
            ["ALTER TABLE public.t ADD COLUMN id integer GENERATED ALWAYS AS IDENTITY (INCREMENT BY 5 START WITH 10) NOT NULL"]
        );
        assert_eq!(
            sql(&[Change::DropIdentity { table: t, column: "id".into(), sequence: QualName::new("public", "t_id_seq") }]),
            ["ALTER TABLE public.t ALTER COLUMN id DROP IDENTITY"]
        );
    }

    #[test]
    fn dropping_a_table_runs_before_dropping_a_sequence_its_defaults_use() {
        let changes = [
            Change::DropSequence(sequence("free", "bigint", opts("bigint", 1))),
            drop_table(table("public", "t", vec![serial_column("free")])),
            Change::SetComment { target: CommentTarget::Sequence(QualName::new("public", "kept")), comment: Some("c".into()) },
        ];
        assert_eq!(sql(&changes), ["DROP TABLE public.t", "DROP SEQUENCE public.free", "COMMENT ON SEQUENCE public.kept IS 'c'"]);
    }

    #[test]
    fn dropping_the_owner_of_a_shared_sequence_runs_after_what_still_uses_it() {
        // t.id is serial; z.x takes values from t's sequence too. Dropping t.id drops the
        // sequence, so z (and its default) must go first (2BP01 otherwise).
        let t = QualName::new("public", "t");
        let seq = QualName::new("public", "t_id_seq");
        let mut x = serial_column("t_id_seq");
        x.name = "x".into();
        let z = table("public", "z", vec![x]);
        let drop_id = Change::DropColumn {
            table: t.clone(),
            column: "id".into(),
            type_ref: None,
            sequences: vec![seq.clone()],
            owned_sequences: vec![seq.clone()],
        };
        assert_eq!(sql(&[drop_id, drop_table(z.clone())]), ["DROP TABLE public.z", "ALTER TABLE public.t DROP COLUMN id"]);

        // Both tables dropped, the owner first in diff order.
        let owner = Change::DropTable { table: table("public", "t", vec![serial_column("t_id_seq")]), owned_sequences: vec![seq] };
        assert_eq!(sql(&[owner, drop_table(z)]), ["DROP TABLE public.z", "DROP TABLE public.t"]);
    }

    #[test]
    fn identity_to_serial_orders_by_the_identity_sequences_real_name() {
        // Postgres truncates `<table>_<column>_seq` to 63 bytes; the catalog's name is what counts.
        let long = "a".repeat(40);
        let t = QualName::new("public", long.clone());
        let truncated = format!("{long}_{}", "b".repeat(18));
        assert_eq!(truncated.len(), 59);
        let name = QualName::new("public", truncated.clone());
        let changes = [
            Change::CreateSequence(sequence(&truncated, "integer", opts("integer", 1))),
            Change::DropIdentity { table: t.clone(), column: "b".repeat(30), sequence: name },
        ];
        let planned = sql(&changes);
        assert!(planned[0].ends_with("DROP IDENTITY"), "{planned:?}");
        assert!(planned[1].starts_with("CREATE SEQUENCE"), "{planned:?}");

        // Adding an identity whose sequence takes a dropped sequence's name waits for the drop.
        let identity =
            Identity { generated: Generated::Always, options: opts("integer", 1), sequence: Some(QualName::new("public", "t_id_seq1")) };
        let changes = [
            Change::AddIdentity { table: QualName::new("public", "t"), column: "id".into(), type_name: "integer".into(), identity },
            Change::DropSequence(sequence("t_id_seq1", "integer", opts("integer", 1))),
        ];
        assert_eq!(sql(&changes)[0], "DROP SEQUENCE public.t_id_seq1");
    }

    fn constraint(name: &str, kind: pgmig_model::ConstraintKind, definition: &str, columns: &[&str]) -> Constraint {
        Constraint {
            name: name.into(),
            kind,
            definition: definition.into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            references: None,
            comment: None,
        }
    }

    fn pkey(table: &str) -> Constraint {
        constraint(&format!("{table}_pkey"), pgmig_model::ConstraintKind::PrimaryKey, "PRIMARY KEY (id)", &["id"])
    }

    /// `<from>_<column>_fkey`: `from.<column>` references `to.id` through `to_pkey`.
    fn fkey(from: &str, column: &str, to: &str) -> Constraint {
        let mut c = constraint(
            &format!("{from}_{column}_fkey"),
            pgmig_model::ConstraintKind::ForeignKey,
            &format!("FOREIGN KEY ({column}) REFERENCES {to}(id)"),
            &[column],
        );
        c.references = Some(pgmig_model::References {
            table: QualName::new("public", to),
            columns: vec!["id".into()],
            constraint: Some(format!("{to}_pkey")),
        });
        c
    }

    fn keyed(name: &str, other: &str, constraints: Vec<Constraint>) -> Table {
        let mut t = table("public", name, vec![Column { not_null: true, ..col("id", None) }, col(other, None)]);
        t.constraints = constraints;
        t
    }

    #[test]
    fn tables_referencing_each_other_are_created_first_and_their_foreign_keys_added_after() {
        let changes = [
            Change::CreateTable(keyed("a", "b_id", vec![pkey("a")])),
            Change::CreateTable(keyed("b", "a_id", vec![pkey("b")])),
            Change::AddConstraint { table: QualName::new("public", "a"), constraint: fkey("a", "b_id", "b") },
            Change::AddConstraint { table: QualName::new("public", "b"), constraint: fkey("b", "a_id", "a") },
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE TABLE public.a (\n    id integer NOT NULL,\n    b_id integer,\n    CONSTRAINT a_pkey PRIMARY KEY (id)\n)",
                "CREATE TABLE public.b (\n    id integer NOT NULL,\n    a_id integer,\n    CONSTRAINT b_pkey PRIMARY KEY (id)\n)",
                "ALTER TABLE public.a ADD CONSTRAINT a_b_id_fkey FOREIGN KEY (b_id) REFERENCES b(id)",
                "ALTER TABLE public.b ADD CONSTRAINT b_a_id_fkey FOREIGN KEY (a_id) REFERENCES a(id)",
            ]
        );
    }

    #[test]
    fn a_foreign_key_waits_for_the_key_it_uses_and_goes_before_it() {
        let p = QualName::new("public", "p");
        let t = QualName::new("public", "t");
        // In diff order the foreign key comes first both ways.
        let added = sql(&[
            Change::AddConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
            Change::AddConstraint { table: p.clone(), constraint: pkey("p") },
        ]);
        assert_eq!(
            added,
            [
                "ALTER TABLE public.p ADD CONSTRAINT p_pkey PRIMARY KEY (id)",
                "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)"
            ]
        );
        let dropped = sql(&[
            Change::DropConstraint { table: p.clone(), constraint: pkey("p") },
            Change::DropConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
        ]);
        assert_eq!(dropped, ["ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey", "ALTER TABLE public.p DROP CONSTRAINT p_pkey"]);
        // A dropped table goes after the foreign keys into it, whether they are dropped on their
        // own or with their table.
        let parent = keyed("p", "x", vec![pkey("p")]);
        let child = keyed("t", "pid", vec![fkey("t", "pid", "p")]);
        assert_eq!(
            sql(&[drop_table(parent.clone()), Change::DropConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") }]),
            ["ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey", "DROP TABLE public.p"]
        );
        assert_eq!(sql(&[drop_table(parent), drop_table(child)]), ["DROP TABLE public.t", "DROP TABLE public.p"]);
        // A table referencing itself has no cycle.
        assert_eq!(sql(&[drop_table(keyed("p", "up", vec![pkey("p"), fkey("p", "up", "p")]))]), ["DROP TABLE public.p"]);
    }

    #[test]
    fn a_key_is_dropped_before_its_column_changes_and_added_after() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::DropNotNull { table: t.clone(), column: "id".into() },
            Change::AddConstraint { table: t.clone(), constraint: pkey("t") },
            Change::AlterColumnType {
                table: t.clone(),
                column: "id".into(),
                type_name: "bigint".into(),
                type_ref: None,
                old_type_ref: None,
                using: None,
            },
            Change::DropConstraint { table: t.clone(), constraint: pkey("t") },
            Change::AddColumn { table: t.clone(), column: col("x", None) },
            Change::AddConstraint {
                table: t.clone(),
                constraint: constraint("x_pos", pgmig_model::ConstraintKind::Check, "CHECK ((x > 0))", &["x"]),
            },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ADD COLUMN x integer",
                "ALTER TABLE public.t DROP CONSTRAINT t_pkey",
                "ALTER TABLE public.t ALTER COLUMN id TYPE bigint",
                "ALTER TABLE public.t ALTER COLUMN id DROP NOT NULL",
                "ALTER TABLE public.t ADD CONSTRAINT t_pkey PRIMARY KEY (id)",
                "ALTER TABLE public.t ADD CONSTRAINT x_pos CHECK ((x > 0))",
            ]
        );
    }

    #[test]
    fn a_foreign_key_is_dropped_before_the_referenced_column_changes_type_and_added_after() {
        let (p, t) = (QualName::new("public", "p"), QualName::new("public", "t"));
        let alter = |table: &QualName, column: &str| Change::AlterColumnType {
            table: table.clone(),
            column: column.into(),
            type_name: "text".into(),
            type_ref: None,
            old_type_ref: None,
            using: None,
        };
        let changes = [
            alter(&p, "id"),
            alter(&t, "pid"),
            Change::DropConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
            Change::AddConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey",
                "ALTER TABLE public.p ALTER COLUMN id TYPE text",
                "ALTER TABLE public.t ALTER COLUMN pid TYPE text",
                "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)",
            ]
        );
    }

    #[test]
    fn names_in_the_unqualified_schema_print_without_it() {
        let scoped = PlanOptions { unqualified_schema: Some("public".into()) };
        let mut t = keyed("t", "pid", vec![pkey("t")]);
        t.columns.push(col("m", row_type("app", "mood")));
        let owned = Some(OwnedBy { table: t.name.clone(), column: "id".into() });
        let changes = [
            Change::CreateTable(t.clone()),
            Change::CreateSequence(sequence("Seq", "bigint", opts("bigint", 1))),
            Change::SetSequenceOwner { name: QualName::new("public", "Seq"), owned_by: owned, old: None },
            Change::SetComment { target: CommentTarget::Column { table: t.name.clone(), column: "id".into() }, comment: None },
            Change::AddConstraint { table: t.name.clone(), constraint: fkey("t", "pid", "p") },
            Change::SetComment { target: CommentTarget::Type(QualName::new("app", "mood")), comment: None },
        ];
        assert_eq!(
            plan_with(&changes, &scoped).expect("plan").statements,
            [
                "CREATE SEQUENCE \"Seq\"",
                "CREATE TABLE t (\n    id integer NOT NULL,\n    pid integer,\n    m mood,\n    CONSTRAINT t_pkey PRIMARY KEY (id)\n)",
                "ALTER SEQUENCE \"Seq\" OWNED BY t.id",
                "ALTER TABLE t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)",
                "COMMENT ON COLUMN t.id IS NULL",
                "COMMENT ON TYPE app.mood IS NULL",
            ]
        );
    }

    #[test]
    fn objects_referenced_but_not_planned_are_assumed_to_exist() {
        // `p`, its key and the `app.mood` type are not managed: nothing creates them.
        let mut t = keyed("t", "pid", vec![]);
        t.columns.push(col("m", row_type("app", "mood")));
        let changes = [Change::AddConstraint { table: t.name.clone(), constraint: fkey("t", "pid", "p") }, Change::CreateTable(t.clone())];
        assert_eq!(sql(&changes)[1], "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)");
        assert_eq!(sql(&[drop_table(keyed("t", "pid", vec![fkey("t", "pid", "p")]))]), ["DROP TABLE public.t"]);
    }

    #[test]
    fn renames_deferrability_and_comments_on_constraints() {
        let t = QualName::new("App", "t");
        let mut deferred = fkey("t", "pid", "p");
        deferred.name = "Odd".into();
        let alter = |deferrable, initially| {
            let to = Constraint { deferrable, deferred: initially, ..deferred.clone() };
            sql(&[Change::AlterConstraint { table: t.clone(), to }])
        };
        assert_eq!(alter(true, true), ["ALTER TABLE \"App\".t ALTER CONSTRAINT \"Odd\" DEFERRABLE INITIALLY DEFERRED"]);
        assert_eq!(alter(true, false), ["ALTER TABLE \"App\".t ALTER CONSTRAINT \"Odd\" DEFERRABLE INITIALLY IMMEDIATE"]);
        assert_eq!(alter(false, false), ["ALTER TABLE \"App\".t ALTER CONSTRAINT \"Odd\" NOT DEFERRABLE"]);
        let renamed = sql(&[
            Change::SetComment { target: CommentTarget::Constraint { table: t.clone(), name: "Odd".into() }, comment: Some("it's".into()) },
            Change::RenameConstraint { table: t.clone(), from: "old".into(), to: deferred.clone() },
        ]);
        assert_eq!(
            renamed,
            ["ALTER TABLE \"App\".t RENAME CONSTRAINT old TO \"Odd\"", "COMMENT ON CONSTRAINT \"Odd\" ON \"App\".t IS 'it''s'"]
        );
    }
    fn index(table: &str, name: &str, key: &str, columns: &[&str]) -> pgmig_model::Index {
        pgmig_model::Index {
            name: name.into(),
            definition: format!("CREATE INDEX {name} ON public.{table} USING btree {key}"),
            unique: false,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            comment: None,
        }
    }

    #[test]
    fn an_index_is_created_after_its_table_and_columns_and_commented_after_that() {
        let t = QualName::new("public", "t");
        let i = index("t", "t_x_idx", "(lower(x))", &["x"]);
        let changes = [
            Change::SetComment { target: CommentTarget::Index { table: t.clone(), name: i.name.clone() }, comment: Some("it's".into()) },
            Change::CreateIndex { table: t.clone(), index: i },
            Change::AddColumn { table: t.clone(), column: col("x", None) },
            Change::CreateIndex { table: QualName::new("public", "n"), index: index("n", "n_idx", "(id)", &["id"]) },
            Change::CreateTable(table("public", "n", vec![col("id", None)])),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE TABLE public.n (\n    id integer\n)",
                "ALTER TABLE public.t ADD COLUMN x integer",
                "CREATE INDEX t_x_idx ON public.t USING btree (lower(x))",
                "CREATE INDEX n_idx ON public.n USING btree (id)",
                "COMMENT ON INDEX public.t_x_idx IS 'it''s'",
            ]
        );
        let opts = PlanOptions { unqualified_schema: Some("public".into()) };
        let drop = Change::DropIndex { table: t.clone(), index: index("t", "Odd", "(x)", &["x"]) };
        assert_eq!(plan_with(std::slice::from_ref(&drop), &opts).expect("plan").statements, ["DROP INDEX \"Odd\""]);
        assert_eq!(sql(&[drop]), ["DROP INDEX public.\"Odd\""]);
    }

    #[test]
    fn an_index_is_dropped_before_its_columns_change_or_go_and_before_its_name_is_reused() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::CreateIndex { table: t.clone(), index: index("t", "t_idx", "(y)", &["y"]) },
            Change::DropColumn { table: t.clone(), column: "x".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
            Change::AlterColumnType {
                table: t.clone(),
                column: "y".into(),
                type_name: "bigint".into(),
                type_ref: None,
                old_type_ref: None,
                using: None,
            },
            // A constraint taking the index's name waits for the drop too.
            Change::AddConstraint {
                table: t.clone(),
                constraint: constraint("t_x_key", pgmig_model::ConstraintKind::Unique, "UNIQUE (y)", &["y"]),
            },
            Change::DropIndex { table: t.clone(), index: index("t", "t_x_key", "(x)", &["x"]) },
            Change::DropIndex { table: t.clone(), index: index("t", "t_idx", "(x, y)", &["x", "y"]) },
        ];
        assert_eq!(
            sql(&changes),
            [
                "DROP INDEX public.t_x_key",
                "DROP INDEX public.t_idx",
                "ALTER TABLE public.t ALTER COLUMN y TYPE bigint",
                "CREATE INDEX t_idx ON public.t USING btree (y)",
                "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE (y)",
                "ALTER TABLE public.t DROP COLUMN x",
            ]
        );
    }

    #[test]
    fn a_foreign_key_using_a_unique_index_is_dropped_before_it_and_added_after_it() {
        let mut f = fkey("t", "pid", "p");
        f.references.as_mut().expect("references").constraint = Some("p_id_idx".into());
        let t = QualName::new("public", "t");
        let p = QualName::new("public", "p");
        let unique = pgmig_model::Index { unique: true, ..index("p", "p_id_idx", "(id)", &["id"]) };
        let changes = [
            Change::DropIndex { table: p.clone(), index: unique.clone() },
            Change::CreateIndex { table: p, index: unique },
            Change::AddConstraint { table: t.clone(), constraint: f.clone() },
            Change::DropConstraint { table: t, constraint: f },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey",
                "DROP INDEX public.p_id_idx",
                "CREATE INDEX p_id_idx ON public.p USING btree (id)",
                "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)",
            ]
        );
    }

    #[test]
    fn tables_sequences_and_indexes_reuse_each_others_names_after_the_drop() {
        let t = QualName::new("public", "t");
        let foo = || index("t", "foo", "(id)", &["id"]);
        let drop_index = || Change::DropIndex { table: t.clone(), index: foo() };
        let create_index = || Change::CreateIndex { table: t.clone(), index: foo() };
        // Index `foo` dropped, a table or sequence `foo` created.
        assert_eq!(
            sql(&[drop_index(), Change::CreateTable(table("public", "foo", vec![col("id", None)]))]),
            ["DROP INDEX public.foo", "CREATE TABLE public.foo (\n    id integer\n)"]
        );
        assert_eq!(
            sql(&[drop_index(), Change::CreateSequence(sequence("foo", "bigint", opts("bigint", 1)))]),
            ["DROP INDEX public.foo", "CREATE SEQUENCE public.foo"]
        );
        // A table or sequence `foo` dropped, index `foo` created.
        assert_eq!(
            sql(&[create_index(), drop_table(table("public", "foo", vec![col("id", None)]))]),
            ["DROP TABLE public.foo", "CREATE INDEX foo ON public.t USING btree (id)"]
        );
        assert_eq!(
            sql(&[create_index(), Change::DropSequence(sequence("foo", "bigint", opts("bigint", 1)))]),
            ["DROP SEQUENCE public.foo", "CREATE INDEX foo ON public.t USING btree (id)"]
        );
        // Another schema's name is another relation.
        let other = Change::CreateTable(table("app", "foo", vec![col("id", None)]));
        assert_eq!(sql(&[other, drop_index()])[0], "CREATE TABLE app.foo (\n    id integer\n)");
    }
}
