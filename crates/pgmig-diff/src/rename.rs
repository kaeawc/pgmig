//! Rename hints: `-- pgmig:rename <kind> <old> <new>` in the desired-state SQL says that an object
//! `from` has is the one `to` has under a new name, so the plan renames it instead of dropping it
//! and creating another (which, for a table or a column, loses its data). The catalog cannot tell
//! the two apart, so a rename is never inferred: without a hint the diff is a drop and an add, and
//! it only suggests the hint to write ([`suggest`]).
//!
//! # Hints
//!
//! | directive | renames |
//! |---|---|
//! | `rename table <schema>.<old> <new>` | a table (also a partitioned table or a partition) |
//! | `rename column <schema>.<table>.<old> <new>` | a column; the table is named as in the desired state |
//! | `rename enum <schema>.<old> <new>` | an enum type |
//! | `rename domain <schema>.<old> <new>` | a domain |
//! | `rename type <schema>.<old> <new>` | a composite type |
//! | `rename sequence <schema>.<old> <new>` | a sequence |
//! | `rename index <schema>.<old> <new>` | an index no constraint owns |
//!
//! Names follow SQL: unquoted ones are folded to lower case, `"Mixed Case"` keeps its case and `""`
//! is a quote inside one. The old name is schema-qualified; the new one is bare, or qualified with
//! the same schema. A rename never moves an object to another schema (`SET SCHEMA` is not planned):
//! such a hint is refused ([`parse`]). Constraints and schemas have no hint: a constraint renamed
//! without any other change is already found (see the crate documentation), and a schema is not
//! renamed.
//!
//! # Applying hints
//!
//! [`resolve`] decides which hints apply to a pair of models. A hint applies when the old object
//! is in `a` and the new one in `b`, the new name is free in `a` and the old name is not in `b`
//! (and no earlier hint renames the same object or to the same name). Anything else is unused
//! ([`Resolution::unused`], with a reason): not an error, since a hint can stay in the files after
//! it ran, when `a` already has the new name.
//!
//! The diff then runs on `a` as it is after the renames, which the caller makes exact by running
//! the rename changes ([`Resolution::applied`], planned first) on a copy of `a` and inspecting it:
//! what a rename also changes (an index definition that names the column, a foreign key that names
//! the table) is what the server prints, not something to patch in the model. What differs besides
//! the name is diffed as usual, so a renamed column whose type changes is a `RENAME COLUMN` and
//! then an `ALTER COLUMN ... TYPE`.
//!
//! # Owned sequences follow their column
//!
//! A sequence `OWNED BY` a column (a `serial` column's) keeps its name when its table or column is
//! renamed, while the desired state names it after the new one (`users_id_seq` becomes
//! `accounts_id_seq`). Diffed as it is, that sequence would be dropped and another one created,
//! starting over, and the next row would take a value the table already has. So a table or column
//! hint also renames the sequences of `a` owned by the renamed column when `b` has one owned by
//! that column under another name (with the same checks as a `rename sequence` hint, and only if no
//! hint renames it already): the plan renames the sequence and its current value carries on. Such
//! a change has [`Change::RenameSequence::follows`] set, so that a policy skipping the rename of its
//! table or column skips it too. An identity column's sequence needs nothing: its name is never
//! compared.
//!
//! When no hint applies (the table was renamed outside pgmig, say), the plan drops the old sequence
//! and creates the new one, which starts over. [`suggest`] then names the `rename sequence` hint to
//! write, for a dropped and a created sequence owned by the same column; it is never applied
//! without the hint.

use std::collections::{BTreeMap, BTreeSet};

use pgmig_model::{Model, OwnedBy, QualName, quote_ident};
use serde::{Deserialize, Serialize};

use crate::Change;

/// A parsed `-- pgmig:rename` hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Rename {
    Table {
        from: QualName,
        to: String,
    },
    /// `table` is the table as the desired state names it.
    Column {
        table: QualName,
        from: String,
        to: String,
    },
    Enum {
        from: QualName,
        to: String,
    },
    Sequence {
        from: QualName,
        to: String,
    },
    Domain {
        from: QualName,
        to: String,
    },
    /// A composite type.
    Type {
        from: QualName,
        to: String,
    },
    Index {
        from: QualName,
        to: String,
    },
}

impl std::fmt::Display for Rename {
    /// The hint's arguments as `parse` reads them: `table public.old new`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let named = |f: &mut std::fmt::Formatter<'_>, kind: &str, from: &dyn std::fmt::Display, to: &str| {
            write!(f, "{kind} {from} {}", quote_ident(to))
        };
        match self {
            Rename::Table { from, to } => named(f, "table", from, to),
            Rename::Column { table, from, to } => named(f, "column", &format!("{table}.{}", quote_ident(from)), to),
            Rename::Enum { from, to } => named(f, "enum", from, to),
            Rename::Sequence { from, to } => named(f, "sequence", from, to),
            Rename::Domain { from, to } => named(f, "domain", from, to),
            Rename::Type { from, to } => named(f, "type", from, to),
            Rename::Index { from, to } => named(f, "index", from, to),
        }
    }
}

