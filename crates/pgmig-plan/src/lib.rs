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
//! [`Plan::batches`] are the scripts to run in separate transactions, and [`Plan::to_sql`] shows
//! the point as a comment. pgmig does not check which later statements use a new value; any
//! statement after an `ADD VALUE` waits for the commit.
//!
//! Changes Postgres cannot make in place (removing or reordering enum values) fail the plan with
//! [`Error::Unsupported`].

pub mod graph;

use graph::{Deps, Node, Object};
use pgmig_diff::{Change, CommentTarget, EnumPosition};
use pgmig_model::{Column, QualName, quote_ident, quote_literal};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

    /// The plan as a SQL script: one statement per entry, each ending in `;`, with a comment line
    /// at the commit point.
    pub fn to_sql(&self) -> String {
        self.batches().join(COMMIT_COMMENT)
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

/// Marks [`Plan::commit_after`] in [`Plan::to_sql`].
pub const COMMIT_COMMENT: &str = "-- commit: new enum values cannot be used in the transaction that adds them\n";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Statements that each need the next one to run first (and the last needs the first).
    #[error("the changes depend on each other in a cycle: {}", .0.join("; "))]
    Cycle(Vec<String>),
    /// Changes that cannot be planned, one message each.
    #[error("unsupported changes: {}", .0.join("; "))]
    Unsupported(Vec<String>),
}

pub fn plan(changes: &[Change]) -> Result<Plan, Error> {
    let unsupported: Vec<String> = changes.iter().filter_map(unsupported).collect();
    if !unsupported.is_empty() {
        return Err(Error::Unsupported(unsupported));
    }
    let (adds, rest): (Vec<&Change>, Vec<&Change>) = changes.iter().partition(|c| matches!(c, Change::AddEnumValue { .. }));
    let mut statements = sorted(&adds)?;
    let commit_after = (!statements.is_empty() && !rest.is_empty()).then(|| statements.len() - 1);
    statements.extend(sorted(&rest)?);
    Ok(Plan { statements, commit_after })
}

fn sorted(changes: &[&Change]) -> Result<Vec<String>, Error> {
    let nodes: Vec<Node> = changes.iter().map(|c| Node { phase: phase(c), deps: deps(c) }).collect();
    match graph::sort(&nodes) {
        Ok(order) => Ok(order.into_iter().map(|i| statement(changes[i])).collect()),
        Err(graph::Cycle(steps)) => Err(Error::Cycle(steps.into_iter().map(|i| statement(changes[i])).collect())),
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
        Change::CreateEnum(_) => 1,
        // Before defaults that print the new name are set.
        Change::RenameEnumValue { .. } => 2,
        Change::CreateTable(_) => 3,
        Change::AddColumn { .. } => 4,
        Change::DropDefault { .. } => 5,
        Change::AlterColumnType { .. } => 6,
        Change::SetDefault { .. } => 7,
        Change::DropNotNull { .. } | Change::SetNotNull { .. } => 8,
        Change::DropColumn { .. } => 9,
        Change::DropTable(_) => 10,
        Change::DropEnum(_) => 11,
        Change::DropSchema(_) => 12,
        // After the object it describes exists and has settled.
        Change::SetComment { .. } => 13,
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
        Change::SetComment { target, .. } => {
            // A comment only waits for the object (and a column's table) to exist.
            let requires = match target {
                CommentTarget::Schema(s) => vec![Object::Schema(s.clone())],
                CommentTarget::Table(t) => vec![in_table(t)],
                CommentTarget::Column(t, c) => vec![in_table(t), column(t, c)],
                CommentTarget::Type(t) => vec![Object::Type(t.clone())],
            };
            Deps { requires, ..Deps::default() }
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
        Change::CreateEnum(e) => {
            let labels: Vec<String> = e.labels.iter().map(|l| quote_literal(l)).collect();
            format!("CREATE TYPE {} AS ENUM ({})", e.name, labels.join(", "))
        }
        Change::DropEnum(e) => format!("DROP TYPE {}", e.name),
        Change::AddEnumValue { name, value, position } => {
            let at = match position {
                EnumPosition::Before(l) => format!(" BEFORE {}", quote_literal(l)),
                EnumPosition::After(l) => format!(" AFTER {}", quote_literal(l)),
                EnumPosition::End => String::new(),
            };
            format!("ALTER TYPE {name} ADD VALUE {}{at}", quote_literal(value))
        }
        Change::RenameEnumValue { name, from, to } => {
            // The catalog cannot tell a rename from removing one value and adding another, so say
            // what this does to stored data.
            format!(
                "-- renames a value: rows holding {} will hold {}\nALTER TYPE {name} RENAME VALUE {} TO {}",
                quote_literal(from),
                quote_literal(to),
                quote_literal(from),
                quote_literal(to)
            )
        }
        Change::UnsupportedEnumChange { name, .. } => format!("-- unsupported change to enum {name}"),
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
        Change::SetComment { target, comment } => {
            let object = match target {
                CommentTarget::Schema(s) => format!("SCHEMA {}", quote_ident(s)),
                CommentTarget::Table(t) => format!("TABLE {t}"),
                CommentTarget::Column(t, c) => format!("COLUMN {t}.{}", quote_ident(c)),
                CommentTarget::Type(t) => format!("TYPE {t}"),
            };
            format!("COMMENT ON {object} IS {}", comment.as_deref().map_or("NULL".to_owned(), quote_literal))
        }
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
    use pgmig_model::{Enum, Table};

    fn col(name: &str, type_ref: Option<QualName>) -> Column {
        let type_name = type_ref.as_ref().map_or("integer".to_owned(), |t| t.name.clone());
        Column { name: name.into(), type_name, type_ref, not_null: false, default: None, comment: None }
    }

    fn table(schema: &str, name: &str, columns: Vec<Column>) -> Table {
        Table { name: QualName::new(schema, name), columns, comment: None }
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
            Change::SetComment { target: CommentTarget::Column(t.clone(), "id".into()), comment: Some("a\\b\nc".into()) },
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
                "COMMENT ON COLUMN app.t.id IS 'a\\b\nc'",
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
            Change::DropTable(users),
            Change::DropColumn { table: t, column: "m".into(), type_ref: m },
        ]);
        assert_eq!(dropped, ["ALTER TABLE public.t DROP COLUMN m", "DROP TABLE public.users", "DROP TYPE app.mood", "DROP SCHEMA app"]);
    }

    #[test]
    fn added_values_come_first_in_diff_order_and_are_committed_before_the_rest() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault { table: t.clone(), column: "m".into(), default: "'b'::app.mood".into() },
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
        assert_eq!(p.to_sql(), p.batches().join(COMMIT_COMMENT));

        let only_adds = plan(&changes[1..2]).expect("plan");
        assert_eq!(only_adds.commit_after, None);
        assert_eq!(only_adds.to_sql(), "ALTER TYPE app.mood ADD VALUE 'a' BEFORE 'c';\n");
    }

    #[test]
    fn a_renamed_value_runs_before_defaults_that_use_it() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault { table: t, column: "m".into(), default: "'new'::app.mood".into() },
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
}
