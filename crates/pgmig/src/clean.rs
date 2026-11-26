//! `schema clean`: drop every object pgmig manages in a database, leaving it as a new database
//! holds it. For local development.
//!
//! The plan is the diff from the database to what it would hold without its managed objects: a
//! new database's `public` schema (kept, with its standard comment, when it is managed) and the
//! objects the options do not manage. Those stay, and so does every managed object one of them
//! needs, or dropping it would fail (`2BP01`) or take the unmanaged object with it: the schema
//! holding it, the enum or table row type a column uses, the sequence a default takes values from
//! or that owns a sequence, the table, columns and key a foreign key references, the columns a
//! constraint uses. Each is reported as [`Kept`].
//!
//! The revisions schema is never managed, so it stays with the migration history it records.
//! Objects pgmig does not model (extensions, views, functions) are not dropped; one that depends on
//! a dropped object makes the plan fail, and its transaction is rolled back.

use serde::{Deserialize, Serialize};

use pgmig_model::{Column, QualName, Table};

use crate::{Change, Conn, Model, Options, Plan, PlanOptions, Result, diff, inspect, plan_with};

/// A managed object `schema clean` does not drop, because an object that stays needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    /// `table public.users`, `column public.users.email`, `enum public.mood`, ...
    pub object: String,
    /// What needs it: `column public.keep.m uses it`.
    pub because: String,
}

/// What [`schema_plan_clean`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaClean {
    /// `current_database()` of the target: what the confirmation prompt asks for.
    pub database: String,
    /// The drops, schema-qualified.
    pub plan: Plan,
    pub changes: Vec<Change>,
    pub kept: Vec<Kept>,
}

/// `schema clean`, first half: the plan that drops every object `opts` manages in the database at
/// `target` (see the module doc). Apply it with [`crate::apply`]. The diff policy
/// ([`Options::skip`]) does not apply.
pub fn schema_plan_clean(target: &str, opts: &Options) -> Result<SchemaClean> {
    opts.validate()?;
    let mut conn = Conn::connect(target)?;
    let full = inspect(&mut conn)?;
    let database =
        conn.query("SELECT current_database()")?.into_iter().next().and_then(|r| r.into_iter().next().flatten()).unwrap_or_default();
    let (desired, kept) = clean_desired(&full, opts);
    let changes = diff(&opts.managed(&full), &opts.managed(&desired));
    let plan = plan_with(&changes, &PlanOptions::default())?;
    Ok(SchemaClean { database, plan, changes, kept })
}

/// Whether `answer` (a line typed at the prompt) confirms cleaning `database`: the database name
/// itself, surrounding whitespace aside. Never confirmed for an empty name.
pub fn clean_confirmed(answer: &str, database: &str) -> bool {
    !database.is_empty() && answer.trim() == database
}

/// What `full` holds after a clean under `opts`: its unmanaged objects, what they need, and a new
/// database's managed objects (`public`). The managed objects kept are reported.
pub fn clean_desired(full: &Model, opts: &Options) -> (Model, Vec<Kept>) {
    let managed = opts.managed(full);
    let mut d = full.clone();
    d.schemas.retain(|n, _| !managed.schemas.contains_key(n));
    for (name, s) in opts.empty_database().schemas {
        d.schemas.insert(name, s);
    }
    d.enums.retain(|n, _| !managed.enums.contains_key(n));
    d.sequences.retain(|n, _| !managed.sequences.contains_key(n));
    d.tables = full
        .tables
        .iter()
        .filter_map(|(name, t)| {
            let Some(mt) = managed.tables.get(name) else { return Some(t.clone()) };
            // A managed table with unmanaged columns stays with only those.
            let columns: Vec<Column> = t.columns.iter().filter(|c| mt.column(&c.name).is_none()).cloned().collect();
            if columns.is_empty() {
                return None;
            }
            let constraints = t.constraints.iter().filter(|c| mt.constraint(&c.name).is_none()).cloned().collect();
            Some(Table { columns, constraints, ..t.clone() })
        })
        .map(|t| (t.name.clone(), t))
        .collect();

    let mut kept = Vec::new();
    loop {
        let mut changed = false;
        for (need, because) in needs(full, &d) {
            if let Some(object) = restore(full, &mut d, &need) {
                kept.push(Kept { object, because });
                changed = true;
            }
        }
        if !changed {
            return (d, kept);
        }
    }
}

/// An object an object of the cleaned model needs.
enum Need {
    Schema(String),
    Table(QualName),
    Enum(QualName),
    Sequence(QualName),
    Column(QualName, String),
    Constraint(QualName, String),
}

/// Everything the objects of `d` need, with what needs it. `full` resolves type references.
fn needs(full: &Model, d: &Model) -> Vec<(Need, String)> {
    let mut out = Vec::new();
    for t in d.tables.values() {
        let table = format!("table {}", t.name);
        out.push((Need::Schema(t.name.schema.clone()), format!("{table} is in it")));
        for c in &t.columns {
            let col = format!("column {}.{}", t.name, pgmig_model::quote_ident(&c.name));
            if let Some(r) = &c.type_ref {
                if full.enums.contains_key(r) {
                    out.push((Need::Enum(r.clone()), format!("{col} uses it")));
                } else if full.tables.contains_key(r) && r != &t.name {
                    out.push((Need::Table(r.clone()), format!("{col} uses its row type")));
                }
            }
            for s in &c.default_sequences {
                out.push((Need::Sequence(s.clone()), format!("the default of {col} uses it")));
            }
        }
        for k in &t.constraints {
            let con = format!("constraint {} on {}", pgmig_model::quote_ident(&k.name), t.name);
            for c in &k.columns {
                out.push((Need::Column(t.name.clone(), c.clone()), format!("{con} uses it")));
            }
            if let Some(r) = &k.references {
                out.push((Need::Table(r.table.clone()), format!("{con} references it")));
                for c in &r.columns {
                    out.push((Need::Column(r.table.clone(), c.clone()), format!("{con} references it")));
                }
                if let Some(idx) = &r.constraint {
                    out.push((Need::Constraint(r.table.clone(), idx.clone()), format!("{con} uses its index")));
                }
            }
        }
    }
    for e in d.enums.values() {
        out.push((Need::Schema(e.name.schema.clone()), format!("enum {} is in it", e.name)));
    }
    for s in d.sequences.values() {
        out.push((Need::Schema(s.name.schema.clone()), format!("sequence {} is in it", s.name)));
        if let Some(o) = &s.owned_by {
            // Dropping the owning column would drop the sequence with it.
            out.push((Need::Column(o.table.clone(), o.column.clone()), format!("sequence {} is owned by it", s.name)));
        }
    }
    out
}