impl Rename {
    /// The kind keyword of the hint: `table`, `column`, ...
    pub fn kind(&self) -> &'static str {
        match self {
            Rename::Table { .. } => "table",
            Rename::Column { .. } => "column",
            Rename::Enum { .. } => "enum",
            Rename::Sequence { .. } => "sequence",
            Rename::Domain { .. } => "domain",
            Rename::Type { .. } => "type",
            Rename::Index { .. } => "index",
        }
    }

    /// The hint as it is written in a file: `-- pgmig:rename table public.old new`.
    pub fn directive(&self) -> String {
        format!("-- pgmig:rename {self}")
    }
}

const KINDS: &str = "table, column, enum, sequence, domain, type or index";

/// Parse the arguments of a `-- pgmig:rename` directive. The error says what is wrong with them.
pub fn parse(args: &str) -> Result<Rename, String> {
    let args = args.trim();
    let (kind, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    if kind.is_empty() {
        return Err(format!("expected `rename <kind> <old name> <new name>` where kind is {KINDS}"));
    }
    let kind = kind.to_ascii_lowercase();
    if !matches!(kind.as_str(), "table" | "column" | "enum" | "sequence" | "domain" | "type" | "index") {
        return Err(format!("unknown kind `{kind}` to rename (expected {KINDS})"));
    }
    let (old, rest) = path(rest)?.ok_or_else(|| format!("rename {kind}: expected the old name, then the new one"))?;
    let (new, rest) = path(rest)?.ok_or_else(|| format!("rename {kind}: expected the new name after the old one"))?;
    if let Some(extra) = rest.split_whitespace().next() {
        return Err(format!("rename {kind}: unexpected `{extra}` after the new name"));
    }
    let want = if kind == "column" { 3 } else { 2 };
    if old.len() != want {
        let form = if kind == "column" { "<schema>.<table>.<column>" } else { "<schema>.<name>" };
        return Err(format!("rename {kind}: the old name must be {form}, not `{}`", old.join(".")));
    }
    // The new name is bare, or qualified like the old one: an object keeps its schema.
    let container = &old[..want - 1];
    let to = match new.as_slice() {
        [name] => name.clone(),
        [prefix @ .., name] if prefix == container => name.clone(),
        _ if new.len() == want => {
            return Err(format!(
                "rename {kind}: `{}` is in another place than `{}`; a rename keeps the object where it is (moving it to another schema is not planned)",
                new.join("."),
                old.join(".")
            ));
        }
        _ => return Err(format!("rename {kind}: the new name must be a bare name, not `{}`", new.join("."))),
    };
    let from_name = || QualName::new(old[0].clone(), old[1].clone());
    Ok(match kind.as_str() {
        "table" => Rename::Table { from: from_name(), to },
        "column" => Rename::Column { table: from_name(), from: old[2].clone(), to },
        "enum" => Rename::Enum { from: from_name(), to },
        "sequence" => Rename::Sequence { from: from_name(), to },
        "domain" => Rename::Domain { from: from_name(), to },
        "type" => Rename::Type { from: from_name(), to },
        _ => Rename::Index { from: from_name(), to },
    })
}

/// A dotted name at the start of `s` (after whitespace), and the rest; `None` when `s` has only
/// whitespace left. Unquoted identifiers are folded to lower case.
fn path(s: &str) -> Result<Option<(Vec<String>, &str)>, String> {
    let mut rest = s.trim_start();
    if rest.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    loop {
        let (part, after) = identifier(rest)?;
        parts.push(part);
        match after.strip_prefix('.') {
            Some(next) => rest = next,
            None => {
                rest = after;
                break;
            }
        }
    }
    if let Some(c) = rest.chars().next().filter(|c| !c.is_whitespace()) {
        return Err(format!("unexpected `{c}` in a name; quote it (\"...\") if it is part of the name"));
    }
    Ok(Some((parts, rest)))
}

/// One identifier at the start of `s`, and what follows it.
fn identifier(s: &str) -> Result<(String, &str), String> {
    if let Some(quoted) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = quoted.char_indices();
        while let Some((i, c)) = chars.next() {
            if c != '"' {
                out.push(c);
                continue;
            }
            // A doubled quote is a quote in the name; a single one ends it.
            if quoted[i + 1..].starts_with('"') {
                out.push('"');
                chars.next();
                continue;
            }
            if out.is_empty() {
                return Err("a quoted name cannot be empty".to_owned());
            }
            return Ok((out, &quoted[i + 1..]));
        }
        return Err("unterminated quoted name".to_owned());
    }
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(s.len());
    let (word, after) = s.split_at(end);
    if word.is_empty() || word.starts_with(|c: char| c.is_ascii_digit() || c == '$') {
        return Err(match s.chars().next() {
            Some(c) => format!("expected a name, found `{c}`"),
            None => "expected a name".to_owned(),
        });
    }
    Ok((word.to_ascii_lowercase(), after))
}

