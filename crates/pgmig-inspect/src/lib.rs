//! Reads a database's catalog into a [`Model`].
//!
//! The same queries run against pgmem and real Postgres, and only `pg_catalog` is used:
//! `information_schema` hides objects the current user doesn't own.

use pgmig_conn::{Conn, Row};
use std::collections::{BTreeMap, BTreeSet};

use pgmig_model::{
    Attribute, Column, Composite, Constraint, ConstraintKind, Domain, DomainConstraint, Enum, Generated, Identity, Index, Model, OwnedBy,
    PartitionOf, QualName, References, Schema, Sequence, SequenceOptions, Table, inherited_constraint, inherited_index,
    partition_column_differs, quote_literal,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("inspecting {what}: {source}")]
    Query { what: &'static str, source: pgmig_conn::Error },
    #[error("inspecting {what}: unexpected NULL in column {column}")]
    Null { what: &'static str, column: usize },
    #[error("inspecting {what}: column {column} is not a number: {value}")]
    Number { what: &'static str, column: usize, value: String },
}

pub type Result<T> = std::result::Result<T, Error>;

const USER_SCHEMA: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema') AND left(n.nspname, 3) <> 'pg_'";

/// The model of the database `conn` is connected to.
///
/// Server functions (`format_type`, `pg_get_expr`, `pg_get_constraintdef`, `regclass` output) leave
/// out the schema of an object on the session's `search_path`, so the same schema would print
/// differently under different settings, and printed text such as `REFERENCES b(id)` could bind to
/// another `b` when executed elsewhere. Inspection therefore sets the session's `search_path` to
/// `pg_catalog` alone, so every name in a user schema prints qualified, and restores it afterwards,
/// also when a query fails. It opens no transaction, so a transaction the session is in (one a
/// desired-state file left open, say) keeps its work.
pub fn inspect(conn: &mut Conn) -> Result<Model> {
    let mut guard = SearchPath::set(conn, "pg_catalog")?;
    let model = inspect_catalog(guard.conn);
    let restored = guard.restore();
    let model = model?;
    restored?;
    Ok(model)
}

/// The session's `search_path`, replaced for inspection; dropping it puts the saved value back.
struct SearchPath<'a> {
    conn: &'a mut Conn,
    saved: Option<String>,
}

impl<'a> SearchPath<'a> {
    fn set(conn: &'a mut Conn, value: &str) -> Result<SearchPath<'a>> {
        let rows = run(conn, "search path", "SELECT current_setting('search_path')")?;
        let saved = rows.first().map(|row| text(row, 0, "search path")).transpose()?.unwrap_or_default();
        set_search_path(conn, value)?;
        Ok(SearchPath { conn, saved: Some(saved) })
    }

    fn restore(&mut self) -> Result<()> {
        match self.saved.take() {
            Some(saved) => set_search_path(self.conn, &saved),
            None => Ok(()),
        }
    }
}

impl Drop for SearchPath<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn set_search_path(conn: &mut Conn, value: &str) -> Result<()> {
    let sql = format!("SELECT set_config('search_path', {}, false)", quote_literal(value));
    run(conn, "search path", &sql).map(drop)
}

fn inspect_catalog(conn: &mut Conn) -> Result<Model> {
    let mut model = Model::default();

    let schemas = format!("SELECT n.nspname, obj_description(n.oid, 'pg_namespace') FROM pg_namespace n WHERE {USER_SCHEMA} ORDER BY 1");
    for row in run(conn, "schemas", &schemas)? {
        let name = text(&row, 0, "schemas")?;
        model.schemas.insert(name.clone(), Schema { name, comment: row.get(1).cloned().flatten() });
    }

    // A partition's parent is its one `pg_inherits` row; `pg_get_partkeydef` is NULL for a table
    // that is not partitioned.
    let tables = format!(
        "SELECT n.nspname, c.relname, obj_description(c.oid, 'pg_class'), pg_get_partkeydef(c.oid), pn.nspname, pc.relname, \
                pg_get_expr(c.relpartbound, c.oid) \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_inherits h ON c.relispartition AND h.inhrelid = c.oid \
         LEFT JOIN pg_class pc ON pc.oid = h.inhparent \
         LEFT JOIN pg_namespace pn ON pn.oid = pc.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND {USER_SCHEMA} ORDER BY 1, 2"
    );
    for row in run(conn, "tables", &tables)? {
        let what = "tables";
        let name = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        let partition_of = match (qual_name(&row, 4, 5), row.get(6).cloned().flatten()) {
            (Some(parent), Some(bound)) => Some(PartitionOf { parent, bound }),
            _ => None,
        };
        model.tables.insert(
            name.clone(),
            Table {
                name,
                columns: Vec::new(),
                comment: row.get(2).cloned().flatten(),
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_key: row.get(3).cloned().flatten(),
                partition_of,
            },
        );
    }

    let enums = format!(
        "SELECT n.nspname, t.typname, obj_description(t.oid, 'pg_type'), e.enumlabel \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         LEFT JOIN pg_enum e ON e.enumtypid = t.oid \
         WHERE t.typtype = 'e' AND {USER_SCHEMA} ORDER BY 1, 2, e.enumsortorder"
    );
    for row in run(conn, "enums", &enums)? {
        let name = QualName::new(text(&row, 0, "enums")?, text(&row, 1, "enums")?);
        let e =
            model.enums.entry(name.clone()).or_insert_with(|| Enum { name, labels: Vec::new(), comment: row.get(2).cloned().flatten() });
        // NULL for an enum with no values.
        if let Some(label) = row.get(3).cloned().flatten() {
            e.labels.push(label);
        }
    }

    inspect_domains(conn, &mut model)?;
    inspect_composites(conn, &mut model)?;

    // Sequences with what owns them: `a` is `OWNED BY` (standalone), `i` an identity column's.
    let sequences = format!(
        "SELECT n.nspname, c.relname, format_type(s.seqtypid, NULL), s.seqstart, s.seqincrement, s.seqmin, s.seqmax, \
                s.seqcache, s.seqcycle, obj_description(c.oid, 'pg_class'), dep.deptype, dn.nspname, dt.relname, da.attname \
         FROM pg_sequence s \
         JOIN pg_class c ON c.oid = s.seqrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_depend dep ON dep.classid = 'pg_class'::regclass AND dep.objid = c.oid \
              AND dep.refclassid = 'pg_class'::regclass AND dep.deptype IN ('a', 'i') \
         LEFT JOIN pg_class dt ON dt.oid = dep.refobjid \
         LEFT JOIN pg_namespace dn ON dn.oid = dt.relnamespace \
         LEFT JOIN pg_attribute da ON da.attrelid = dep.refobjid AND da.attnum = dep.refobjsubid \
         WHERE {USER_SCHEMA} ORDER BY 1, 2"
    );
    let mut identities: BTreeMap<(QualName, String), (SequenceOptions, QualName)> = BTreeMap::new();
    for row in run(conn, "sequences", &sequences)? {
        let what = "sequences";
        let options = SequenceOptions {
            start: number(&row, 3, what)?,
            increment: number(&row, 4, what)?,
            min: number(&row, 5, what)?,
            max: number(&row, 6, what)?,
            cache: number(&row, 7, what)?,
            cycle: text(&row, 8, what)? == "t",
        };
        let owner = match (row.get(11).cloned().flatten(), row.get(12).cloned().flatten(), row.get(13).cloned().flatten()) {
            (Some(schema), Some(table), Some(column)) => Some(OwnedBy { table: QualName::new(schema, table), column }),
            _ => None,
        };
        let name = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        let deptype = row.get(10).cloned().flatten();
        if deptype.as_deref() == Some("i") {
            if let Some(o) = owner {
                identities.insert((o.table, o.column), (options, name));
            }
            continue;
        }
        let sequence = Sequence {
            name: name.clone(),
            data_type: text(&row, 2, what)?,
            options,
            owned_by: owner,
            comment: row.get(9).cloned().flatten(),
        };
        model.sequences.insert(name, sequence);
    }

    let columns = format!(
        "SELECT n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                pg_get_expr(d.adbin, d.adrelid), tn.nspname, COALESCE(e.typname, t.typname), \
                col_description(a.attrelid, a.attnum), a.attidentity, a.attnum, a.attinhcount > 0 \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         LEFT JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_type e ON e.typarray = a.atttypid \
         LEFT JOIN pg_namespace tn ON tn.oid = COALESCE(e.typnamespace, t.typnamespace) \
         WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped AND {USER_SCHEMA} \
         ORDER BY 1, 2, a.attnum"
    );
    let mut resolved: BTreeMap<String, Option<QualName>> = BTreeMap::new();
    let mut attnames: BTreeMap<(QualName, String), String> = BTreeMap::new();
    // Tables with a column the catalog marks as inherited (see [`keep_partitions_own`]).
    let mut marked: BTreeSet<QualName> = BTreeSet::new();
    for row in run(conn, "columns", &columns)? {
        let table = QualName::new(text(&row, 0, "columns")?, text(&row, 1, "columns")?);
        let name = text(&row, 2, "columns")?;
        attnames.insert((table.clone(), text(&row, 10, "columns")?), name.clone());
        if text(&row, 11, "columns")? == "t" {
            marked.insert(table.clone());
        }
        let default = match row.get(5).cloned().flatten() {
            Some(expr) => Some(qualify_regclass_literals(&expr, |literal| resolve_cached(conn, &mut resolved, literal))?),
            None => None,
        };
        let mut default_sequences = Vec::new();
        for literal in default.as_deref().map(nextval_arguments).unwrap_or_default() {
            if let Some(seq) = resolve_cached(conn, &mut resolved, &literal)?
                && !default_sequences.contains(&seq)
            {
                default_sequences.push(seq);
            }
        }
        let generated = match row.get(9).cloned().flatten().as_deref() {
            Some("a") => Some(Generated::Always),
            Some("d") => Some(Generated::ByDefault),
            _ => None,
        };
        let identity = generated.and_then(|generated| {
            let (options, sequence) = identities.get(&(table.clone(), name.clone())).cloned()?;
            Some(Identity { generated, options, sequence: Some(sequence) })
        });
        let column = Column {
            name,
            type_name: text(&row, 3, "columns")?,
            not_null: text(&row, 4, "columns")? == "t",
            default,
            type_ref: type_ref(&row),
            comment: row.get(8).cloned().flatten(),
            default_sequences,
            identity,
        };
        if let Some(t) = model.tables.get_mut(&table) {
            t.columns.push(column);
        }
    }

    // A constraint's schema is its table's: pgmem reports `connamespace` as `public` for every
    // constraint (kaeawc/pgmem#147), and Postgres always puts a table's constraints in its schema.
    // `NOT NULL` constraints (`n`, Postgres 18) are `attnotnull`; inherited ones are the parent's.
    let constraints = format!(
        "SELECT n.nspname, cl.relname, co.conname, co.contype, pg_get_constraintdef(co.oid), co.condeferrable, co.condeferred, \
                co.convalidated, co.conkey, fn.nspname, fc.relname, co.confkey, ci.relname, obj_description(co.oid, 'pg_constraint'), \
                CASE WHEN co.contype IN ('p', 'u') THEN pg_get_indexdef(co.conindid) END, cl.relkind = 'p' \
         FROM pg_constraint co \
         JOIN pg_class cl ON cl.oid = co.conrelid \
         JOIN pg_namespace n ON n.oid = cl.relnamespace \
         LEFT JOIN pg_class fc ON fc.oid = co.confrelid \
         LEFT JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
         LEFT JOIN pg_class ci ON ci.oid = co.conindid \
         WHERE co.contype IN ('p', 'u', 'c', 'f', 'x') AND co.conislocal AND cl.relkind IN ('r', 'p') AND {USER_SCHEMA} \
           AND NOT (cl.relispartition AND (co.coninhcount > 0 OR co.conparentid <> 0)) \
         ORDER BY 1, 2, 3"
    );
    for row in run(conn, "constraints", &constraints)? {
        let what = "constraints";
        let table = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        let kind = match text(&row, 3, what)?.as_str() {
            "p" => ConstraintKind::PrimaryKey,
            "u" => ConstraintKind::Unique,
            "c" => ConstraintKind::Check,
            "f" => ConstraintKind::ForeignKey,
            _ => ConstraintKind::Exclusion,
        };
        let names = |table: &QualName, column: usize| -> Vec<String> {
            let numbers = row.get(column).cloned().flatten().unwrap_or_default();
            int_array(&numbers).filter_map(|n| attnames.get(&(table.clone(), n)).cloned()).collect()
        };
        let references = match (kind, row.get(9).cloned().flatten(), row.get(10).cloned().flatten()) {
            (ConstraintKind::ForeignKey, Some(schema), Some(name)) => {
                let referenced = QualName::new(schema, name);
                Some(References { columns: names(&referenced, 11), table: referenced, constraint: row.get(12).cloned().flatten() })
            }
            _ => None,
        };
        let constraint = Constraint {
            name: text(&row, 2, what)?,
            kind,
            definition: text(&row, 4, what)?,
            deferrable: text(&row, 5, what)? == "t",
            deferred: text(&row, 6, what)? == "t",
            validated: text(&row, 7, what)? == "t",
            columns: names(&table, 8),
            references,
            comment: row.get(13).cloned().flatten(),
            index_definition: row.get(14).cloned().flatten(),
            on_partitioned_table: text(&row, 15, what)? == "t",
        };
        if let Some(t) = model.tables.get_mut(&table) {
            t.constraints.push(constraint);
        }
    }
    // By name in byte order, whatever the server's collation.
    for t in model.tables.values_mut() {
        t.constraints.sort_by(|x, y| x.name.cmp(&y.name));
    }

    inspect_indexes(conn, &mut model, &attnames)?;
    keep_partitions_own(&mut model, &marked);

    Ok(model)
}

/// Leaves each partition with only what is its own (see [`Table::partition_of`]).
///
/// The catalog marks what a partition has from its parent: a column's `attinhcount`, a
/// constraint's `coninhcount` or `conparentid` (`conislocal` is false for those), an index attached
/// to its parent's in `pg_inherits`. The constraint and index queries leave those out, so what is
/// left is the partition's own, even when it is defined like one of its parent's. Every column of a
/// partition is its parent's (`attinhcount > 0`; an attached table's columns too), so a column
/// stays only when it differs from the parent's ([`partition_column_differs`]).
///
/// pgmem leaves the marks unset: `attinhcount` 0 (kaeawc/pgmem#288), `coninhcount` and
/// `conparentid` 0 (kaeawc/pgmem#205), no `pg_inherits` row for an attached index
/// (kaeawc/pgmem#289). A partition none of whose columns is marked (on Postgres, a partition of a
/// table without columns, which has nothing to leave out) is compared with its parent instead:
/// constraints and indexes defined like the parent's ([`inherited_constraint`],
/// [`inherited_index`]) are taken for the parent's.
fn keep_partitions_own(model: &mut Model, marked: &BTreeSet<QualName>) {
    let full = model.tables.clone();
    for t in model.tables.values_mut() {
        let Some(parent) = t.partition_of.as_ref().and_then(|p| full.get(&p.parent)) else { continue };
        t.columns.retain(|c| parent.column(&c.name).is_none_or(|pc| partition_column_differs(c, pc)));
        if marked.contains(&t.name) {
            continue;
        }
        t.constraints.retain(|c| !parent.constraints.iter().any(|pc| inherited_constraint(pc, c)));
        let name = t.name.clone();
        t.indexes.retain(|i| !parent.indexes.iter().any(|pi| inherited_index(&parent.name, pi, &name, i)));
    }
}

/// The indexes of the model's tables that no constraint owns: not a primary key, unique or
/// exclusion constraint's (`conindid`; a foreign key's `conindid` is the index it references, which
/// stays), and not a partition's index attached to its parent's (`pg_inherits`), which Postgres
/// creates and drops with the parent's.
///
/// `attnames` maps `(table, attnum)` to column names.
fn inspect_indexes(conn: &mut Conn, model: &mut Model, attnames: &BTreeMap<(QualName, String), String>) -> Result<()> {
    let what = "indexes";
    let indexes = format!(
        "SELECT n.nspname, t.relname, i.relname, pg_get_indexdef(x.indexrelid), x.indisunique, x.indkey, \
                obj_description(i.oid, 'pg_class'), x.indexrelid, t.relkind = 'p' \
         FROM pg_index x \
         JOIN pg_class i ON i.oid = x.indexrelid \
         JOIN pg_class t ON t.oid = x.indrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         WHERE t.relkind IN ('r', 'p') AND {USER_SCHEMA} \
           AND NOT EXISTS (SELECT 1 FROM pg_constraint co WHERE co.conindid = x.indexrelid AND co.contype IN ('p', 'u', 'x')) \
           AND NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = x.indexrelid) \
         ORDER BY 1, 2, 3"
    );
    let mut by_oid: BTreeMap<String, (QualName, usize)> = BTreeMap::new();
    for row in run(conn, what, &indexes)? {
        let table = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        let mut columns: Vec<String> = Vec::new();
        for n in int_vector(&row.get(5).cloned().flatten().unwrap_or_default()) {
            if let Some(name) = attnames.get(&(table.clone(), n))
                && !columns.contains(name)
            {
                columns.push(name.clone());
            }
        }
        let index = Index {
            name: text(&row, 2, what)?,
            definition: text(&row, 3, what)?,
            unique: text(&row, 4, what)? == "t",
            columns,
            comment: row.get(6).cloned().flatten(),
            on_partitioned_table: text(&row, 8, what)? == "t",
        };
        if let Some(t) = model.tables.get_mut(&table) {
            by_oid.insert(text(&row, 7, what)?, (table, t.indexes.len()));
            t.indexes.push(index);
        }
    }
    // The columns expressions and predicates use, which `indkey` records as 0.
    let used = "SELECT d.objid, d.refobjsubid FROM pg_depend d JOIN pg_index x ON x.indexrelid = d.objid \
                WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass AND d.refobjid = x.indrelid \
                  AND d.refobjsubid > 0 ORDER BY 1, 2";
    for row in run(conn, what, used)? {
        let Some((table, at)) = by_oid.get(&text(&row, 0, what)?) else { continue };
        let Some(name) = attnames.get(&(table.clone(), text(&row, 1, what)?)) else { continue };
        if let Some(index) = model.tables.get_mut(table).and_then(|t| t.indexes.get_mut(*at))
            && !index.columns.contains(name)
        {
            index.columns.push(name.clone());
        }
    }
    for t in model.tables.values_mut() {
        t.indexes.sort_by(|x, y| x.name.cmp(&y.name));
    }
    Ok(())
}

/// Domains and their check constraints.
///
/// pgmem leaves `typdefaultbin` empty and keeps only `typdefault` (kaeawc/pgmem#265), so the
/// default falls back to it. On Postgres the two are set together, and `pg_get_expr` prints the
/// default under inspection's search path.
fn inspect_domains(conn: &mut Conn, model: &mut Model) -> Result<()> {
    let what = "domains";
    let domains = format!(
        "SELECT n.nspname, t.typname, format_type(t.typbasetype, t.typtypmod), bn.nspname, COALESCE(be.typname, bt.typname), \
                cn.nspname, cl.collname, COALESCE(pg_get_expr(t.typdefaultbin, 0), t.typdefault), t.typnotnull, \
                obj_description(t.oid, 'pg_type') \
         FROM pg_type t \
         JOIN pg_namespace n ON n.oid = t.typnamespace \
         JOIN pg_type bt ON bt.oid = t.typbasetype \
         LEFT JOIN pg_type be ON be.typarray = bt.oid \
         LEFT JOIN pg_namespace bn ON bn.oid = COALESCE(be.typnamespace, bt.typnamespace) \
         LEFT JOIN pg_collation cl ON cl.oid = t.typcollation AND t.typcollation <> bt.typcollation \
         LEFT JOIN pg_namespace cn ON cn.oid = cl.collnamespace \
         WHERE t.typtype = 'd' AND {USER_SCHEMA} ORDER BY 1, 2"
    );
    for row in run(conn, what, &domains)? {
        let name = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        let domain = Domain {
            name: name.clone(),
            base_type: text(&row, 2, what)?,
            base_ref: user_type(&row, 3, 4),
            collation: qual_name(&row, 5, 6),
            default: row.get(7).cloned().flatten(),
            not_null: text(&row, 8, what)? == "t",
            constraints: Vec::new(),
            comment: row.get(9).cloned().flatten(),
        };
        model.domains.insert(name, domain);
    }
    // Check constraints only: a domain's `NOT NULL` (a constraint of its own since Postgres 17) is
    // `typnotnull`.
    let constraints = format!(
        "SELECT n.nspname, t.typname, co.conname, pg_get_constraintdef(co.oid), co.convalidated, obj_description(co.oid, 'pg_constraint') \
         FROM pg_constraint co \
         JOIN pg_type t ON t.oid = co.contypid \
         JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE co.contype = 'c' AND t.typtype = 'd' AND {USER_SCHEMA} ORDER BY 1, 2, 3"
    );
    let what = "domain constraints";
    for row in run(conn, what, &constraints)? {
        let domain = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        let constraint = DomainConstraint {
            name: text(&row, 2, what)?,
            definition: text(&row, 3, what)?,
            validated: text(&row, 4, what)? == "t",
            comment: row.get(5).cloned().flatten(),
        };
        if let Some(d) = model.domains.get_mut(&domain) {
            d.constraints.push(constraint);
        }
    }
    // By name in byte order, whatever the server's collation.
    for d in model.domains.values_mut() {
        d.constraints.sort_by(|x, y| x.name.cmp(&y.name));
    }
    Ok(())
}

/// Composite types made with `CREATE TYPE ... AS`: their relation is of kind `c`, where a table's
/// row type has the table's.
fn inspect_composites(conn: &mut Conn, model: &mut Model) -> Result<()> {
    let what = "composite types";
    let composites = format!(
        "SELECT n.nspname, t.typname, obj_description(t.oid, 'pg_type') \
         FROM pg_type t \
         JOIN pg_namespace n ON n.oid = t.typnamespace \
         JOIN pg_class c ON c.oid = t.typrelid \
         WHERE t.typtype = 'c' AND c.relkind = 'c' AND {USER_SCHEMA} ORDER BY 1, 2"
    );
    for row in run(conn, what, &composites)? {
        let name = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        model.composites.insert(name.clone(), Composite { name, attributes: Vec::new(), comment: row.get(2).cloned().flatten() });
    }
    let what = "composite type attributes";
    let attributes = format!(
        "SELECT n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod), tn.nspname, COALESCE(e.typname, t.typname), \
                cn.nspname, cl.collname, col_description(a.attrelid, a.attnum) \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_type e ON e.typarray = a.atttypid \
         LEFT JOIN pg_namespace tn ON tn.oid = COALESCE(e.typnamespace, t.typnamespace) \
         LEFT JOIN pg_collation cl ON cl.oid = a.attcollation AND a.attcollation <> t.typcollation \
         LEFT JOIN pg_namespace cn ON cn.oid = cl.collnamespace \
         WHERE c.relkind = 'c' AND a.attnum > 0 AND NOT a.attisdropped AND {USER_SCHEMA} \
         ORDER BY 1, 2, a.attnum"
    );
    for row in run(conn, what, &attributes)? {
        let composite = QualName::new(text(&row, 0, what)?, text(&row, 1, what)?);
        let attribute = Attribute {
            name: text(&row, 2, what)?,
            type_name: text(&row, 3, what)?,
            type_ref: user_type(&row, 4, 5),
            collation: qual_name(&row, 6, 7),
            comment: row.get(8).cloned().flatten(),
        };
        if let Some(c) = model.composites.get_mut(&composite) {
            c.attributes.push(attribute);
        }
    }
    Ok(())
}

/// The name in columns `schema` and `name` of `row`, when both are set.
fn qual_name(row: &Row, schema: usize, name: usize) -> Option<QualName> {
    Some(QualName::new(row.get(schema).cloned().flatten()?, row.get(name).cloned().flatten()?))
}

/// The elements of an `int2vector` as the server prints it (`1 2`; pgmem prints `{1,2}`,
/// kaeawc/pgmem#170), as text.
fn int_vector(printed: &str) -> impl Iterator<Item = String> + '_ {
    printed.trim_start_matches('{').trim_end_matches('}').split([',', ' ']).filter(|n| !n.is_empty()).map(str::to_owned)
}

/// The elements of an `int2[]` as the server prints it (`{1,2}`), as text.
fn int_array(printed: &str) -> impl Iterator<Item = String> + '_ {
    printed.trim_start_matches('{').trim_end_matches('}').split(',').map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned)
}