/// Put `need` back into `d` from `full`; the object restored, or `None` when `d` already has it
/// (or `full` does not, which is not pgmig's to keep). A column or constraint of a table `d` has
/// dropped brings back the whole table.
fn restore(full: &Model, d: &mut Model, need: &Need) -> Option<String> {
    match need {
        Need::Schema(s) => {
            if d.schemas.contains_key(s) {
                return None;
            }
            d.schemas.insert(s.clone(), full.schemas.get(s)?.clone());
            Some(format!("schema {}", pgmig_model::quote_ident(s)))
        }
        Need::Enum(n) => {
            if d.enums.contains_key(n) {
                return None;
            }
            d.enums.insert(n.clone(), full.enums.get(n)?.clone());
            Some(format!("enum {n}"))
        }
        Need::Sequence(n) => {
            if d.sequences.contains_key(n) {
                return None;
            }
            d.sequences.insert(n.clone(), full.sequences.get(n)?.clone());
            Some(format!("sequence {n}"))
        }
        Need::Table(n) => restore_table(full, d, n),
        Need::Column(n, c) => {
            let Some(t) = d.tables.get_mut(n) else { return restore_table(full, d, n) };
            if t.column(c).is_some() {
                return None;
            }
            let ft = full.tables.get(n)?;
            ft.column(c)?;
            // In the table's column order.
            t.columns = ft.columns.iter().filter(|fc| &fc.name == c || t.column(&fc.name).is_some()).cloned().collect();
            Some(format!("column {n}.{}", pgmig_model::quote_ident(c)))
        }
        Need::Constraint(n, k) => {
            let Some(t) = d.tables.get_mut(n) else { return restore_table(full, d, n) };
            if t.constraint(k).is_some() {
                return None;
            }
            t.constraints.push(full.tables.get(n)?.constraint(k)?.clone());
            Some(format!("constraint {} on {n}", pgmig_model::quote_ident(k)))
        }
    }
}

fn restore_table(full: &Model, d: &mut Model, n: &QualName) -> Option<String> {
    if d.tables.contains_key(n) {
        return None;
    }
    d.tables.insert(n.clone(), full.tables.get(n)?.clone());
    Some(format!("table {n}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Filter;
    use pgmig_model::{Enum, Schema};

    fn col(name: &str, type_ref: Option<QualName>) -> Column {
        Column {
            name: name.into(),
            type_name: "integer".into(),
            type_ref,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: vec![],
            identity: None,
        }
    }

    fn model() -> Model {
        let mut m = Model::empty_database();
        m.schemas.insert("app".into(), Schema { name: "app".into(), comment: None });
        let mood = QualName::new("app", "mood");
        m.enums.insert(mood.clone(), Enum { name: mood.clone(), labels: vec!["a".into()], comment: None });
        for (name, cols) in [("keep", vec![col("m", Some(mood.clone()))]), ("gone", vec![col("id", None)])] {
            let n = QualName::new("public", name);
            m.tables.insert(n.clone(), Table { name: n, columns: cols, comment: None, constraints: vec![] });
        }
        m
    }

    #[test]
    fn confirmation_needs_the_database_name() {
        assert!(clean_confirmed("app_dev\n", "app_dev"));
        assert!(clean_confirmed("  app_dev ", "app_dev"));
        for wrong in ["", "y", "yes", "app", "APP_DEV", "app_dev2"] {
            assert!(!clean_confirmed(wrong, "app_dev"), "{wrong:?}");
        }
        assert!(!clean_confirmed("", ""));
    }

    #[test]
    fn everything_managed_goes_and_public_stays_as_new() {
        let mut full = model();
        full.schemas.get_mut("public").expect("public").comment = Some("changed".into());
        let (d, kept) = clean_desired(&full, &Options::default());
        assert_eq!(d, Model::empty_database());
        assert!(kept.is_empty());
    }

    #[test]
    fn what_an_excluded_table_needs_is_kept() {
        let opts = Options { filter: Filter::parse::<&str, _>(&[], &["table:public.keep"]).expect("filter"), ..Options::default() };
        let (d, kept) = clean_desired(&model(), &opts);
        assert!(d.tables.contains_key(&QualName::new("public", "keep")));
        assert!(!d.tables.contains_key(&QualName::new("public", "gone")));
        assert!(d.enums.contains_key(&QualName::new("app", "mood")) && d.schemas.contains_key("app"));
        let objects: Vec<&str> = kept.iter().map(|k| k.object.as_str()).collect();
        assert_eq!(objects, ["enum app.mood", "schema app"]);
        assert_eq!(kept[0].because, "column public.keep.m uses it");
    }
}
