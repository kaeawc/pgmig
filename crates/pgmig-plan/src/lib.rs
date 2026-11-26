//! Turns [`Change`]s into ordered SQL.
//!
//! Each change declares the objects it creates, drops and references ([`graph::Deps`]); the
//! statements are ordered by that dependency graph, so creates run after what they reference,
//! drops run before what they reference, and an alter runs after the creates it needs and before
//! the drops it unblocks. Unrelated statements keep a fixed phase order (create schemas, create
//! tables, alter columns, drop columns, tables, schemas) and then the diff's order. See
//! [`graph`] for the rules and for how cycles are reported.

pub mod graph;

use graph::{Deps, Node, Object};
use pgmig_diff::Change;
use pgmig_model::{Column, QualName, quote_ident};

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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Statements that each need the next one to run first (and the last needs the first).
    #[error("the changes depend on each other in a cycle: {}", .0.join("; "))]
    Cycle(Vec<String>),
}

pub fn plan(changes: &[Change]) -> Result<Plan, Error> {
    let nodes: Vec<Node> = changes.iter().map(|c| Node { phase: phase(c), deps: deps(c) }).collect();
    match graph::sort(&nodes) {
        Ok(order) => Ok(Plan { statements: order.into_iter().map(|i| statement(&changes[i])).collect() }),
        Err(graph::Cycle(steps)) => Err(Error::Cycle(steps.into_iter().map(|i| statement(&changes[i])).collect())),
    }
}

/// The tie-break between statements the graph does not order.
fn phase(change: &Change) -> u8 {
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

fn deps(change: &Change) -> Deps {
    let column = |table: &QualName, column: &str| Object::Column(table.clone(), column.to_owned());
    let in_table = |table: &QualName| Object::Table(table.clone());
    match change {
        Change::CreateSchema(name) => Deps { creates: vec![Object::Schema(name.clone())], ..Deps::default() },
        Change::DropSchema(name) => Deps { drops: vec![Object::Schema(name.clone())], ..Deps::default() },
        Change::CreateTable(t) => {
            let mut requires = vec![Object::Schema(t.name.schema.clone())];
            requires.extend(types(t.columns.iter().map(|c| &c.type_ref)));
            Deps { creates: vec![Object::Table(t.name.clone()), Object::Type(t.name.clone())], requires, ..Deps::default() }
        }
        Change::DropTable(t) => {
            let mut releases = vec![Object::Schema(t.name.schema.clone())];
            releases.extend(types(t.columns.iter().map(|c| &c.type_ref)));
            Deps { drops: vec![Object::Table(t.name.clone()), Object::Type(t.name.clone())], releases, ..Deps::default() }
        }
        Change::AddColumn { table, column: c } => {
            let mut requires = vec![in_table(table)];
            requires.extend(types([&c.type_ref]));
            Deps { creates: vec![column(table, &c.name)], requires, ..Deps::default() }
        }
        Change::DropColumn { table, column: c, type_ref } => {
            Deps { drops: vec![column(table, c)], requires: vec![in_table(table)], releases: types([type_ref]), ..Deps::default() }
        }
        Change::AlterColumnType { table, column: c, type_ref, old_type_ref, .. } => {
            let mut requires = vec![in_table(table)];
            requires.extend(types([type_ref]));
            let releases = if type_ref == old_type_ref { Vec::new() } else { types([old_type_ref]) };
            Deps { alters: vec![column(table, c)], requires, releases, ..Deps::default() }
        }
        Change::SetNotNull { table, column: c }
        | Change::DropNotNull { table, column: c }
        | Change::SetDefault { table, column: c, .. }
        | Change::DropDefault { table, column: c } => {
            Deps { alters: vec![column(table, c)], requires: vec![in_table(table)], ..Deps::default() }
        }
    }
}

/// The user-defined types among column type references.
fn types<'a>(refs: impl IntoIterator<Item = &'a Option<QualName>>) -> Vec<Object> {
    refs.into_iter().flatten().map(|t| Object::Type(t.clone())).collect()
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
        Change::DropTable(t) => format!("DROP TABLE {}", t.name),
        Change::AddColumn { table, column } => format!("ALTER TABLE {table} ADD COLUMN {}", column_def(column)),
        Change::DropColumn { table, column, .. } => format!("ALTER TABLE {table} DROP COLUMN {}", quote_ident(column)),
        Change::AlterColumnType { table, column, type_name, .. } => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pgmig_model::Table;

    fn col(name: &str, type_ref: Option<QualName>) -> Column {
        let type_name = type_ref.as_ref().map_or("integer".to_owned(), |t| t.name.clone());
        Column { name: name.into(), type_name, type_ref, not_null: false, default: None }
    }

    fn table(schema: &str, name: &str, columns: Vec<Column>) -> Table {
        Table { name: QualName::new(schema, name), columns }
    }

    fn row_type(schema: &str, name: &str) -> Option<QualName> {
        Some(QualName::new(schema, name))
    }

    fn sql(changes: &[Change]) -> Vec<String> {
        plan(changes).expect("plan").statements
    }

    #[test]
    fn schema_is_created_before_its_table_and_dropped_after() {
        let t = table("app", "t", vec![col("id", None)]);
        let created = sql(&[Change::CreateTable(t.clone()), Change::CreateSchema("app".into())]);
        assert_eq!(created, ["CREATE SCHEMA app", "CREATE TABLE app.t (\n    id integer\n)"]);
        let dropped = sql(&[Change::DropSchema("app".into()), Change::DropTable(t)]);
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
        let dropped = sql(&[Change::DropTable(point), Change::DropTable(path)]);
        assert_eq!(dropped, ["DROP TABLE public.zpath", "DROP TABLE public.apoint"]);
    }

    #[test]
    fn column_changes_wait_for_the_types_they_use() {
        let t = QualName::new("public", "t");
        let new = table("public", "znew", vec![col("x", None)]);
        let old = table("public", "aold", vec![col("x", None)]);
        let changes = [
            Change::DropTable(old),
            Change::DropColumn { table: t.clone(), column: "gone".into(), type_ref: row_type("public", "aold") },
            Change::AlterColumnType {
                table: t.clone(),
                column: "c".into(),
                type_name: "znew".into(),
                type_ref: row_type("public", "znew"),
                old_type_ref: row_type("public", "aold"),
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
    fn unrelated_statements_keep_phase_then_diff_order() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::DropSchema("old".into()),
            Change::SetNotNull { table: t.clone(), column: "a".into() },
            Change::CreateTable(table("public", "b", vec![])),
            Change::DropDefault { table: t.clone(), column: "a".into() },
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
        let Error::Cycle(statements) = &err;
        assert_eq!(statements.len(), 2);
        assert!(err.to_string().starts_with("the changes depend on each other in a cycle: CREATE TABLE"), "{err}");
    }
}