/// The column's type when it lives in a user schema: columns 6 (schema) and 7 (name).
fn type_ref(row: &Row) -> Option<QualName> {
    user_type(row, 6, 7)
}

/// The type in columns `schema` and `name` of `row` when it lives in a user schema.
fn user_type(row: &Row, schema: usize, name: usize) -> Option<QualName> {
    let q = qual_name(row, schema, name)?;
    let system = q.schema == "pg_catalog" || q.schema == "information_schema" || q.schema.starts_with("pg_");
    (!system).then_some(q)
}

/// The arguments of `nextval('<name>'::regclass)` calls in an expression as `pg_get_expr` prints
/// it, unquoted from their literals. The server always prints the `regclass` cast, so this reads
/// only its own output. Postgres records the same references in `pg_depend`, but pgmem does not
/// (kaeawc/pgmem#155), and reading the expression gives the same answer on both.
pub fn nextval_arguments(expr: &str) -> Vec<String> {
    const CALL: &str = "nextval('";
    let mut out = Vec::new();
    let mut rest = expr;
    while let Some(at) = rest.find(CALL) {
        rest = &rest[at + CALL.len()..];
        let mut literal = String::new();
        let mut chars = rest.char_indices().peekable();
        let mut end = None;
        while let Some((i, ch)) = chars.next() {
            if ch == '\'' {
                if chars.peek().is_some_and(|&(_, next)| next == '\'') {
                    chars.next();
                    literal.push('\'');
                    continue;
                }
                end = Some(i + 1);
                break;
            }
            literal.push(ch);
        }
        let Some(end) = end else { break };
        rest = &rest[end..];
        if rest.starts_with("::regclass") {
            out.push(literal);
        }
    }
    out
}

