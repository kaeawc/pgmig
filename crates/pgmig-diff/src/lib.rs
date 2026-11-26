//! `diff(a, b)`: the changes that turn model `a` into model `b`.

use pgmig_model::{Column, Model, QualName, Table};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    CreateSchema(String),
    DropSchema(String),
    CreateTable(Table),
    /// The table as it was, so the planner sees what its columns referenced.
    DropTable(Table),
    AddColumn {
        table: QualName,
        column: Column,
    },
    /// `type_ref` is the dropped column's.
    DropColumn {
        table: QualName,
        column: String,
        type_ref: Option<QualName>,
    },
    /// `type_ref` is the new type's, `old_type_ref` the replaced one's.
    AlterColumnType {
        table: QualName,
        column: String,
        type_name: String,
        type_ref: Option<QualName>,
        old_type_ref: Option<QualName>,
    },
    SetNotNull {
        table: QualName,
        column: String,
    },
    DropNotNull {
        table: QualName,
        column: String,
    },
    SetDefault {
        table: QualName,
        column: String,
        default: String,
    },
    DropDefault {
        table: QualName,
        column: String,
    },
}

pub fn diff(a: &Model, b: &Model) -> Vec<Change> {
    let mut changes = Vec::new();

    for name in b.schemas.keys().filter(|n| !a.schemas.contains_key(*n)) {
        changes.push(Change::CreateSchema(name.clone()));
    }
    for name in a.schemas.keys().filter(|n| !b.schemas.contains_key(*n)) {
        changes.push(Change::DropSchema(name.clone()));
    }

    for (name, table) in &b.tables {
        match a.tables.get(name) {
            None => changes.push(Change::CreateTable(table.clone())),
            Some(old) => diff_table(old, table, &mut changes),
        }
    }
    for table in a.tables.values().filter(|t| !b.tables.contains_key(&t.name)) {
        changes.push(Change::DropTable(table.clone()));
    }

    changes
}

fn diff_table(a: &Table, b: &Table, changes: &mut Vec<Change>) {
    let table = &b.name;
    for col in &b.columns {
        let Some(old) = a.column(&col.name) else {
            changes.push(Change::AddColumn { table: table.clone(), column: col.clone() });
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
            });
        }
        match (&old.default, &col.default) {
            (_, Some(new)) if old.default.as_ref() != Some(new) => {
                changes.push(Change::SetDefault { table: table.clone(), column: column(), default: new.clone() });
            }
            (Some(_), None) => changes.push(Change::DropDefault { table: table.clone(), column: column() }),
            _ => {}
        }
        match (old.not_null, col.not_null) {
            (false, true) => changes.push(Change::SetNotNull { table: table.clone(), column: column() }),
            (true, false) => changes.push(Change::DropNotNull { table: table.clone(), column: column() }),
            _ => {}
        }
    }
    for old in a.columns.iter().filter(|c| b.column(&c.name).is_none()) {
        changes.push(Change::DropColumn { table: table.clone(), column: old.name.clone(), type_ref: old.type_ref.clone() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, type_name: &str) -> Column {
        Column { name: name.into(), type_name: type_name.into(), type_ref: None, not_null: false, default: None }
    }

    fn model(columns: Vec<Column>) -> Model {
        let mut m = Model::empty_database();
        let name = QualName::new("public", "t");
        m.tables.insert(name.clone(), Table { name, columns });
        m
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
                    old_type_ref: None
                },
                Change::SetNotNull { table: t.clone(), column: "id".into() },
                Change::AddColumn { table: t.clone(), column: col("new", "text") },
                Change::DropColumn { table: t, column: "gone".into(), type_ref: None },
            ]
        );
    }
}
