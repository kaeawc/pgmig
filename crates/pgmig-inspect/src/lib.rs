//! Reads a database's catalog into a [`Model`].
//!
//! The same queries run against pgmem and real Postgres, and only `pg_catalog` is used:
//! `information_schema` hides objects the current user doesn't own.

use pgmig_conn::{Conn, Row};
use std::collections::BTreeMap;

use pgmig_model::{Column, Enum, Generated, Identity, Model, OwnedBy, QualName, Schema, Sequence, SequenceOptions, Table, quote_literal};

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

pub fn inspect(conn: &mut Conn) -> Result<Model> {
    let mut model = Model::default();

    let schemas = format!("SELECT n.nspname, obj_description(n.oid, 'pg_namespace') FROM pg_namespace n WHERE {USER_SCHEMA} ORDER BY 1");
    for row in run(conn, "schemas", &schemas)? {
        let name = text(&row, 0, "schemas")?;
        model.schemas.insert(name.clone(), Schema { name, comment: row.get(1).cloned().flatten() });
    }

    let tables = format!(
        "SELECT n.nspname, c.relname, obj_description(c.oid, 'pg_class') FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND {USER_SCHEMA} ORDER BY 1, 2"
    );
    for row in run(conn, "tables", &tables)? {
        let name = QualName::new(text(&row, 0, "tables")?, text(&row, 1, "tables")?);
        model.tables.insert(name.clone(), Table { name, columns: Vec::new(), comment: row.get(2).cloned().flatten() });
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
                col_description(a.attrelid, a.attnum), a.attidentity \
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
    for row in run(conn, "columns", &columns)? {
        let table = QualName::new(text(&row, 0, "columns")?, text(&row, 1, "columns")?);
        let name = text(&row, 2, "columns")?;
        let default = row.get(5).cloned().flatten();
        let mut default_sequences = Vec::new();
        for literal in default.as_deref().map(nextval_arguments).unwrap_or_default() {
            if !resolved.contains_key(&literal) {
                let found = resolve_relation(conn, &literal)?;
                resolved.insert(literal.clone(), found);
            }
            if let Some(Some(seq)) = resolved.get(&literal)
                && !default_sequences.contains(seq)
            {
                default_sequences.push(seq.clone());
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

    Ok(model)
}

/// The column's type when it lives in a user schema: columns 6 (schema) and 7 (name).
fn type_ref(row: &Row) -> Option<QualName> {
    let schema = row.get(6).cloned().flatten()?;
    let name = row.get(7).cloned().flatten()?;
    let system = schema == "pg_catalog" || schema == "information_schema" || schema.starts_with("pg_");
    (!system).then(|| QualName::new(schema, name))
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

/// The relation a `regclass` literal names, resolved by the server with its search path.
fn resolve_relation(conn: &mut Conn, literal: &str) -> Result<Option<QualName>> {
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
    use super::nextval_arguments;

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