/// The `'<name>'::regclass` literals in an expression as the server prints it: each literal's byte
/// range (quotes included) and its unquoted text. String literals and quoted identifiers are skipped
/// whole, so a quote inside them starts nothing.
fn regclass_literals(expr: &str) -> Vec<(std::ops::Range<usize>, String)> {
    const CAST: &str = "::regclass";
    let bytes = expr.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        if quote != b'\'' && quote != b'"' {
            i += 1;
            continue;
        }
        let start = i;
        let mut text = Vec::new();
        i += 1;
        let mut closed = false;
        while i < bytes.len() {
            if bytes[i] == quote {
                if bytes.get(i + 1) == Some(&quote) {
                    text.push(quote);
                    i += 2;
                    continue;
                }
                i += 1;
                closed = true;
                break;
            }
            text.push(bytes[i]);
            i += 1;
        }
        if !closed {
            break;
        }
        if quote == b'\'' && expr[i..].starts_with(CAST) {
            out.push((start..i, String::from_utf8_lossy(&text).into_owned()));
        }
    }
    out
}

/// `expr` with every unqualified `regclass` literal naming a user relation rewritten to the
/// qualified, quoted form the server prints under inspection's search path
/// (`nextval('public.t_id_seq'::regclass)`). `resolve` looks a literal up. On Postgres there is
/// nothing to rewrite; pgmem prints `public` relations unqualified (kaeawc/pgmem#226), which would
/// otherwise compare unequal to a Postgres target's defaults on every run.
fn qualify_regclass_literals(expr: &str, mut resolve: impl FnMut(&str) -> Result<Option<QualName>>) -> Result<String> {
    let mut out = String::new();
    let mut last = 0;
    for (range, literal) in regclass_literals(expr) {
        if !is_unqualified(&literal) {
            continue;
        }
        let Some(name) = resolve(&literal)? else { continue };
        if name.schema == "pg_catalog" {
            continue;
        }
        out.push_str(&expr[last..range.start]);
        out.push_str(&quote_literal(&name.to_string()));
        last = range.end;
    }
    out.push_str(&expr[last..]);
    Ok(out)
}

