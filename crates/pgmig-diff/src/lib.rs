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
//! # Domains
//!
//! Domains are compared by base type and collation, default, `NOT NULL` and check constraints
//! (matched by name, by definition without `NOT VALID`, and by whether they are validated). A new
//! domain carries its validated checks in [`Change::CreateDomain`]; its `NOT VALID` ones follow as
//! [`Change::AddDomainConstraint`] (which `CREATE DOMAIN` would validate). A check that becomes
//! validated is [`Change::ValidateDomainConstraint`]; any other change to a check (its definition,
//! or validated becoming `NOT VALID`) drops it and adds it again.
//!
//! Postgres cannot change a domain's base type or collation in place, so the domain is dropped and
//! created again, which only works when nothing that stays uses it. When a table column, a
//! composite type's attribute or another domain keeps using it, the change is
//! [`Change::UnsupportedDomainChange`], which the planner refuses.
//!
//! # Composite types
//!
//! Composite types (`CREATE TYPE ... AS (...)`) are compared attribute by attribute, matched by
//! name (Postgres cannot reorder them, and adds new ones at the end), by type, collation and
//! comment ([`Change::AddAttribute`], [`Change::DropAttribute`], [`Change::AlterAttributeType`]).
//! Postgres refuses to change an attribute's type while a table column uses the composite type
//! (directly, in an array, or through a domain or another composite type): such a change is
//! [`Change::UnsupportedCompositeChange`], which the planner refuses. A column that stops using it
//! (dropped, or changing to an unrelated type) or starts using it (added, or changing to it) does
//! not block the change: [`Change::AlterAttributeType`] lists the types holding the composite type, so
//! the planner ends those uses before the change and starts the new ones after it. The exception is
//! a column that holds the composite type both before and after, such as one changing from the
//! composite type to a domain over it (or the reverse): no order ends the use, so that is
//! [`Change::UnsupportedCompositeChange`] too.
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
//! # Indexes
//!
//! Indexes no constraint owns are compared by their printed definition (`pg_get_indexdef`) and
//! matched by name within their table. A changed index is dropped and created again
//! ([`Change::DropIndex`], [`Change::CreateIndex`]); the catalog cannot tell a renamed index from
//! one dropped and another created, so that is what a rename is too. An index whose column
//! changes type is rebuilt by Postgres and is not a change unless its definition prints
//! differently. A new table's indexes are created after it; a dropped table's go with it, and
//! [`Change::DropTable`] lists them. An index removed from a kept table is always dropped
//! explicitly, also when a column it uses is dropped (which would drop it implicitly): the drop
//! runs first. A foreign key that stays the same but references a unique index that is dropped
//! (or dropped and created again) is dropped first and added again after, as for a constraint.
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

use pgmig_model::{
    Attribute, Column, Composite, Constraint, ConstraintKind, Domain, DomainConstraint, Enum, Identity, Index, Model, OwnedBy, QualName,
    Sequence, Table, quote_ident,
};

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
    /// A type that is not a table's row type (an enum, a domain or a composite type).
    Type(QualName),
    Sequence(QualName),
    /// A table constraint by its table and name.
    Constraint {
        table: QualName,
        name: String,
    },
    /// An index by its table and name (it is in the table's schema).
    Index {
        table: QualName,
        name: String,
    },
    /// A domain's check constraint by its domain and name.
    DomainConstraint {
        domain: QualName,
        name: String,
    },
    /// An attribute of a composite type.
    Attribute {
        composite: QualName,
        attribute: String,
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
    /// The domain with the check constraints created with it (see the module doc).
    CreateDomain(Domain),
    /// `DROP DOMAIN`; the domain as it was.
    DropDomain(Domain),
    SetDomainDefault {
        name: QualName,
        default: String,
    },
    DropDomainDefault {
        name: QualName,
    },
    SetDomainNotNull {
        name: QualName,
    },
    DropDomainNotNull {
        name: QualName,
    },
    /// `ALTER DOMAIN ... ADD CONSTRAINT`, `NOT VALID` when the constraint is not validated.
    AddDomainConstraint {
        domain: QualName,
        constraint: DomainConstraint,
    },
    /// `ALTER DOMAIN ... DROP CONSTRAINT`; `constraint` is the constraint as it was.
    DropDomainConstraint {
        domain: QualName,
        constraint: DomainConstraint,
    },
    /// `ALTER DOMAIN ... VALIDATE CONSTRAINT`.
    ValidateDomainConstraint {
        domain: QualName,
        name: String,
    },
    /// The base type (with its collation) changed from `from` to `to` while the objects
    /// `used_by` (`column public.t.c`, `attribute public.pair.a`, `domain public.d`) keep using
    /// the domain (see the module doc); the planner reports it as an error.
    UnsupportedDomainChange {
        name: QualName,
        from: String,
        to: String,
        used_by: Vec<String>,
    },
    /// The composite type with its attributes.
    CreateCompositeType(Composite),
    /// `DROP TYPE`; the composite type as it was.
    DropCompositeType(Composite),
    /// `ALTER TYPE ... ADD ATTRIBUTE`.
    AddAttribute {
        composite: QualName,
        attribute: Attribute,
    },
    /// `ALTER TYPE ... DROP ATTRIBUTE`; `type_ref` is the dropped attribute's.
    DropAttribute {
        composite: QualName,
        attribute: String,
        type_ref: Option<QualName>,
    },
    /// `ALTER TYPE ... ALTER ATTRIBUTE ... TYPE` to `type_name` with `collation`; `type_ref` is the
    /// new type's, `old_type_ref` the replaced one's. `holders` are the other types whose values
    /// hold the composite type, in either model: domains over it and composite types with an
    /// attribute of it, directly or through each other. Postgres refuses the change while a table
    /// column uses the composite type through any of them, so the planner runs what stops such a
    /// use before it and what starts one after it.
    AlterAttributeType {
        composite: QualName,
        attribute: String,
        type_name: String,
        collation: Option<QualName>,
        type_ref: Option<QualName>,
        old_type_ref: Option<QualName>,
        holders: Vec<QualName>,
    },
    /// An attribute's type (with its collation) changed from `from` to `to` while the table columns
    /// `used_by` use the composite type (see the module doc); the planner reports it as an error.
    UnsupportedCompositeChange {
        name: QualName,
        attribute: String,
        from: String,
        to: String,
        used_by: Vec<String>,
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
    /// `CREATE INDEX` as the server printed it ([`Index::definition`]).
    CreateIndex {
        table: QualName,
        index: Index,
    },
    /// `DROP INDEX`; `index` is the index as it was.
    DropIndex {
        table: QualName,
        index: Index,
    },
    /// `COMMENT ON ... IS <comment>`; `None` removes the comment. Never emitted for an object
    /// that is being dropped.
    SetComment {
        target: CommentTarget,
        comment: Option<String>,
    },
}

/// Declares [`ChangeKind`] with one variant per [`Change`] variant, and its names.
macro_rules! change_kinds {
    ($($variant:ident => $name:literal,)*) => {
        /// What kind of change a [`Change`] is: one kind per variant, named like the variant's
        /// `kind` tag (snake_case). The names are stable: a diff policy lists them (`skip`).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum ChangeKind {
            $($variant,)*
        }

        impl ChangeKind {
            /// Every kind, in the order of [`Change`]'s variants.
            pub const ALL: &[ChangeKind] = &[$(ChangeKind::$variant,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(ChangeKind::$variant => $name,)*
                }
            }
        }

        impl Change {
            pub fn kind(&self) -> ChangeKind {
                match self {
                    $(Change::$variant { .. } => ChangeKind::$variant,)*
                }
            }
        }
    };
}

