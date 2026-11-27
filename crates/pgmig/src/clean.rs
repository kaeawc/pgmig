//! `schema clean`: drop every object pgmig manages in a database, leaving it as a new database
//! holds it. For local development.
//!
//! The plan is the diff from the database to what it would hold without its managed objects: a
//! new database's `public` schema (kept, with its standard comment, when it is managed) and the
//! objects the options do not manage. Those stay, and so does every managed object one of them
//! needs, or dropping it would fail (`2BP01`) or take the unmanaged object with it: the schema
//! holding it, the enum, domain, composite type or table row type a column uses (and those a kept
//! domain is over or a kept composite type's attributes use), the sequence a default takes values from
//! or that owns a sequence, the table, columns and key a foreign key references, the columns a
//! constraint or index uses, the partitioned table a partition is of. Each is reported as [`Kept`]. A table that stays keeps its unmanaged
//! indexes; managed indexes go (with their table, or alone when the table stays).
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
    /// The address the server accepted the connection on (`inet_server_addr()`); `None` over a
    /// Unix socket.
    pub server_address: Option<String>,
    /// Whether the server is on this machine: a Unix socket or a loopback address. A cleaning
    /// prompt warns when it is not, so a copied production URL stands out.
    pub local: bool,
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
    let first = |rows: Vec<pgmig_conn::Row>| rows.into_iter().next().and_then(|r| r.into_iter().next().flatten());
    let database = first(conn.query("SELECT current_database()")?).unwrap_or_default();
    // pgmem has no inet_server_addr() (kaeawc/pgmem#255): its address is then unknown, and the
    // target's host alone decides.
    let (server_address, server_local) = match conn.query("SELECT host(inet_server_addr())") {
        // NULL: a Unix socket.
        Ok(rows) => {
            let addr = first(rows);
            let local = addr.as_deref().is_none_or(is_loopback);
            (addr, local)
        }
        Err(e) if e.code.as_deref() == Some("42883") => (None, false),
        Err(e) => return Err(e.into()),
    };
    // Local when the target names only this machine (a published container port reports the
    // container's address) or the server says so (a name that resolves to loopback).
    let local = Conn::is_local_target(target)? || server_local;
    let (desired, kept) = clean_desired(&full, opts);
    let changes = diff(&opts.managed(&full), &opts.managed(&desired));
    let plan = plan_with(&changes, &PlanOptions::default())?;
    Ok(SchemaClean { database, server_address, local, plan, changes, kept })
}

/// Whether `addr` (as `host(inet)` prints it) is a loopback address: `127.0.0.0/8`, `::1` or an
/// IPv4-mapped `::ffff:127.x.y.z`.
fn is_loopback(addr: &str) -> bool {
    addr.parse::<std::net::IpAddr>().is_ok_and(|ip| pgmig_conn::is_loopback(&ip))
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
    d.domains.retain(|n, _| !managed.domains.contains_key(n));
    d.composites.retain(|n, _| !managed.composites.contains_key(n));
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
            let indexes = t.indexes.iter().filter(|i| mt.index(&i.name).is_none()).cloned().collect();
            Some(Table { columns, constraints, indexes, ..t.clone() })
        })
        .map(|t| (t.name.clone(), t))
        .collect();

    let kept = close_needs(full, &mut d);
    (d, kept)
}

/// `managed` plus what it needs from `full`, transitively (the closure [`clean_desired`] keeps
/// for unmanaged objects): schemas, foreign-key target tables, enums, domains, composites and
/// sequences it uses. Also the unmanaged tables that hold a foreign key to a table of `managed`
/// (so dropping that table fails for them as it does for real), with what they need; nothing
/// else unmanaged.
pub(crate) fn with_needs(full: &Model, managed: &Model) -> Model {
    let mut d = managed.clone();
    for (name, t) in &full.tables {
        let references_managed = t.constraints.iter().any(|k| k.references.as_ref().is_some_and(|r| managed.tables.contains_key(&r.table)));
        if references_managed && !d.tables.contains_key(name) {
            d.tables.insert(name.clone(), t.clone());
        }
    }
    close_needs(full, &mut d);
    d
}