/// Which hints of a list apply to a pair of models ([`resolve`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// The hints that apply, as the index of the hint and its change, in the order a plan runs
    /// them: types and tables, then columns, then indexes (which are named by their table).
    pub applied: Vec<(usize, Change)>,
    /// The hints that do not apply, by index, and why.
    pub unused: Vec<(usize, String)>,
}

/// The kinds that are namespaces of their own for the checks of [`resolve`].
struct Names<'a> {
    a: &'a Model,
    b: &'a Model,
}

/// Whether `m` has a table, enum, domain or composite type of this name (they share `pg_type`).
fn has_type(m: &Model, n: &QualName) -> bool {
    m.tables.contains_key(n) || m.enums.contains_key(n) || m.domains.contains_key(n) || m.composites.contains_key(n)
}

/// Whether `m` has a relation of this name: a table, sequence, composite type or index (also the
/// one a constraint owns), which share `pg_class`.
fn has_relation(m: &Model, n: &QualName) -> bool {
    m.tables.contains_key(n)
        || m.sequences.contains_key(n)
        || m.composites.contains_key(n)
        || m.tables.values().any(|t| {
            t.name.schema == n.schema
                && (t.index(&n.name).is_some() || t.constraints.iter().any(|c| c.name == n.name && c.index_definition.is_some()))
        })
}

/// The table of `m` in `schema` that has a standalone index of this name.
fn index_table<'a>(m: &'a Model, n: &QualName) -> Option<&'a QualName> {
    m.tables.values().find(|t| t.name.schema == n.schema && t.index(&n.name).is_some()).map(|t| &t.name)
}

fn rank(r: &Rename) -> u8 {
    match r {
        Rename::Column { .. } => 1,
        Rename::Index { .. } => 2,
        _ => 0,
    }
}

/// Which of `renames` apply to `a` and `b` (see the module doc).
pub fn resolve(a: &Model, b: &Model, renames: &[Rename]) -> Resolution {
    let names = Names { a, b };
    let mut out = Resolution::default();
    // What earlier hints renamed, by kind and name, and the tables among them by new name.
    let mut from_taken: BTreeSet<String> = BTreeSet::new();
    let mut to_taken: BTreeSet<String> = BTreeSet::new();
    let mut tables: BTreeMap<QualName, QualName> = BTreeMap::new();
    let mut order: Vec<usize> = (0..renames.len()).collect();
    order.sort_by_key(|&i| (rank(&renames[i]), i));
    for i in order {
        let r = &renames[i];
        match names.check(r, &tables, &from_taken, &to_taken) {
            Ok((change, from, to)) => {
                if let Change::RenameTable { from, to } = &change {
                    tables.insert(QualName::new(from.schema.clone(), to.clone()), from.clone());
                }
                from_taken.insert(from);
                to_taken.insert(to);
                out.applied.push((i, change));
            }
            Err(reason) => out.unused.push((i, reason)),
        }
    }
    owned_sequences(a, b, &mut out.applied, &from_taken, &to_taken);
    out.unused.sort_by_key(|(i, _)| *i);
    out
}

/// Adds to `applied` the renames of the sequences that follow their column (see the module doc):
/// a sequence of `a` owned by a column a hint renames (or a column of a table a hint renames), when
/// `b` has, under another name, a sequence owned by that column as it is renamed, the new name is
/// free in `a`, the old name is not in `b`, and no hint renames either. Each comes under the index
/// of the hint that renames its table (or else its column), with the table renames, before columns.
fn owned_sequences(a: &Model, b: &Model, applied: &mut Vec<(usize, Change)>, from_taken: &BTreeSet<String>, to_taken: &BTreeSet<String>) {
    let mut follow = Vec::new();
    for seq in a.sequences.values() {
        let Some(owner) = &seq.owned_by else { continue };
        let (mut table, mut column, mut hint) = (owner.table.clone(), owner.column.clone(), None);
        for (i, change) in applied.iter() {
            match change {
                Change::RenameTable { from, to } if *from == table => {
                    table = QualName::new(from.schema.clone(), to.clone());
                    hint = Some(*i);
                }
                Change::RenameColumn { table: t, from, to } if *t == table && *from == column => {
                    column = to.clone();
                    hint = hint.or(Some(*i));
                }
                _ => {}
            }
        }
        let Some(hint) = hint else { continue };
        let owned = |s: &&pgmig_model::Sequence| s.owned_by.as_ref().is_some_and(|o| o.table == table && o.column == column);
        let Some(new) = b.sequences.values().find(owned) else { continue };
        if new.name == seq.name
            || new.name.schema != seq.name.schema
            || has_relation(a, &new.name)
            || b.sequences.contains_key(&seq.name)
            || from_taken.contains(&format!("sequence {}", seq.name))
            || to_taken.contains(&format!("sequence {}", new.name))
        {
            continue;
        }
        let follows = Some(OwnedBy { table, column });
        follow.push((hint, Change::RenameSequence { from: seq.name.clone(), to: new.name.name.clone(), follows }));
    }
    // With the other renames of rank 0, before the columns and indexes.
    let at =
        applied.iter().position(|(_, c)| matches!(c, Change::RenameColumn { .. } | Change::RenameIndex { .. })).unwrap_or(applied.len());
    applied.splice(at..at, follow);
}