change_kinds! {
    CreateSchema => "create_schema",
    DropSchema => "drop_schema",
    CreateTable => "create_table",
    DropTable => "drop_table",
    AddColumn => "add_column",
    DropColumn => "drop_column",
    AlterColumnType => "alter_column_type",
    SetNotNull => "set_not_null",
    DropNotNull => "drop_not_null",
    SetDefault => "set_default",
    DropDefault => "drop_default",
    AddIdentity => "add_identity",
    AlterIdentity => "alter_identity",
    DropIdentity => "drop_identity",
    CreateSequence => "create_sequence",
    DropSequence => "drop_sequence",
    AlterSequence => "alter_sequence",
    SetSequenceOwner => "set_sequence_owner",
    CreateEnum => "create_enum",
    DropEnum => "drop_enum",
    AddEnumValue => "add_enum_value",
    RenameEnumValue => "rename_enum_value",
    UnsupportedEnumChange => "unsupported_enum_change",
    CreateDomain => "create_domain",
    DropDomain => "drop_domain",
    SetDomainDefault => "set_domain_default",
    DropDomainDefault => "drop_domain_default",
    SetDomainNotNull => "set_domain_not_null",
    DropDomainNotNull => "drop_domain_not_null",
    AddDomainConstraint => "add_domain_constraint",
    DropDomainConstraint => "drop_domain_constraint",
    ValidateDomainConstraint => "validate_domain_constraint",
    UnsupportedDomainChange => "unsupported_domain_change",
    CreateCompositeType => "create_composite_type",
    DropCompositeType => "drop_composite_type",
    AddAttribute => "add_attribute",
    DropAttribute => "drop_attribute",
    AlterAttributeType => "alter_attribute_type",
    UnsupportedCompositeChange => "unsupported_composite_change",
    AddConstraint => "add_constraint",
    DropConstraint => "drop_constraint",
    RenameConstraint => "rename_constraint",
    AlterConstraint => "alter_constraint",
    CreateIndex => "create_index",
    DropIndex => "drop_index",
    SetComment => "set_comment",
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A name that is not a [`ChangeKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKind(pub String);

impl std::fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = ChangeKind::ALL.iter().map(|k| k.as_str()).collect();
        write!(f, "unknown kind of change {:?} (expected one of {})", self.0, names.join(", "))
    }
}

impl std::error::Error for UnknownKind {}

impl std::str::FromStr for ChangeKind {
    type Err = UnknownKind;

    fn from_str(s: &str) -> Result<ChangeKind, UnknownKind> {
        ChangeKind::ALL.iter().copied().find(|k| k.as_str() == s).ok_or_else(|| UnknownKind(s.to_owned()))
    }
}

impl Change {
    /// The object the change is about, as messages name it: `app` (a schema), `public.t` (a
    /// table, sequence or type), `public.t.c` (a column or attribute, or a constraint by its name),
    /// and for a comment its target's kind first (`column public.t.c`).
    pub fn object(&self) -> String {
        let col = |t: &QualName, c: &str| format!("{t}.{}", quote_ident(c));
        match self {
            Change::CreateSchema(s) | Change::DropSchema(s) => quote_ident(s),
            Change::CreateTable(t) | Change::DropTable { table: t, .. } => t.name.to_string(),
            Change::AddColumn { table, column } => col(table, &column.name),
            Change::DropColumn { table, column, .. }
            | Change::AlterColumnType { table, column, .. }
            | Change::SetNotNull { table, column }
            | Change::DropNotNull { table, column }
            | Change::SetDefault { table, column, .. }
            | Change::DropDefault { table, column, .. }
            | Change::AddIdentity { table, column, .. }
            | Change::AlterIdentity { table, column, .. }
            | Change::DropIdentity { table, column, .. } => col(table, column),
            Change::CreateSequence(s) | Change::DropSequence(s) => s.name.to_string(),
            Change::AlterSequence { to, .. } => to.name.to_string(),
            Change::SetSequenceOwner { name, .. } => name.to_string(),
            Change::CreateEnum(e) | Change::DropEnum(e) => e.name.to_string(),
            Change::AddEnumValue { name, .. } | Change::RenameEnumValue { name, .. } | Change::UnsupportedEnumChange { name, .. } => {
                name.to_string()
            }
            Change::CreateDomain(d) | Change::DropDomain(d) => d.name.to_string(),
            Change::SetDomainDefault { name, .. }
            | Change::DropDomainDefault { name }
            | Change::SetDomainNotNull { name }
            | Change::DropDomainNotNull { name }
            | Change::UnsupportedDomainChange { name, .. } => name.to_string(),
            Change::AddDomainConstraint { domain, constraint } | Change::DropDomainConstraint { domain, constraint } => {
                col(domain, &constraint.name)
            }
            Change::ValidateDomainConstraint { domain, name } => col(domain, name),
            Change::CreateCompositeType(c) | Change::DropCompositeType(c) => c.name.to_string(),
            Change::AddAttribute { composite, attribute } => col(composite, &attribute.name),
            Change::DropAttribute { composite, attribute, .. } | Change::AlterAttributeType { composite, attribute, .. } => {
                col(composite, attribute)
            }
            Change::UnsupportedCompositeChange { name, attribute, .. } => col(name, attribute),
            Change::AddConstraint { table, constraint } | Change::DropConstraint { table, constraint } => col(table, &constraint.name),
            Change::RenameConstraint { table, from, .. } => col(table, from),
            Change::AlterConstraint { table, to } => col(table, &to.name),
            Change::CreateIndex { table, index } | Change::DropIndex { table, index } => {
                QualName::new(table.schema.clone(), index.name.clone()).to_string()
            }
            Change::SetComment { target, .. } => match target {
                CommentTarget::Schema(s) => format!("schema {}", quote_ident(s)),
                CommentTarget::Table(t) => format!("table {t}"),
                CommentTarget::Column { table, column } => format!("column {}", col(table, column)),
                CommentTarget::Type(t) => format!("type {t}"),
                CommentTarget::Sequence(s) => format!("sequence {s}"),
                CommentTarget::Constraint { table, name } => format!("constraint {}", col(table, name)),
                CommentTarget::Index { table, name } => format!("index {}", QualName::new(table.schema.clone(), name.clone())),
                CommentTarget::DomainConstraint { domain, name } => format!("domain constraint {}", col(domain, name)),
                CommentTarget::Attribute { composite, attribute } => format!("attribute {}", col(composite, attribute)),
            },
        }
    }
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
    diff_domains(a, b, &mut changes);
    diff_composites(a, b, &mut changes);

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
                created.indexes.clear();
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
                for i in &table.indexes {
                    changes.push(Change::CreateIndex { table: name.clone(), index: i.clone() });
                    set_comment(index_target(name, i), None, i.comment.as_ref(), &mut changes);
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
    // Constraints and indexes of `a` that stop existing under their name: a foreign key using one
    // must go too.
    let mut removed = removed_indexes(a, b);
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

fn domain_constraint_target(domain: &QualName, c: &DomainConstraint) -> CommentTarget {
    CommentTarget::DomainConstraint { domain: domain.clone(), name: c.name.clone() }
}

fn diff_domains(a: &Model, b: &Model, changes: &mut Vec<Change>) {
    for (name, d) in &b.domains {
        match a.domains.get(name) {
            None => create_domain(d, changes),
            Some(old) if old.base_type != d.base_type || old.collation != d.collation => {
                let used_by = kept_users(a, b, name);
                if used_by.is_empty() {
                    changes.push(Change::DropDomain(old.clone()));
                    create_domain(d, changes);
                } else {
                    changes.push(Change::UnsupportedDomainChange {
                        name: name.clone(),
                        from: old.base_type_clause(),
                        to: d.base_type_clause(),
                        used_by,
                    });
                }
            }
            Some(old) => diff_domain(old, d, changes),
        }
    }
    for d in a.domains.values().filter(|d| !b.domains.contains_key(&d.name)) {
        changes.push(Change::DropDomain(d.clone()));
    }
}

/// A new domain: created with its validated checks, then its `NOT VALID` ones added.
fn create_domain(d: &Domain, changes: &mut Vec<Change>) {
    let mut created = d.clone();
    created.constraints.retain(|c| c.validated);
    changes.push(Change::CreateDomain(created));
    set_comment(CommentTarget::Type(d.name.clone()), None, d.comment.as_ref(), changes);
    for c in &d.constraints {
        if !c.validated {
            changes.push(Change::AddDomainConstraint { domain: d.name.clone(), constraint: c.clone() });
        }
        set_comment(domain_constraint_target(&d.name, c), None, c.comment.as_ref(), changes);
    }
}

/// A domain both models have, with the same base type.
fn diff_domain(a: &Domain, b: &Domain, changes: &mut Vec<Change>) {
    let name = || b.name.clone();
    match (&a.default, &b.default) {
        (_, Some(new)) if a.default.as_ref() != Some(new) => changes.push(Change::SetDomainDefault { name: name(), default: new.clone() }),
        (Some(_), None) => changes.push(Change::DropDomainDefault { name: name() }),
        _ => {}
    }
    match (a.not_null, b.not_null) {
        (false, true) => changes.push(Change::SetDomainNotNull { name: name() }),
        (true, false) => changes.push(Change::DropDomainNotNull { name: name() }),
        _ => {}
    }
    for c in &b.constraints {
        let target = domain_constraint_target(&b.name, c);
        match a.constraint(&c.name) {
            // Validating is the one change made in place.
            Some(o) if o.body() == c.body() && (o.validated == c.validated || c.validated) => {
                if !o.validated && c.validated {
                    changes.push(Change::ValidateDomainConstraint { domain: name(), name: c.name.clone() });
                }
                set_comment(target, o.comment.as_ref(), c.comment.as_ref(), changes);
            }
            Some(o) => {
                changes.push(Change::DropDomainConstraint { domain: name(), constraint: o.clone() });
                changes.push(Change::AddDomainConstraint { domain: name(), constraint: c.clone() });
                set_comment(target, None, c.comment.as_ref(), changes);
            }
            None => {
                changes.push(Change::AddDomainConstraint { domain: name(), constraint: c.clone() });
                set_comment(target, None, c.comment.as_ref(), changes);
            }
        }
    }
    for o in a.constraints.iter().filter(|o| b.constraint(&o.name).is_none()) {
        changes.push(Change::DropDomainConstraint { domain: name(), constraint: o.clone() });
    }
    set_comment(CommentTarget::Type(name()), a.comment.as_ref(), b.comment.as_ref(), changes);
}

/// What in `a` uses type `ty` directly and keeps using it in `b`: table columns, composite type
/// attributes and domains over it, as messages name them.
fn kept_users(a: &Model, b: &Model, ty: &QualName) -> Vec<String> {
    let uses = |r: &Option<QualName>| r.as_ref() == Some(ty);
    let mut out = Vec::new();
    for t in a.tables.values() {
        for c in t.columns.iter().filter(|c| uses(&c.type_ref)) {
            if b.tables.get(&t.name).and_then(|n| n.column(&c.name)).is_some_and(|n| uses(&n.type_ref)) {
                out.push(format!("column {}.{}", t.name, quote_ident(&c.name)));
            }
        }
    }
    for comp in a.composites.values() {
        for at in comp.attributes.iter().filter(|at| uses(&at.type_ref)) {
            if b.composites.get(&comp.name).and_then(|n| n.attribute(&at.name)).is_some_and(|n| uses(&n.type_ref)) {
                out.push(format!("attribute {}.{}", comp.name, quote_ident(&at.name)));
            }
        }
    }
    for d in a.domains.values().filter(|d| uses(&d.base_ref)) {
        if b.domains.get(&d.name).is_some_and(|n| uses(&n.base_ref)) {
            out.push(format!("domain {}", d.name));
        }
    }
    out
}

/// The types a value of type `root` holds in `m`, itself included: through arrays, domains and
/// composite types.
fn reach(m: &Model, root: &QualName) -> BTreeSet<QualName> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.clone()];
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty.clone()) {
            continue;
        }
        if let Some(d) = m.domains.get(&ty) {
            stack.extend(d.base_ref.clone());
        }
        if let Some(comp) = m.composites.get(&ty) {
            stack.extend(comp.attributes.iter().filter_map(|at| at.type_ref.clone()));
        }
    }
    seen
}

/// The types of `a` that table columns use and keep using in `b`, directly or through arrays,
/// domains and composite types, each with those columns (as messages name them). A column whose
/// type changes counts for the types both its old type (in `a`) and its new type (in `b`) reach:
/// it holds them before and after, so no order of the steps can end the use.
fn types_in_use(a: &Model, b: &Model) -> BTreeMap<QualName, Vec<String>> {
    let mut out: BTreeMap<QualName, Vec<String>> = BTreeMap::new();
    for t in a.tables.values() {
        for c in &t.columns {
            let Some(root) = &c.type_ref else { continue };
            let Some(new_root) = b.tables.get(&t.name).and_then(|n| n.column(&c.name)).and_then(|n| n.type_ref.as_ref()) else {
                continue;
            };
            let label = format!("column {}.{}", t.name, quote_ident(&c.name));
            let mut held = reach(a, root);
            if new_root != root {
                let after = reach(b, new_root);
                held.retain(|ty| after.contains(ty));
            }
            for ty in held {
                out.entry(ty).or_default().push(label.clone());
            }
        }
    }
    out
}

/// The types other than `ty` whose values hold `ty` in `a` or in `b`: domains over it and composite
/// types with an attribute of it, directly or through each other (arrays are seen through, as
/// `type_ref` is the element type).
fn holders(a: &Model, b: &Model, ty: &QualName) -> Vec<QualName> {
    let mut out = BTreeSet::new();
    for m in [a, b] {
        let mut held = BTreeSet::from([ty.clone()]);
        loop {
            let holds = |r: &Option<QualName>| r.as_ref().is_some_and(|r| held.contains(r));
            let new: Vec<QualName> = m
                .domains
                .values()
                .filter(|d| holds(&d.base_ref))
                .map(|d| d.name.clone())
                .chain(m.composites.values().filter(|c| c.attributes.iter().any(|at| holds(&at.type_ref))).map(|c| c.name.clone()))
                .filter(|n| !held.contains(n))
                .collect();
            if new.is_empty() {
                break;
            }
            held.extend(new);
        }
        held.remove(ty);
        out.extend(held);
    }
    out.into_iter().collect()
}

fn attribute_target(composite: &QualName, attribute: &str) -> CommentTarget {
    CommentTarget::Attribute { composite: composite.clone(), attribute: attribute.to_owned() }
}

fn diff_composites(a: &Model, b: &Model, changes: &mut Vec<Change>) {
    let in_use = types_in_use(a, b);
    for (name, c) in &b.composites {
        let Some(old) = a.composites.get(name) else {
            changes.push(Change::CreateCompositeType(c.clone()));
            set_comment(CommentTarget::Type(name.clone()), None, c.comment.as_ref(), changes);
            for at in &c.attributes {
                set_comment(attribute_target(name, &at.name), None, at.comment.as_ref(), changes);
            }
            continue;
        };
        for at in &c.attributes {
            let Some(o) = old.attribute(&at.name) else {
                changes.push(Change::AddAttribute { composite: name.clone(), attribute: at.clone() });
                set_comment(attribute_target(name, &at.name), None, at.comment.as_ref(), changes);
                continue;
            };
            if o.type_name != at.type_name || o.collation != at.collation {
                match in_use.get(name) {
                    Some(used_by) => changes.push(Change::UnsupportedCompositeChange {
                        name: name.clone(),
                        attribute: at.name.clone(),
                        from: o.type_clause(),
                        to: at.type_clause(),
                        used_by: used_by.clone(),
                    }),
                    None => changes.push(Change::AlterAttributeType {
                        composite: name.clone(),
                        attribute: at.name.clone(),
                        type_name: at.type_name.clone(),
                        collation: at.collation.clone(),
                        type_ref: at.type_ref.clone(),
                        old_type_ref: o.type_ref.clone(),
                        holders: holders(a, b, name),
                    }),
                }
            }
            set_comment(attribute_target(name, &at.name), o.comment.as_ref(), at.comment.as_ref(), changes);
        }
        for o in old.attributes.iter().filter(|o| c.attribute(&o.name).is_none()) {
            changes.push(Change::DropAttribute { composite: name.clone(), attribute: o.name.clone(), type_ref: o.type_ref.clone() });
        }
        set_comment(CommentTarget::Type(name.clone()), old.comment.as_ref(), c.comment.as_ref(), changes);
    }
    for c in a.composites.values().filter(|c| !b.composites.contains_key(&c.name)) {
        changes.push(Change::DropCompositeType(c.clone()));
    }
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
    for i in &b.indexes {
        match a.index(&i.name) {
            Some(o) if o.definition == i.definition => set_comment(index_target(table, i), o.comment.as_ref(), i.comment.as_ref(), changes),
            Some(o) => {
                changes.push(Change::DropIndex { table: table.clone(), index: o.clone() });
                changes.push(Change::CreateIndex { table: table.clone(), index: i.clone() });
                set_comment(index_target(table, i), None, i.comment.as_ref(), changes);
            }
            None => {
                changes.push(Change::CreateIndex { table: table.clone(), index: i.clone() });
                set_comment(index_target(table, i), None, i.comment.as_ref(), changes);
            }
        }
    }
    for o in a.indexes.iter().filter(|o| b.index(&o.name).is_none()) {
        changes.push(Change::DropIndex { table: table.clone(), index: o.clone() });
    }
}

fn index_target(table: &QualName, i: &Index) -> CommentTarget {
    CommentTarget::Index { table: table.clone(), name: i.name.clone() }
}

/// The indexes of tables both models have that stop existing under their name: dropped, or
/// dropped and created again.
fn removed_indexes(a: &Model, b: &Model) -> BTreeSet<(QualName, String)> {
    let mut out = BTreeSet::new();
    for new in b.tables.values() {
        let Some(old) = a.tables.get(&new.name) else { continue };
        for o in &old.indexes {
            if new.index(&o.name).is_none_or(|i| i.definition != o.definition) {
                out.insert((new.name.clone(), o.name.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kind names are part of pgmig's interface (`skip`): they are the changes' `kind` tags.
    #[test]
    fn kinds_are_named_like_the_changes_tags() {
        for kind in ChangeKind::ALL {
            assert_eq!(serde_json::to_string(kind).expect("serialize"), format!("\"{kind}\""));
            assert_eq!(kind.as_str().parse::<ChangeKind>(), Ok(*kind));
        }
        let changes = [
            Change::DropSchema("app".into()),
            Change::DropNotNull { table: QualName::new("public", "t"), column: "c".into() },
            Change::SetComment { target: CommentTarget::Schema("app".into()), comment: None },
        ];
        for c in &changes {
            let json = serde_json::to_value(c).expect("serialize");
            assert_eq!(json["kind"], c.kind().as_str());
        }
        assert_eq!(changes[1].object(), "public.t.c");
        assert_eq!(changes[2].object(), "schema app");
        let err = "drop_tables".parse::<ChangeKind>().expect_err("unknown");
        assert!(
            err.to_string().starts_with("unknown kind of change \"drop_tables\" (expected one of create_schema, drop_schema,"),
            "{err}"
        );
    }

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
        m.tables.insert(name.clone(), Table { name, columns, comment: None, constraints: vec![], indexes: vec![] });
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
        m.tables
            .insert(name.clone(), Table { name, columns: vec![c], comment: comment.map(Into::into), constraints: vec![], indexes: vec![] });
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
            index_definition: None,
            on_partitioned_table: false,
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
            m.tables.insert(
                name.clone(),
                Table { name, columns: vec![col("id", "integer")], comment: None, constraints: constraints.clone(), indexes: vec![] },
            );
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

    /// An index on `public.t` with the key `(n)` as printed.
    fn idx(name: &str, key: &str) -> Index {
        Index {
            name: name.into(),
            definition: format!("CREATE INDEX {name} ON public.t USING btree {key}"),
            unique: false,
            columns: vec!["n".into()],
            comment: None,
            on_partitioned_table: false,
        }
    }

    fn with_indexes(indexes: Vec<Index>) -> Model {
        let mut m = model(vec![col("n", "integer")]);
        m.tables.get_mut(&t("t")).expect("t").indexes = indexes;
        m
    }

    #[test]
    fn indexes_are_created_replaced_and_dropped_by_name() {
        let commented = |i: Index, c: &str| Index { comment: Some(c.into()), ..i };
        let a =
            with_indexes(vec![idx("same", "(n)"), commented(idx("changed", "(n)"), "kept"), idx("gone", "(n)"), idx("old_name", "(n)")]);
        let b = with_indexes(vec![
            commented(idx("same", "(n)"), "new"),
            commented(idx("changed", "(n DESC)"), "kept"),
            idx("added", "(n)"),
            idx("new_name", "(n)"),
        ]);
        let target = |name: &str| CommentTarget::Index { table: t("t"), name: name.into() };
        assert_eq!(
            diff(&a, &b),
            vec![
                set(target("same"), Some("new")),
                Change::DropIndex { table: t("t"), index: commented(idx("changed", "(n)"), "kept") },
                Change::CreateIndex { table: t("t"), index: commented(idx("changed", "(n DESC)"), "kept") },
                set(target("changed"), Some("kept")),
                Change::CreateIndex { table: t("t"), index: idx("added", "(n)") },
                // A rename is a drop and a create: the catalog cannot tell them apart.
                Change::CreateIndex { table: t("t"), index: idx("new_name", "(n)") },
                Change::DropIndex { table: t("t"), index: idx("gone", "(n)") },
                Change::DropIndex { table: t("t"), index: idx("old_name", "(n)") },
            ]
        );
        assert!(diff(&b, &b).is_empty());
    }

    #[test]
    fn a_new_tables_indexes_follow_it_and_a_dropped_tables_go_with_it() {
        let b = with_indexes(vec![Index { comment: Some("c".into()), ..idx("i", "(n)") }]);
        let mut empty = Model::empty_database();
        empty.tables.clear();
        let created = diff(&empty, &b);
        let Some(Change::CreateTable(table)) = created.first() else { panic!("{created:?}") };
        assert!(table.indexes.is_empty());
        assert_eq!(
            created[1..],
            [
                Change::CreateIndex { table: t("t"), index: Index { comment: Some("c".into()), ..idx("i", "(n)") } },
                set(CommentTarget::Index { table: t("t"), name: "i".into() }, Some("c")),
            ]
        );
        let dropped = diff(&b, &empty);
        assert!(matches!(&dropped[..], [Change::DropTable { table, .. }] if table.indexes.len() == 1), "{dropped:?}");
    }

    #[test]
    fn a_foreign_key_using_a_replaced_unique_index_is_added_again() {
        let f = fk("f", "p", "p_id_idx", "");
        let unique_index = |key: &str| Index {
            name: "p_id_idx".into(),
            definition: format!("CREATE UNIQUE INDEX p_id_idx ON public.p USING btree {key}"),
            unique: true,
            columns: vec!["id".into()],
            comment: None,
            on_partitioned_table: false,
        };
        let with = |key: &str| {
            let mut m = tables(&[("p", vec![]), ("t", vec![f.clone()])]);
            m.tables.get_mut(&t("p")).expect("p").indexes = vec![unique_index(key)];
            m
        };
        assert_eq!(
            diff(&with("(id)"), &with("(id) NULLS NOT DISTINCT")),
            vec![
                Change::DropIndex { table: t("p"), index: unique_index("(id)") },
                Change::CreateIndex { table: t("p"), index: unique_index("(id) NULLS NOT DISTINCT") },
                Change::DropConstraint { table: t("t"), constraint: f.clone() },
                Change::AddConstraint { table: t("t"), constraint: f },
            ]
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
            index_definition: None,
            on_partitioned_table: false,
        };
        let table = Table {
            name: t.clone(),
            columns: vec![col("id", "integer")],
            comment: Some("t".into()),
            constraints: vec![constraint.clone()],
            indexes: vec![idx("t_n_idx", "(n)")],
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
            Change::CreateIndex { table: t.clone(), index: idx("t_n_idx", "(n)") },
            Change::DropIndex { table: t.clone(), index: idx("t_n_idx", "(n)") },
            Change::SetComment { target: CommentTarget::Schema("app".into()), comment: Some("x".into()) },
            Change::SetComment { target: CommentTarget::Table(t.clone()), comment: None },
            Change::SetComment { target: CommentTarget::Column { table: t.clone(), column: id() }, comment: Some("c".into()) },
            Change::SetComment { target: CommentTarget::Type(e), comment: None },
            Change::SetComment { target: CommentTarget::Sequence(s), comment: None },
            Change::SetComment { target: CommentTarget::Constraint { table: t.clone(), name: "t_pkey".into() }, comment: None },
            Change::SetComment { target: CommentTarget::Index { table: t, name: "t_n_idx".into() }, comment: Some("i".into()) },
        ]
        .into_iter()
        .chain(domain_and_composite_changes())
        .collect()
    }

    /// A sample of every domain and composite type change.
    fn domain_and_composite_changes() -> Vec<Change> {
        let d = QualName::new("public", "d");
        let p = QualName::new("public", "p");
        let check = domain_check("d_check", "CHECK ((VALUE > 0)) NOT VALID", false);
        let domain = posint();
        let comp = pair();
        vec![
            Change::CreateDomain(domain.clone()),
            Change::DropDomain(domain),
            Change::SetDomainDefault { name: d.clone(), default: "1".into() },
            Change::DropDomainDefault { name: d.clone() },
            Change::SetDomainNotNull { name: d.clone() },
            Change::DropDomainNotNull { name: d.clone() },
            Change::AddDomainConstraint { domain: d.clone(), constraint: check.clone() },
            Change::DropDomainConstraint { domain: d.clone(), constraint: check },
            Change::ValidateDomainConstraint { domain: d.clone(), name: "d_check".into() },
            Change::UnsupportedDomainChange {
                name: d.clone(),
                from: "integer".into(),
                to: "bigint".into(),
                used_by: vec!["column public.t.id".into()],
            },
            Change::CreateCompositeType(comp.clone()),
            Change::DropCompositeType(comp.clone()),
            Change::AddAttribute { composite: p.clone(), attribute: comp.attributes[0].clone() },
            Change::DropAttribute { composite: p.clone(), attribute: "a".into(), type_ref: None },
            Change::AlterAttributeType {
                composite: p.clone(),
                attribute: "a".into(),
                type_name: "public.d".into(),
                collation: None,
                type_ref: Some(d.clone()),
                old_type_ref: None,
                holders: Vec::new(),
            },
            Change::UnsupportedCompositeChange {
                name: p.clone(),
                attribute: "a".into(),
                from: "integer".into(),
                to: "bigint".into(),
                used_by: vec!["column public.t.p".into()],
            },
            Change::SetComment { target: CommentTarget::DomainConstraint { domain: d, name: "d_check".into() }, comment: None },
            Change::SetComment { target: CommentTarget::Attribute { composite: p, attribute: "a".into() }, comment: Some("a".into()) },
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
            Change::CreateIndex { .. } => 27,
            Change::DropIndex { .. } => 28,
            Change::SetComment { .. } => 29,
            Change::CreateDomain(_) => 30,
            Change::DropDomain(_) => 31,
            Change::SetDomainDefault { .. } => 32,
            Change::DropDomainDefault { .. } => 33,
            Change::SetDomainNotNull { .. } => 34,
            Change::DropDomainNotNull { .. } => 35,
            Change::AddDomainConstraint { .. } => 36,
            Change::DropDomainConstraint { .. } => 37,
            Change::ValidateDomainConstraint { .. } => 38,
            Change::UnsupportedDomainChange { .. } => 39,
            Change::CreateCompositeType(_) => 40,
            Change::DropCompositeType(_) => 41,
            Change::AddAttribute { .. } => 42,
            Change::DropAttribute { .. } => 43,
            Change::AlterAttributeType { .. } => 44,
            Change::UnsupportedCompositeChange { .. } => 45,
        }
    }

    #[test]
    fn every_change_round_trips_through_json() {
        let changes = every_change();
        let covered: BTreeSet<usize> = changes.iter().map(variant).collect();
        assert_eq!(covered, (0..46).collect(), "a sample for every variant");
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

    fn domain_check(name: &str, definition: &str, validated: bool) -> DomainConstraint {
        DomainConstraint { name: name.into(), definition: definition.into(), validated, comment: None }
    }

    /// `public.d`: a positive integer.
    fn posint() -> Domain {
        Domain {
            name: QualName::new("public", "d"),
            base_type: "integer".into(),
            base_ref: None,
            collation: None,
            default: None,
            not_null: false,
            constraints: vec![domain_check("d_pos", "CHECK ((VALUE > 0))", true)],
            comment: None,
        }
    }

    fn attr(name: &str, type_name: &str, type_ref: Option<QualName>) -> Attribute {
        Attribute { name: name.into(), type_name: type_name.into(), type_ref, collation: None, comment: None }
    }

    /// `public.p`: a pair of integers.
    fn pair() -> Composite {
        Composite {
            name: QualName::new("public", "p"),
            attributes: vec![attr("a", "integer", None), attr("b", "integer", None)],
            comment: None,
        }
    }

    fn with_domains(domains: Vec<Domain>, composites: Vec<Composite>, columns: Vec<Column>) -> Model {
        let mut m = model(columns);
        m.domains = domains.into_iter().map(|d| (d.name.clone(), d)).collect();
        m.composites = composites.into_iter().map(|c| (c.name.clone(), c)).collect();
        m
    }

    #[test]
    fn a_new_domain_carries_its_validated_checks_and_adds_the_others() {
        let mut d = posint();
        d.comment = Some("pos".into());
        d.constraints
            .push(DomainConstraint { comment: Some("later".into()), ..domain_check("d_small", "CHECK ((VALUE < 9)) NOT VALID", false) });
        let b = with_domains(vec![d.clone()], vec![], vec![]);
        let name = d.name.clone();
        let mut created = d.clone();
        created.constraints.truncate(1);
        assert_eq!(
            diff(&model(vec![]), &b),
            vec![
                Change::CreateDomain(created),
                set(CommentTarget::Type(name.clone()), Some("pos")),
                Change::AddDomainConstraint { domain: name.clone(), constraint: d.constraints[1].clone() },
                set(CommentTarget::DomainConstraint { domain: name, name: "d_small".into() }, Some("later")),
            ]
        );
        assert_eq!(diff(&b, &model(vec![])), vec![Change::DropDomain(d)]);
        assert!(diff(&b, &b).is_empty());
    }

    #[test]
    fn domain_defaults_not_null_and_checks_are_altered_in_place() {
        let a = posint();
        let mut b = posint();
        b.default = Some("1".into());
        b.not_null = true;
        b.constraints = vec![domain_check("d_big", "CHECK ((VALUE > 10)) NOT VALID", false)];
        let name = a.name.clone();
        let (ma, mb) = (with_domains(vec![a.clone()], vec![], vec![]), with_domains(vec![b.clone()], vec![], vec![]));
        assert_eq!(
            diff(&ma, &mb),
            vec![
                Change::SetDomainDefault { name: name.clone(), default: "1".into() },
                Change::SetDomainNotNull { name: name.clone() },
                Change::AddDomainConstraint { domain: name.clone(), constraint: b.constraints[0].clone() },
                Change::DropDomainConstraint { domain: name.clone(), constraint: a.constraints[0].clone() },
            ]
        );
        assert_eq!(
            diff(&mb, &ma),
            vec![
                Change::DropDomainDefault { name: name.clone() },
                Change::DropDomainNotNull { name: name.clone() },
                Change::AddDomainConstraint { domain: name.clone(), constraint: a.constraints[0].clone() },
                Change::DropDomainConstraint { domain: name, constraint: b.constraints[0].clone() },
            ]
        );
    }

    #[test]
    fn a_not_valid_check_is_validated_and_a_redefined_one_replaced() {
        let with = |c: DomainConstraint| {
            let mut d = posint();
            d.constraints = vec![c];
            with_domains(vec![d], vec![], vec![])
        };
        let name = QualName::new("public", "d");
        // Postgres prints NOT VALID; pgmem does not. The body and the flag decide either way.
        let not_valid = with(domain_check("d_pos", "CHECK ((VALUE > 0)) NOT VALID", false));
        let not_valid_pgmem = with(domain_check("d_pos", "CHECK ((VALUE > 0))", false));
        let valid = with(domain_check("d_pos", "CHECK ((VALUE > 0))", true));
        let validate = vec![Change::ValidateDomainConstraint { domain: name.clone(), name: "d_pos".into() }];
        assert_eq!(diff(&not_valid, &valid), validate);
        assert_eq!(diff(&not_valid_pgmem, &valid), validate);
        assert!(diff(&not_valid, &not_valid_pgmem).is_empty());
        // Back to NOT VALID cannot be done in place.
        let back = diff(&valid, &not_valid);
        assert!(
            matches!(&back[..], [Change::DropDomainConstraint { .. }, Change::AddDomainConstraint { constraint, .. }] if !constraint.validated)
        );
        let other = with(domain_check("d_pos", "CHECK ((VALUE > 1))", true));
        assert!(matches!(&diff(&valid, &other)[..], [Change::DropDomainConstraint { .. }, Change::AddDomainConstraint { .. }]));
    }

    #[test]
    fn a_new_base_type_recreates_an_unused_domain_and_is_unsupported_for_a_used_one() {
        let mut big = posint();
        big.base_type = "bigint".into();
        let name = big.name.clone();
        let unused = diff(&with_domains(vec![posint()], vec![], vec![]), &with_domains(vec![big.clone()], vec![], vec![]));
        assert_eq!(unused, vec![Change::DropDomain(posint()), Change::CreateDomain(big.clone())]);

        let user = || Column { type_ref: Some(name.clone()), ..col("n", "public.d") };
        let a = with_domains(vec![posint()], vec![], vec![user()]);
        let b = with_domains(vec![big.clone()], vec![], vec![user()]);
        assert_eq!(
            diff(&a, &b),
            vec![Change::UnsupportedDomainChange {
                name: name.clone(),
                from: "integer".into(),
                to: "bigint".into(),
                used_by: vec!["column public.t.n".into()]
            }]
        );
        // A column that goes, or stops using the domain, does not keep it.
        let dropped = diff(&a, &with_domains(vec![big.clone()], vec![], vec![]));
        assert!(dropped.contains(&Change::CreateDomain(big.clone())), "{dropped:?}");
        // So does a collation change.
        let mut collated = posint();
        collated.base_type = "text".into();
        let mut c = collated.clone();
        c.collation = Some(QualName::new("pg_catalog", "C"));
        let changed = diff(&with_domains(vec![collated], vec![], vec![user()]), &with_domains(vec![c], vec![], vec![user()]));
        assert!(
            matches!(&changed[..], [Change::UnsupportedDomainChange { to, .. }] if to == "text COLLATE pg_catalog.\"C\""),
            "{changed:?}"
        );
    }

    #[test]
    fn composite_attributes_are_added_dropped_and_retyped() {
        let p = QualName::new("public", "p");
        let mut b = pair();
        b.attributes = vec![attr("a", "bigint", None), attr("c", "public.d", Some(QualName::new("public", "d")))];
        b.attributes[1].comment = Some("c".into());
        let (ma, mb) = (with_domains(vec![], vec![pair()], vec![]), with_domains(vec![], vec![b.clone()], vec![]));
        assert_eq!(
            diff(&ma, &mb),
            vec![
                Change::AlterAttributeType {
                    composite: p.clone(),
                    attribute: "a".into(),
                    type_name: "bigint".into(),
                    collation: None,
                    type_ref: None,
                    old_type_ref: None,
                    holders: Vec::new()
                },
                Change::AddAttribute { composite: p.clone(), attribute: b.attributes[1].clone() },
                set(CommentTarget::Attribute { composite: p.clone(), attribute: "c".into() }, Some("c")),
                Change::DropAttribute { composite: p.clone(), attribute: "b".into(), type_ref: None },
            ]
        );
        assert_eq!(diff(&model(vec![]), &mb)[0], Change::CreateCompositeType(b.clone()));
        assert_eq!(diff(&mb, &model(vec![])), vec![Change::DropCompositeType(b)]);
        // Attribute order is not compared.
        let mut reordered = pair();
        reordered.attributes.reverse();
        assert!(diff(&ma, &with_domains(vec![], vec![reordered], vec![])).is_empty());
    }

    #[test]
    fn retyping_an_attribute_of_a_composite_a_column_uses_is_unsupported() {
        let p = QualName::new("public", "p");
        let mut b = pair();
        b.attributes[0].type_name = "bigint".into();
        // The column uses an array of a domain over the composite type.
        let mut over = posint();
        over.base_type = "public.p".into();
        over.base_ref = Some(p.clone());
        over.constraints.clear();
        let user = || Column { type_ref: Some(over.name.clone()), ..col("ps", "public.d[]") };
        let a = with_domains(vec![over.clone()], vec![pair()], vec![user()]);
        assert_eq!(
            diff(&a, &with_domains(vec![over.clone()], vec![b.clone()], vec![user()])),
            vec![Change::UnsupportedCompositeChange {
                name: p,
                attribute: "a".into(),
                from: "integer".into(),
                to: "bigint".into(),
                used_by: vec!["column public.t.ps".into()]
            }]
        );
        // Without the column it is altered.
        let alone = diff(&with_domains(vec![over.clone()], vec![pair()], vec![]), &with_domains(vec![over], vec![b], vec![]));
        // It lists the domain holding the composite type, so the planner orders column uses of it.
        assert!(
            matches!(&alone[..], [Change::AlterAttributeType { holders, .. }] if holders == &[QualName::new("public", "d")]),
            "{alone:?}"
        );
    }

    /// `pair` with `a` retyped to bigint, a domain `d` over `pair` and a composite `nest` holding it.
    fn holder_fixture() -> (Composite, Domain, Composite) {
        let p = QualName::new("public", "p");
        let mut b = pair();
        b.attributes[0].type_name = "bigint".into();
        let mut over = posint();
        over.base_type = "public.p".into();
        over.base_ref = Some(p.clone());
        over.constraints.clear();
        let mut nest = pair();
        nest.name = QualName::new("public", "nest");
        nest.attributes = vec![attr("p", "public.p", Some(p))];
        (b, over, nest)
    }

    fn column_of(ty: &QualName, sql: &str) -> Column {
        Column { type_ref: Some(ty.clone()), ..col("c", sql) }
    }

    /// Whether the diff refuses the retype of `p.a`, naming the column, and plans no attribute alter.
    fn refused(changes: &[Change]) -> bool {
        changes.iter().any(|c| {
            matches!(c, Change::UnsupportedCompositeChange { name, used_by, .. }
                if name == &QualName::new("public", "p") && used_by == &["column public.t.c".to_string()])
        }) && !changes.iter().any(|c| matches!(c, Change::AlterAttributeType { .. }))
    }

    #[test]
    fn retyping_a_column_between_a_composite_and_a_holder_of_it_is_unsupported() {
        let p = QualName::new("public", "p");
        let (b, over, nest) = holder_fixture();
        let on_p = || column_of(&p, "public.p");
        let on_d = || column_of(&over.name, "public.d");
        let on_nest = || column_of(&nest.name, "public.nest");
        // Domain over the composite type, before and after the retype.
        let (ma, mb) = (with_domains(vec![], vec![pair()], vec![on_p()]), with_domains(vec![over.clone()], vec![b.clone()], vec![on_d()]));
        assert!(refused(&diff(&ma, &mb)), "{:?}", diff(&ma, &mb));
        assert!(refused(&diff(&mb_rev(&over, &pair(), on_d()), &with_domains(vec![], vec![b.clone()], vec![on_p()]))));
        // A composite type with an attribute of it.
        let (ma, mb) =
            (with_domains(vec![], vec![pair()], vec![on_p()]), with_domains(vec![], vec![b.clone(), nest.clone()], vec![on_nest()]));
        assert!(refused(&diff(&ma, &mb)), "{:?}", diff(&ma, &mb));
    }

    fn mb_rev(over: &Domain, old_pair: &Composite, col: Column) -> Model {
        with_domains(vec![over.clone()], vec![old_pair.clone()], vec![col])
    }

    #[test]
    fn retyping_a_column_from_a_composite_to_an_unrelated_type_is_ordered() {
        let p = QualName::new("public", "p");
        let (b, _, _) = holder_fixture();
        let ma = with_domains(vec![], vec![pair()], vec![column_of(&p, "public.p")]);
        let mb = with_domains(vec![], vec![b], vec![col("c", "integer")]);
        let changes = diff(&ma, &mb);
        assert!(changes.iter().any(|c| matches!(c, Change::AlterAttributeType { .. })), "{changes:?}");
        assert!(!changes.iter().any(|c| matches!(c, Change::UnsupportedCompositeChange { .. })), "{changes:?}");
    }
}