/// Restore into `d` from `full` everything `d` needs, until nothing is missing; what was restored.
fn close_needs(full: &Model, d: &mut Model) -> Vec<Kept> {
    let mut kept = Vec::new();
    loop {
        let mut changed = false;
        for (need, because) in needs(full, d) {
            if let Some(object) = restore(full, d, &need) {
                kept.push(Kept { object, because });
                changed = true;
            }
        }
        if !changed {
            return kept;
        }
    }
}

/// An object an object of the cleaned model needs.
enum Need {
    Schema(String),
    Table(QualName),
    Enum(QualName),
    Domain(QualName),
    Composite(QualName),
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
        if let Some(p) = &t.partition_of {
            out.push((Need::Table(p.parent.clone()), format!("{table} is a partition of it")));
        }
        for c in &t.columns {
            let col = format!("column {}.{}", t.name, pgmig_model::quote_ident(&c.name));
            if let Some(r) = &c.type_ref
                && r != &t.name
            {
                out.extend(type_need(full, r, &col));
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
        for i in &t.indexes {
            let index = format!("index {}", QualName::new(t.name.schema.clone(), i.name.clone()));
            for c in &i.columns {
                out.push((Need::Column(t.name.clone(), c.clone()), format!("{index} uses it")));
            }
        }
    }
    for e in d.enums.values() {
        out.push((Need::Schema(e.name.schema.clone()), format!("enum {} is in it", e.name)));
    }
    for dom in d.domains.values() {
        let what = format!("domain {}", dom.name);
        out.push((Need::Schema(dom.name.schema.clone()), format!("{what} is in it")));
        if let Some(r) = &dom.base_ref {
            out.extend(type_need(full, r, &what));
        }
    }
    for c in d.composites.values() {
        out.push((Need::Schema(c.name.schema.clone()), format!("composite type {} is in it", c.name)));
        for a in &c.attributes {
            let what = format!("attribute {}.{}", c.name, pgmig_model::quote_ident(&a.name));
            if let Some(r) = a.type_ref.as_ref().filter(|r| **r != c.name) {
                out.extend(type_need(full, r, &what));
            }
        }
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

/// What `user` (`column public.t.c`) needs to use type `r`: the enum, domain, composite type or
/// table (its row type) of that name in `full`.
fn type_need(full: &Model, r: &QualName, user: &str) -> Option<(Need, String)> {
    if full.enums.contains_key(r) {
        Some((Need::Enum(r.clone()), format!("{user} uses it")))
    } else if full.domains.contains_key(r) {
        Some((Need::Domain(r.clone()), format!("{user} uses it")))
    } else if full.composites.contains_key(r) {
        Some((Need::Composite(r.clone()), format!("{user} uses it")))
    } else if full.tables.contains_key(r) {
        Some((Need::Table(r.clone()), format!("{user} uses its row type")))
    } else {
        None
    }
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
        Need::Domain(n) => {
            if d.domains.contains_key(n) {
                return None;
            }
            d.domains.insert(n.clone(), full.domains.get(n)?.clone());
            Some(format!("domain {n}"))
        }
        Need::Composite(n) => {
            if d.composites.contains_key(n) {
                return None;
            }
            d.composites.insert(n.clone(), full.composites.get(n)?.clone());
            Some(format!("composite type {n}"))
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
            if t.index(k).is_some() {
                return None;
            }
            // A foreign key's index is a constraint's or, failing that, a unique index.
            let ft = full.tables.get(n)?;
            if let Some(c) = ft.constraint(k) {
                t.constraints.push(c.clone());
                return Some(format!("constraint {} on {n}", pgmig_model::quote_ident(k)));
            }
            t.indexes.push(ft.index(k)?.clone());
            t.indexes.sort_by(|x, y| x.name.cmp(&y.name));
            Some(format!("index {}", QualName::new(n.schema.clone(), k.clone())))
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

    #[test]
    fn loopback_addresses() {
        for a in ["127.0.0.1", "127.1.2.3", "::1", "::ffff:127.0.0.1"] {
            assert!(is_loopback(a), "{a}");
        }
        for a in ["10.0.0.5", "192.168.1.1", "::ffff:10.0.0.1", "2001:db8::1", "", "localhost"] {
            assert!(!is_loopback(a), "{a}");
        }
    }

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
            m.tables.insert(
                n.clone(),
                Table {
                    name: n,
                    columns: cols,
                    comment: None,
                    constraints: vec![],
                    indexes: vec![],
                    partition_key: None,
                    partition_of: None,
                },
            );
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
    #[test]
    fn domains_and_composite_types_an_excluded_table_needs_are_kept_with_what_they_use() {
        let mut full = model();
        let (d, p) = (QualName::new("app", "d"), QualName::new("app", "pair"));
        full.domains.insert(
            d.clone(),
            pgmig_model::Domain {
                name: d.clone(),
                base_type: "app.mood".into(),
                base_ref: Some(QualName::new("app", "mood")),
                collation: None,
                default: None,
                not_null: false,
                constraints: vec![],
                comment: None,
            },
        );
        let attribute = pgmig_model::Attribute {
            name: "a".into(),
            type_name: "app.d".into(),
            type_ref: Some(d.clone()),
            collation: None,
            comment: None,
        };
        full.composites.insert(p.clone(), pgmig_model::Composite { name: p.clone(), attributes: vec![attribute], comment: None });
        let keep = QualName::new("public", "keep");
        full.tables.get_mut(&keep).expect("keep").columns = vec![col("ps", Some(p.clone()))];
        let opts = Options { filter: Filter::parse::<&str, _>(&[], &["table:public.keep"]).expect("filter"), ..Options::default() };
        let (desired, kept) = clean_desired(&full, &opts);
        assert!(desired.composites.contains_key(&p) && desired.domains.contains_key(&d));
        assert!(desired.enums.contains_key(&QualName::new("app", "mood")));
        let shown: Vec<String> = kept.iter().map(|k| format!("{} ({})", k.object, k.because)).collect();
        assert_eq!(
            shown,
            [
                "composite type app.pair (column public.keep.ps uses it)",
                "schema app (composite type app.pair is in it)",
                "domain app.d (attribute app.pair.a uses it)",
                "enum app.mood (domain app.d uses it)",
            ]
        );
        // With nothing needing them they go.
        let (bare, _) = clean_desired(&full, &Options::default());
        assert!(bare.domains.is_empty() && bare.composites.is_empty());
    }

    #[test]
    fn indexes_follow_their_table_and_keep_the_columns_they_use() {
        let index = |table: &str, name: &str, columns: &[&str]| pgmig_model::Index {
            name: name.into(),
            definition: format!("CREATE INDEX {name} ON public.{table} USING btree ({})", columns.join(", ")),
            unique: false,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            comment: None,
            on_partitioned_table: false,
        };
        let mut full = model();
        let gone = QualName::new("public", "gone");
        full.tables.get_mut(&gone).expect("gone").columns.push(col("secret", None));
        full.tables.get_mut(&gone).expect("gone").indexes =
            vec![index("gone", "gone_id_idx", &["id"]), index("gone", "gone_both_idx", &["id", "secret"])];
        full.tables.get_mut(&QualName::new("public", "keep")).expect("keep").indexes = vec![index("keep", "keep_m_idx", &["m"])];
        let opts = Options {
            filter: Filter::parse::<&str, _>(&[], &["table:public.keep", "public.gone.secret"]).expect("filter"),
            ..Options::default()
        };
        let (d, kept) = clean_desired(&full, &opts);
        // The excluded table keeps its index.
        assert_eq!(d.tables[&QualName::new("public", "keep")].indexes, [index("keep", "keep_m_idx", &["m"])]);
        // The index over the excluded column is not managed: it stays, and so does the column it uses.
        let g = &d.tables[&gone];
        assert_eq!(g.indexes, [index("gone", "gone_both_idx", &["id", "secret"])]);
        assert_eq!(g.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "secret"]);
        assert!(
            kept.contains(&Kept { object: "column public.gone.id".into(), because: "index public.gone_both_idx uses it".into() }),
            "{kept:?}"
        );
        let changes = diff(&opts.managed(&full), &opts.managed(&d));
        assert_eq!(changes, [Change::DropIndex { table: gone, index: index("gone", "gone_id_idx", &["id"]) }]);
    }
}