/// [`resolve_relation`], remembering answers in `resolved` by literal.
fn resolve_cached(conn: &mut Conn, resolved: &mut BTreeMap<String, Option<QualName>>, literal: &str) -> Result<Option<QualName>> {
    if let Some(found) = resolved.get(literal) {
        return Ok(found.clone());
    }
    let found = resolve_relation(conn, literal)?;
    resolved.insert(literal.to_owned(), found.clone());
    Ok(found)
}

/// The relation a `regclass` literal names, resolved by the server with inspection's search path,
/// where `regclass` output names every user relation with its schema.
///
/// pgmem prints a `public` relation unqualified whatever the search path (kaeawc/pgmem#226), so a
/// literal that resolves to nothing is also tried in `public`. On Postgres that never matches: an
/// unqualified literal there is a `pg_catalog` relation, which the first lookup finds.
fn resolve_relation(conn: &mut Conn, literal: &str) -> Result<Option<QualName>> {
    let found = lookup_relation(conn, literal)?;
    if found.is_some() || !is_unqualified(literal) {
        return Ok(found);
    }
    lookup_relation(conn, &format!("public.{literal}"))
}

fn lookup_relation(conn: &mut Conn, literal: &str) -> Result<Option<QualName>> {
    let sql = format!(
        "SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = to_regclass({})",
        quote_literal(literal)
    );
    let rows = run(conn, "column default sequences", &sql)?;
    Ok(match rows.first() {
        Some(row) => Some(QualName::new(text(row, 0, "column default sequences")?, text(row, 1, "column default sequences")?)),
        None => None,
    })
}

