//! Reads a database's catalog into a [`Model`].
//!
//! The same queries run against pgmem and real Postgres, and only `pg_catalog` is used:
//! `information_schema` hides objects the current user doesn't own.

use pgmig_conn::{Conn, Row};
use pgmig_model::{Column, Model, QualName, Schema, Table};

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

    for row in run(conn, "schemas", &format!("SELECT n.nspname FROM pg_namespace n WHERE {USER_SCHEMA} ORDER BY 1"))? {
        let name = text(&row, 0, "schemas")?;
        model.schemas.insert(name.clone(), Schema { name });
    }

    let tables = format!(
        "SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND {USER_SCHEMA} ORDER BY 1, 2"
    );
    for row in run(conn, "tables", &tables)? {
        let name = QualName::new(text(&row, 0, "tables")?, text(&row, 1, "tables")?);
        model.tables.insert(name.clone(), Table { name, columns: Vec::new() });
    }

    let columns = format!(
        "SELECT n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                pg_get_expr(d.adbin, d.adrelid) \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
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
        };
        if let Some(t) = model.tables.get_mut(&table) {
            t.columns.push(column);
        }
    }

    Ok(model)
}

fn run(conn: &mut Conn, what: &'static str, sql: &str) -> Result<Vec<Row>> {
    conn.query(sql).map_err(|source| Error::Query { what, source })
}

fn text(row: &Row, column: usize, what: &'static str) -> Result<String> {
    row.get(column).cloned().flatten().ok_or(Error::Null { what, column })
}