impl Names<'_> {
    /// The change for `r` and the keys it uses up, or why `r` does not apply. `tables` maps the
    /// new names of tables earlier hints rename to their old names.
    fn check(
        &self,
        r: &Rename,
        tables: &BTreeMap<QualName, QualName>,
        from_taken: &BTreeSet<String>,
        to_taken: &BTreeSet<String>,
    ) -> Result<(Change, String, String), String> {
        let (a, b) = (self.a, self.b);
        let key = |what: &str, n: &dyn std::fmt::Display| format!("{what} {n}");
        // The checks every kind shares, for an object `from` renamed to `to`.
        let common = |what: &str,
                      from: &dyn std::fmt::Display,
                      to: &dyn std::fmt::Display,
                      in_a: bool,
                      in_b: bool,
                      new_in_a: bool,
                      old_in_b: bool|
         -> Result<(String, String), String> {
            let (kf, kt) = (key(what, from), key(what, to));
            if !in_a {
                let hint = if new_in_a && in_b { " (renamed already?)" } else { "" };
                return Err(format!("{kf} does not exist in the current schema{hint}"));
            }
            if !in_b {
                return Err(format!("{kt} does not exist in the desired schema"));
            }
            if from_taken.contains(&kf) {
                return Err(format!("an earlier hint already renames {kf}"));
            }
            if to_taken.contains(&kt) {
                return Err(format!("an earlier hint already renames another {what} to {to}"));
            }
            if new_in_a {
                return Err(format!("{kt} already exists in the current schema"));
            }
            if old_in_b {
                return Err(format!("{kf} still exists in the desired schema, so it is not renamed"));
            }
            Ok((kf, kt))
        };
        let with_schema = |from: &QualName, to: &str| QualName::new(from.schema.clone(), to);
        match r {
            Rename::Table { from, to } => {
                let t = with_schema(from, to);
                let (kf, kt) = common(
                    "table",
                    from,
                    &t,
                    a.tables.contains_key(from),
                    b.tables.contains_key(&t),
                    has_type(a, &t) || has_relation(a, &t),
                    b.tables.contains_key(from),
                )?;
                Ok((Change::RenameTable { from: from.clone(), to: to.clone() }, kf, kt))
            }
            Rename::Column { table, from, to } => {
                let old_table = tables.get(table).unwrap_or(table);
                let (at, bt) = (a.tables.get(old_table), b.tables.get(table));
                let qualified = |c: &str| format!("{table}.{}", quote_ident(c));
                let (kf, kt) = common(
                    "column",
                    &qualified(from),
                    &qualified(to),
                    at.is_some_and(|t| t.column(from).is_some()),
                    bt.is_some_and(|t| t.column(to).is_some()),
                    at.is_some_and(|t| t.column(to).is_some()),
                    bt.is_some_and(|t| t.column(from).is_some()),
                )?;
                Ok((Change::RenameColumn { table: table.clone(), from: from.clone(), to: to.clone() }, kf, kt))
            }
            Rename::Enum { from, to } => {
                let t = with_schema(from, to);
                let (kf, kt) = common(
                    "enum",
                    from,
                    &t,
                    a.enums.contains_key(from),
                    b.enums.contains_key(&t),
                    has_type(a, &t),
                    b.enums.contains_key(from),
                )?;
                Ok((Change::RenameEnum { from: from.clone(), to: to.clone() }, kf, kt))
            }
            Rename::Domain { from, to } => {
                let t = with_schema(from, to);
                let (kf, kt) = common(
                    "domain",
                    from,
                    &t,
                    a.domains.contains_key(from),
                    b.domains.contains_key(&t),
                    has_type(a, &t),
                    b.domains.contains_key(from),
                )?;
                Ok((Change::RenameDomain { from: from.clone(), to: to.clone() }, kf, kt))
            }
            Rename::Type { from, to } => {
                let t = with_schema(from, to);
                let (kf, kt) = common(
                    "type",
                    from,
                    &t,
                    a.composites.contains_key(from),
                    b.composites.contains_key(&t),
                    has_type(a, &t) || has_relation(a, &t),
                    b.composites.contains_key(from),
                )?;
                Ok((Change::RenameCompositeType { from: from.clone(), to: to.clone() }, kf, kt))
            }
            Rename::Sequence { from, to } => {
                let t = with_schema(from, to);
                let (kf, kt) = common(
                    "sequence",
                    from,
                    &t,
                    a.sequences.contains_key(from),
                    b.sequences.contains_key(&t),
                    has_relation(a, &t),
                    b.sequences.contains_key(from),
                )?;
                Ok((Change::RenameSequence { from: from.clone(), to: to.clone(), follows: None }, kf, kt))
            }
            Rename::Index { from, to } => {
                let t = with_schema(from, to);
                let (old_table, new_table) = (index_table(a, from), index_table(b, &t));
                let (kf, kt) = common(
                    "index",
                    from,
                    &t,
                    old_table.is_some(),
                    new_table.is_some(),
                    has_relation(a, &t),
                    index_table(b, from).is_some(),
                )?;
                let (old_table, new_table) = (old_table.expect("checked"), new_table.expect("checked"));
                // An index stays on its table, as that table is named once the plan has renamed it.
                let renamed = tables.iter().find(|(_, old)| *old == old_table).map_or(old_table, |(new, _)| new);
                if renamed != new_table {
                    return Err(format!("{kf} is on {old_table} but {kt} is on {new_table}: a rename keeps an index on its table"));
                }
                Ok((Change::RenameIndex { table: new_table.clone(), from: from.name.clone(), to: to.clone() }, kf, kt))
            }
        }
    }
}