/// Whether a relation name as `regclass` prints it has no schema: no `.` outside double quotes.
fn is_unqualified(name: &str) -> bool {
    let mut quoted = false;
    for ch in name.chars() {
        match ch {
            '"' => quoted = !quoted,
            '.' if !quoted => return false,
            _ => {}
        }
    }
    true
}

fn number(row: &Row, column: usize, what: &'static str) -> Result<i64> {
    let value = text(row, column, what)?;
    value.parse().map_err(|_| Error::Number { what, column, value })
}

fn run(conn: &mut Conn, what: &'static str, sql: &str) -> Result<Vec<Row>> {
    conn.query(sql).map_err(|source| Error::Query { what, source })
}

fn text(row: &Row, column: usize, what: &'static str) -> Result<String> {
    row.get(column).cloned().flatten().ok_or(Error::Null { what, column })
}

#[cfg(test)]
mod tests {
    use super::{int_array, int_vector, is_unqualified, nextval_arguments, qualify_regclass_literals, regclass_literals};
    use pgmig_model::QualName;

    #[test]
    fn unqualified_regclass_literals_are_rewritten_qualified() {
        let resolve = |literal: &str| {
            Ok(match literal {
                "t_id_seq" => Some(QualName::new("public", "t_id_seq")),
                "\"Seq\"" => Some(QualName::new("Odd", "Seq")),
                "pg_class" => Some(QualName::new("pg_catalog", "pg_class")),
                _ => None,
            })
        };
        let q = |expr: &str| qualify_regclass_literals(expr, resolve).expect("rewrite");
        assert_eq!(q("nextval('t_id_seq'::regclass)"), "nextval('public.t_id_seq'::regclass)");
        assert_eq!(q("nextval('\"Seq\"'::regclass)"), "nextval('\"Odd\".\"Seq\"'::regclass)");
        assert_eq!(
            q("(nextval('t_id_seq'::regclass) + nextval('app.s'::regclass))"),
            "(nextval('public.t_id_seq'::regclass) + nextval('app.s'::regclass))"
        );
        assert_eq!(q("'pg_class'::regclass"), "'pg_class'::regclass");
        assert_eq!(q("nextval('missing'::regclass)"), "nextval('missing'::regclass)");
        assert_eq!(q("'t_id_seq'::text"), "'t_id_seq'::text");
    }

