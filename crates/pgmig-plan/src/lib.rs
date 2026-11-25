//! Turns [`Change`]s into ordered SQL.
//!
//! Order: create schemas, create tables, alter columns, then drop in reverse (columns, tables,
//! schemas), so every statement only references objects that exist when it runs.

use pgmig_diff::Change;
use pgmig_model::{Column, quote_ident};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub statements: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// The plan as a SQL script: one statement per entry, each ending in `;`.
    pub fn to_sql(&self) -> String {
        self.statements.iter().map(|s| format!("{s};\n")).collect()
    }
}

pub fn plan(changes: &[Change]) -> Plan {
    let mut ordered: Vec<&Change> = changes.iter().collect();
    ordered.sort_by_key(|c| rank(c));
    Plan { statements: ordered.into_iter().map(statement).collect() }
}

fn rank(change: &Change) -> u8 {
    match change {
        Change::CreateSchema(_) => 0,
        Change::CreateTable(_) => 1,
        Change::AddColumn { .. } => 2,
        Change::DropDefault { .. } => 3,
        Change::AlterColumnType { .. } => 4,
        Change::SetDefault { .. } => 5,
        Change::DropNotNull { .. } | Change::SetNotNull { .. } => 6,
        Change::DropColumn { .. } => 7,
        Change::DropTable(_) => 8,
        Change::DropSchema(_) => 9,
    }
}

fn statement(change: &Change) -> String {
    match change {
        Change::CreateSchema(name) => format!("CREATE SCHEMA {}", quote_ident(name)),
        Change::DropSchema(name) => format!("DROP SCHEMA {}", quote_ident(name)),
        Change::CreateTable(t) => {
            let cols: Vec<String> = t.columns.iter().map(|c| format!("    {}", column_def(c))).collect();
            if cols.is_empty() {
                format!("CREATE TABLE {} ()", t.name)
            } else {
                format!("CREATE TABLE {} (\n{}\n)", t.name, cols.join(",\n"))
            }
        }
        Change::DropTable(name) => format!("DROP TABLE {name}"),
        Change::AddColumn { table, column } => format!("ALTER TABLE {table} ADD COLUMN {}", column_def(column)),
        Change::DropColumn { table, column } => format!("ALTER TABLE {table} DROP COLUMN {}", quote_ident(column)),
        Change::AlterColumnType { table, column, type_name } => {
            format!("ALTER TABLE {table} ALTER COLUMN {} TYPE {type_name}", quote_ident(column))
        }
        Change::SetNotNull { table, column } => format!("ALTER TABLE {table} ALTER COLUMN {} SET NOT NULL", quote_ident(column)),
        Change::DropNotNull { table, column } => format!("ALTER TABLE {table} ALTER COLUMN {} DROP NOT NULL", quote_ident(column)),
        Change::SetDefault { table, column, default } => {
            format!("ALTER TABLE {table} ALTER COLUMN {} SET DEFAULT {default}", quote_ident(column))
        }
        Change::DropDefault { table, column } => format!("ALTER TABLE {table} ALTER COLUMN {} DROP DEFAULT", quote_ident(column)),
    }
}

fn column_def(c: &Column) -> String {
    let mut s = format!("{} {}", quote_ident(&c.name), c.type_name);
    if let Some(d) = &c.default {
        s.push_str(" DEFAULT ");
        s.push_str(d);
    }
    if c.not_null {
        s.push_str(" NOT NULL");
    }
    s
}