/// A hint worth writing: the diff drops something and adds something else that looks like it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// The line to add to the desired state: `-- pgmig:rename table public.old new`.
    pub directive: String,
    /// Why it is suggested.
    pub message: String,
}

/// Rename hints to consider for `changes` (the diff from `a`): a table dropped while another
/// table with the same columns (names and types) is created in its schema, a column dropped
/// while another of the same type is added to the same table, and a sequence dropped while another
/// one owned by the same column is created in its schema. Nothing is renamed without the hint.
pub fn suggest(changes: &[Change], a: &Model) -> Vec<Suggestion> {
    let mut out = Vec::new();
    fn signature(t: &pgmig_model::Table) -> Vec<(&str, &str)> {
        let mut cols: Vec<(&str, &str)> = t.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
        cols.sort_unstable();
        cols
    }
    let created: Vec<&pgmig_model::Table> = changes
        .iter()
        .filter_map(|c| match c {
            Change::CreateTable(t) if t.partition_of.is_none() => Some(t),
            _ => None,
        })
        .collect();
    for change in changes {
        let Change::DropTable { table: old, .. } = change else { continue };
        if old.partition_of.is_some() {
            continue;
        }
        for new in created.iter().filter(|n| n.name.schema == old.name.schema && signature(n) == signature(old)) {
            let rename = Rename::Table { from: old.name.clone(), to: new.name.name.clone() };
            out.push(Suggestion {
                message: format!(
                    "table {} is dropped and table {} is created with the same columns; if it is a rename, the hint keeps the data",
                    old.name, new.name
                ),
                directive: rename.directive(),
            });
        }
    }
    for change in changes {
        let Change::DropColumn { table, column: old, .. } = change else { continue };
        let Some(type_name) = a.tables.get(table).and_then(|t| t.column(old)).map(|c| &c.type_name) else { continue };
        for added in changes.iter().filter_map(|c| match c {
            Change::AddColumn { table: t, column } if t == table && column.type_name == *type_name => Some(column),
            _ => None,
        }) {
            let rename = Rename::Column { table: table.clone(), from: old.clone(), to: added.name.clone() };
            out.push(Suggestion {
                message: format!(
                    "column {table}.{} is dropped and column {} of the same type ({type_name}) is added; if it is a rename, the hint keeps the data",
                    quote_ident(old),
                    quote_ident(&added.name)
                ),
                directive: rename.directive(),
            });
        }
    }
    // A sequence dropped while another one owned by the same column is created: the table was
    // renamed outside pgmig (or the sequence was named differently), and the new one starts over.
    for change in changes {
        let Change::DropSequence(old) = change else { continue };
        let Some(owner) = &old.owned_by else { continue };
        for new in changes.iter().filter_map(|c| match c {
            Change::CreateSequence(s) if s.name.schema == old.name.schema && s.owned_by.as_ref() == Some(owner) => Some(s),
            _ => None,
        }) {
            let rename = Rename::Sequence { from: old.name.clone(), to: new.name.name.clone() };
            out.push(Suggestion {
                message: format!(
                    "sequence {} is dropped and sequence {} is created, both owned by column {}.{}; if it is a rename, the hint keeps its current value",
                    old.name,
                    new.name,
                    owner.table,
                    quote_ident(&owner.column)
                ),
                directive: rename.directive(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pgmig_model::{Column, Table};

    use super::*;

    fn q(schema: &str, name: &str) -> QualName {
        QualName::new(schema, name)
    }

    #[test]
    fn parse_reads_each_kind() {
        assert_eq!(parse("table public.users accounts"), Ok(Rename::Table { from: q("public", "users"), to: "accounts".into() }));
        assert_eq!(
            parse("  COLUMN public.users.name full_name  "),
            Ok(Rename::Column { table: q("public", "users"), from: "name".into(), to: "full_name".into() })
        );
        assert_eq!(parse("enum app.Mood app.feeling"), Ok(Rename::Enum { from: q("app", "mood"), to: "feeling".into() }));
        assert_eq!(parse("sequence public.a b"), Ok(Rename::Sequence { from: q("public", "a"), to: "b".into() }));
        assert_eq!(parse("domain public.a b"), Ok(Rename::Domain { from: q("public", "a"), to: "b".into() }));
        assert_eq!(parse("type public.a b"), Ok(Rename::Type { from: q("public", "a"), to: "b".into() }));
        assert_eq!(parse("index public.a b"), Ok(Rename::Index { from: q("public", "a"), to: "b".into() }));
    }

    #[test]
    fn parse_follows_sql_quoting() {
        assert_eq!(parse("table public.a x.y.z"), Err("rename table: the new name must be a bare name, not `x.y.z`".to_owned()));
        assert_eq!(
            parse(r##"table "My App"."Mixed Case" "Say ""hi""""##),
            Ok(Rename::Table { from: q("My App", "Mixed Case"), to: "Say \"hi\"".into() })
        );
        assert_eq!(parse("table public.USERS Accounts"), Ok(Rename::Table { from: q("public", "users"), to: "accounts".into() }));
        // A dot inside quotes is part of the name.
        assert_eq!(parse(r#"table public."a.b" c"#), Ok(Rename::Table { from: q("public", "a.b"), to: "c".into() }));
    }

    #[test]
    fn rename_displays_as_it_parses() {
        for args in [
            "table public.users accounts",
            "column public.users.name full_name",
            r#"table "My App"."Mixed Case" "Other Name""#,
            r#"column public.t."select" "from""#,
            "enum app.mood feeling",
            "index public.a_idx b_idx",
        ] {
            assert_eq!(parse(args).expect(args).to_string(), args);
        }
        assert_eq!(parse("table public.a b").expect("parse").directive(), "-- pgmig:rename table public.a b");
    }

    #[test]
    fn parse_says_what_is_wrong() {
        let cases = [
            ("", "expected `rename <kind> <old name> <new name>` where kind is table, column, enum, sequence, domain, type or index"),
            ("view public.v w", "unknown kind `view` to rename (expected table, column, enum, sequence, domain, type or index)"),
            ("table", "rename table: expected the old name, then the new one"),
            ("table public.a", "rename table: expected the new name after the old one"),
            ("table a b", "rename table: the old name must be <schema>.<name>, not `a`"),
            ("column public.t b", "rename column: the old name must be <schema>.<table>.<column>, not `public.t`"),
            ("table public.a b c", "rename table: unexpected `c` after the new name"),
            (
                "table public.a other.b",
                "rename table: `other.b` is in another place than `public.a`; a rename keeps the object where it is (moving it to another schema is not planned)",
            ),
            ("table public.a public.b.c", "rename table: the new name must be a bare name, not `public.b.c`"),
            ("table public.a b;", "unexpected `;` in a name; quote it (\"...\") if it is part of the name"),
            ("table public.a \"b", "unterminated quoted name"),
            ("table public.a \"\"", "a quoted name cannot be empty"),
            ("table public.a 1b", "expected a name, found `1`"),
            ("table public..a b", "expected a name, found `.`"),
        ];
        for (args, want) in cases {
            assert_eq!(parse(args), Err(want.to_owned()), "{args:?}");
        }
    }

    fn column(name: &str, type_name: &str) -> Column {
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

    fn model(tables: &[(&str, &[(&str, &str)])]) -> Model {
        let mut m = Model::empty_database();
        for (name, columns) in tables {
            let name = q("public", name);
            m.tables.insert(
                name.clone(),
                Table {
                    name,
                    columns: columns.iter().map(|(n, t)| column(n, t)).collect(),
                    comment: None,
                    constraints: vec![],
                    indexes: vec![],
                    partition_key: None,
                    partition_of: None,
                },
            );
        }
        m
    }

    #[test]
    fn a_hint_applies_when_the_old_object_is_in_a_and_the_new_one_in_b() {
        let a = model(&[("users", &[("id", "integer"), ("name", "text")])]);
        let b = model(&[("accounts", &[("id", "integer"), ("full_name", "text")])]);
        let hints = [parse("column public.accounts.name full_name").expect("parse"), parse("table public.users accounts").expect("parse")];
        let r = resolve(&a, &b, &hints);
        assert!(r.unused.is_empty(), "{:?}", r.unused);
        // The table first, then the column, which is named by the table's new name.
        assert_eq!(
            r.applied,
            [
                (1, Change::RenameTable { from: q("public", "users"), to: "accounts".into() }),
                (0, Change::RenameColumn { table: q("public", "accounts"), from: "name".into(), to: "full_name".into() }),
            ]
        );
    }

    #[test]
    fn a_hint_that_does_not_apply_is_unused_with_a_reason() {
        let a = model(&[("accounts", &[("id", "integer")]), ("kept", &[("id", "integer")])]);
        let b = model(&[("accounts", &[("id", "integer")]), ("kept", &[("id", "integer")]), ("other", &[("id", "integer")])]);
        let cases = [
            // `a` has the new name already: the hint ran.
            ("table public.users accounts", "table public.users does not exist in the current schema (renamed already?)"),
            ("table public.gone other", "table public.gone does not exist in the current schema"),
            ("table public.kept nowhere", "table public.nowhere does not exist in the desired schema"),
            ("table public.accounts kept", "table public.kept already exists in the current schema"),
            ("table public.accounts other", "table public.accounts still exists in the desired schema, so it is not renamed"),
            ("column public.kept.id key", "column public.kept.key does not exist in the desired schema"),
            ("column public.nowhere.id key", "column public.nowhere.id does not exist in the current schema"),
            ("enum public.e f", "enum public.e does not exist in the current schema"),
            ("index public.i j", "index public.i does not exist in the current schema"),
        ];
        for (args, reason) in cases {
            let r = resolve(&a, &b, &[parse(args).expect(args)]);
            assert!(r.applied.is_empty(), "{args}");
            assert_eq!(r.unused, [(0, reason.to_owned())], "{args}");
        }
    }

    #[test]
    fn two_hints_cannot_rename_one_object_or_to_one_name() {
        let a = model(&[("s", &[("id", "integer")]), ("t", &[("id", "integer")])]);
        let b = model(&[("u", &[("id", "integer")]), ("v", &[("id", "integer")])]);
        let hints = [
            parse("table public.t u").expect("parse"),
            parse("table public.t v").expect("parse"),
            parse("table public.s u").expect("parse"),
        ];
        let r = resolve(&a, &b, &hints);
        assert_eq!(r.applied, [(0, Change::RenameTable { from: q("public", "t"), to: "u".into() })]);
        assert_eq!(
            r.unused,
            [
                (1, "an earlier hint already renames table public.t".to_owned()),
                (2, "an earlier hint already renames another table to public.u".to_owned()),
            ]
        );
    }

    #[test]
    fn an_index_stays_on_its_table() {
        let mut a = model(&[("t", &[("id", "integer")]), ("u", &[("id", "integer")])]);
        let mut b = a.clone();
        let index = |name: &str| pgmig_model::Index {
            name: name.into(),
            definition: String::new(),
            unique: false,
            columns: vec!["id".into()],
            comment: None,
            on_partitioned_table: false,
        };
        a.tables.get_mut(&q("public", "t")).expect("t").indexes.push(index("old_idx"));
        b.tables.get_mut(&q("public", "u")).expect("u").indexes.push(index("new_idx"));
        let r = resolve(&a, &b, &[parse("index public.old_idx new_idx").expect("parse")]);
        assert_eq!(
            r.unused,
            [(
                0,
                "index public.old_idx is on public.t but index public.new_idx is on public.u: a rename keeps an index on its table"
                    .to_owned()
            )]
        );
        let mut b = a.clone();
        b.tables.get_mut(&q("public", "t")).expect("t").indexes[0].name = "new_idx".into();
        let r = resolve(&a, &b, &[parse("index public.old_idx new_idx").expect("parse")]);
        assert_eq!(r.applied, [(0, Change::RenameIndex { table: q("public", "t"), from: "old_idx".into(), to: "new_idx".into() })]);
    }

    #[test]
    fn an_owned_sequence_follows_its_renamed_table_and_column() {
        let seq = |name: &str, table: &str, column: &str| pgmig_model::Sequence {
            name: q("public", name),
            data_type: "integer".into(),
            options: pgmig_model::SequenceOptions::defaults("integer", 1).expect("options"),
            owned_by: Some(pgmig_model::OwnedBy { table: q("public", table), column: column.into() }),
            comment: None,
        };
        let owner = |table: &str, column: &str| Some(pgmig_model::OwnedBy { table: q("public", table), column: column.into() });
        let with = |mut m: Model, seqs: &[pgmig_model::Sequence]| {
            for s in seqs {
                m.sequences.insert(s.name.clone(), s.clone());
            }
            m
        };
        let a = with(
            model(&[("users", &[("id", "integer"), ("n", "integer")])]),
            &[seq("users_id_seq", "users", "id"), seq("users_n_seq", "users", "n")],
        );
        let b = with(
            model(&[("accounts", &[("id", "integer"), ("m", "integer")])]),
            &[seq("accounts_id_seq", "accounts", "id"), seq("accounts_m_seq", "accounts", "m")],
        );
        let hints = [parse("column public.accounts.n m").expect("parse"), parse("table public.users accounts").expect("parse")];
        let r = resolve(&a, &b, &hints);
        assert!(r.unused.is_empty(), "{:?}", r.unused);
        assert_eq!(
            r.applied,
            [
                (1, Change::RenameTable { from: q("public", "users"), to: "accounts".into() }),
                (
                    1,
                    Change::RenameSequence {
                        from: q("public", "users_id_seq"),
                        to: "accounts_id_seq".into(),
                        follows: owner("accounts", "id")
                    }
                ),
                (
                    1,
                    Change::RenameSequence {
                        from: q("public", "users_n_seq"),
                        to: "accounts_m_seq".into(),
                        follows: owner("accounts", "m")
                    }
                ),
                (0, Change::RenameColumn { table: q("public", "accounts"), from: "n".into(), to: "m".into() }),
            ]
        );

        // A hint for the sequence wins; a sequence whose name stays (or that stays in `b`) is left.
        let hints = [parse("table public.users accounts").expect("parse"), parse("sequence public.users_id_seq other_seq").expect("parse")];
        let b2 = with(
            model(&[("accounts", &[("id", "integer"), ("n", "integer")])]),
            &[seq("other_seq", "accounts", "id"), seq("users_n_seq", "accounts", "n")],
        );
        let r = resolve(&a, &b2, &hints);
        assert_eq!(
            r.applied,
            [
                (0, Change::RenameTable { from: q("public", "users"), to: "accounts".into() }),
                (1, Change::RenameSequence { from: q("public", "users_id_seq"), to: "other_seq".into(), follows: None }),
            ]
        );
    }

    #[test]
    fn suggestions_name_the_hint_to_write() {
        let a = model(&[("users", &[("id", "integer"), ("name", "text")]), ("other", &[("x", "integer")])]);
        let b = model(&[("accounts", &[("name", "text"), ("id", "integer")])]);
        let changes = crate::diff(&a, &b);
        let s = suggest(&changes, &a);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].directive, "-- pgmig:rename table public.users accounts");
        assert!(s[0].message.starts_with("table public.users is dropped and table public.accounts is created"), "{}", s[0].message);

        let a = model(&[("t", &[("id", "integer"), ("name", "text")])]);
        let b = model(&[("t", &[("id", "integer"), ("full_name", "text"), ("n", "bigint")])]);
        let s = suggest(&crate::diff(&a, &b), &a);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].directive, "-- pgmig:rename column public.t.name full_name");
    }

    #[test]
    fn a_sequence_dropped_and_created_for_the_same_column_is_suggested() {
        let seq = |name: &str, table: &str, column: &str| pgmig_model::Sequence {
            name: q("public", name),
            data_type: "integer".into(),
            options: pgmig_model::SequenceOptions::defaults("integer", 1).expect("options"),
            owned_by: Some(pgmig_model::OwnedBy { table: q("public", table), column: column.into() }),
            comment: None,
        };
        let mut a = model(&[("accounts", &[("id", "integer"), ("n", "integer")])]);
        let mut b = a.clone();
        for s in [seq("users_id_seq", "accounts", "id"), seq("users_n_seq", "accounts", "n")] {
            a.sequences.insert(s.name.clone(), s);
        }
        for s in [seq("accounts_id_seq", "accounts", "id"), seq("other_seq", "other", "id")] {
            b.sequences.insert(s.name.clone(), s);
        }
        let s = suggest(&crate::diff(&a, &b), &a);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].directive, "-- pgmig:rename sequence public.users_id_seq accounts_id_seq");
        assert_eq!(
            s[0].message,
            "sequence public.users_id_seq is dropped and sequence public.accounts_id_seq is created, both owned by column public.accounts.id; if it is a rename, the hint keeps its current value"
        );
    }
}
