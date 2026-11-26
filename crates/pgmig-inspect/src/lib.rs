//! Reads a database's catalog into a [`Model`].
//!
//! The same queries run against pgmem and real Postgres, and only `pg_catalog` is used:
//! `information_schema` hides objects the current user doesn't own.

use pgmig_conn::{Conn, Row};
use pgmig_model::{Column, Enum, Model, QualName, Schema, Table};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("inspecting {what}: {source}")]
    Query { what: &'static str, source: pgmig_conn::Error },
    #[error("inspecting {what}: unexpected NULL in column {column}")]
    Null { what: &'static str, column: usize },
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

    let columns = format!(
        "SELECT n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                pg_get_expr(d.adbin, d.adrelid), tn.nspname, COALESCE(e.typname, t.typname), \
                col_description(a.attrelid, a.attnum) \
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
    for row in run(conn, "columns", &columns)? {
        let table = QualName::new(text(&row, 0, "columns")?, text(&row, 1, "columns")?);
        let column = Column {
            name: text(&row, 2, "columns")?,
            type_name: text(&row, 3, "columns")?,
            not_null: text(&row, 4, "columns")? == "t",
            default: row.get(5).cloned().flatten(),
            type_ref: type_ref(&row),
            comment: row.get(8).cloned().flatten(),
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

fn run(conn: &mut Conn, what: &'static str, sql: &str) -> Result<Vec<Row>> {
    conn.query(sql).map_err(|source| Error::Query { what, source })
}

fn text(row: &Row, column: usize, what: &'static str) -> Result<String> {
    row.get(column).cloned().flatten().ok_or(Error::Null { what, column })
}