    #[test]
    fn regclass_literals_are_found_with_their_ranges() {
        let expr = "nextval('t_id_seq'::regclass)";
        let found = regclass_literals(expr);
        assert_eq!(found, [(8..18, "t_id_seq".to_owned())]);
        assert_eq!(&expr[found[0].0.clone()], "'t_id_seq'");
        assert_eq!(
            regclass_literals("(nextval('\"Odd\".\"It''s\"'::regclass) + 'x'::text::regclass::oid)"),
            [(9..24, "\"Odd\".\"It's\"".to_owned())]
        );
        assert!(regclass_literals("'nextval(''x''::regclass)'::text").is_empty());
        assert!(regclass_literals("(\"it's\" + 1)").is_empty());
        assert!(regclass_literals("'unterminated").is_empty());
    }

    #[test]
    fn qualified_names_are_told_apart() {
        assert!(is_unqualified("t_id_seq"));
        assert!(is_unqualified("\"a.b\""));
        assert!(!is_unqualified("app.counter"));
        assert!(!is_unqualified("\"App\".\"It's\""));
    }

    #[test]
    fn int_arrays_are_read_as_printed() {
        assert_eq!(int_array("{1,2}").collect::<Vec<_>>(), ["1", "2"]);
        assert_eq!(int_array("{3}").collect::<Vec<_>>(), ["3"]);
        assert!(int_array("{}").next().is_none());
        assert!(int_array("").next().is_none());
    }

    #[test]
    fn int_vectors_are_read_as_printed() {
        assert_eq!(int_vector("2 3").collect::<Vec<_>>(), ["2", "3"]);
        assert_eq!(int_vector("0").collect::<Vec<_>>(), ["0"]);
        assert_eq!(int_vector("{1,2}").collect::<Vec<_>>(), ["1", "2"]);
        assert!(int_vector("").next().is_none());
    }

    #[test]
    fn nextval_arguments_are_read_from_printed_defaults() {
        assert_eq!(nextval_arguments("nextval('t_id_seq'::regclass)"), ["t_id_seq"]);
        assert_eq!(nextval_arguments("(nextval('\"App\".\"It''s\"'::regclass) * 2)"), ["\"App\".\"It's\""]);
        assert_eq!(nextval_arguments("(nextval('a'::regclass) + nextval('b.c'::regclass))"), ["a", "b.c"]);
        assert!(nextval_arguments("'nextval(''x''::regclass)'::text").is_empty());
        assert!(nextval_arguments("42").is_empty());
        assert!(nextval_arguments("nextval('unterminated").is_empty());
    }
}
