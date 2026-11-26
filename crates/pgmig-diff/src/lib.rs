//! `diff(a, b)`: the changes that turn model `a` into model `b`.

use pgmig_model::{Column, Model, QualName, Table};

/// An object that can carry a comment. New kinds (constraints, indexes) add a variant here, a
/// matching `Object` in the planner, and a comparison in [`diff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentTarget {
    Schema(String),
    Table(QualName),
    Column(QualName, String),
}

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
    /// `COMMENT ON ... IS <comment>`; `None` removes the comment. Never emitted for an object
    /// that is being dropped.
    SetComment {
        target: CommentTarget,
        comment: Option<String>,
    },
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

    for (name, table) in &b.tables {
        match a.tables.get(name) {
            None => {
                changes.push(Change::CreateTable(table.clone()));
                set_comment(CommentTarget::Table(name.clone()), None, table.comment.as_ref(), &mut changes);
                for col in &table.columns {
                    set_comment(CommentTarget::Column(name.clone(), col.name.clone()), None, col.comment.as_ref(), &mut changes);
                }
            }
            Some(old) => diff_table(old, table, &mut changes),
        }
    }
    for table in a.tables.values().filter(|t| !b.tables.contains_key(&t.name)) {
        changes.push(Change::DropTable(table.clone()));
    }

    changes
}

/// Emit a `SetComment` when the comment differs (`old` is `None` for a new object).
fn set_comment(target: CommentTarget, old: Option<&String>, new: Option<&String>, changes: &mut Vec<Change>) {
    if old != new {
        changes.push(Change::SetComment { target, comment: new.cloned() });
    }
}

fn diff_table(a: &Table, b: &Table, changes: &mut Vec<Change>) {
    let table = &b.name;
    set_comment(CommentTarget::Table(table.clone()), a.comment.as_ref(), b.comment.as_ref(), changes);
    for col in &b.columns {
        let Some(old) = a.column(&col.name) else {
            changes.push(Change::AddColumn { table: table.clone(), column: col.clone() });
            set_comment(CommentTarget::Column(table.clone(), col.name.clone()), None, col.comment.as_ref(), changes);
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
        set_comment(CommentTarget::Column(table.clone(), column()), old.comment.as_ref(), col.comment.as_ref(), changes);
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
        Column { name: name.into(), type_name: type_name.into(), type_ref: None, not_null: false, default: None, comment: None }
    }

    fn model(columns: Vec<Column>) -> Model {
        let mut m = Model::empty_database();
        let name = QualName::new("public", "t");
        m.tables.insert(name.clone(), Table { name, columns, comment: None });
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

    fn commented(comment: Option<&str>) -> Model {
        let mut m = Model::empty_database();
        m.schemas.insert("app".into(), pgmig_model::Schema { name: "app".into(), comment: comment.map(Into::into) });
        let mut c = col("id", "integer");
        c.comment = comment.map(Into::into);
        let name = QualName::new("public", "t");
        m.tables.insert(name.clone(), Table { name, columns: vec![c], comment: comment.map(Into::into) });
        m
    }

    fn set(target: CommentTarget, comment: Option<&str>) -> Change {
        Change::SetComment { target, comment: comment.map(Into::into) }
    }

    fn targets() -> [CommentTarget; 3] {
        let t = QualName::new("public", "t");
        [CommentTarget::Schema("app".into()), CommentTarget::Table(t.clone()), CommentTarget::Column(t, "id".into())]
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
            vec![Change::AddColumn { table: t.clone(), column: n }, set(CommentTarget::Column(t, "note".into()), Some("hi"))]
        );
    }

    #[test]
    fn the_public_schemas_default_comment_is_not_a_difference() {
        assert!(diff(&Model::empty_database(), &Model::empty_database()).is_empty());
    }
}
