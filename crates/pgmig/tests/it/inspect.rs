//! `schema inspect` against a new database.

use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::{ShadowSpec, Source};

use crate::support::{TestDb, postgres_dsn};

#[test]
fn inspect_has_no_changes_to_a_new_database() {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let file = std::env::temp_dir().join(format!("pgmig-inspect-{}-{}.sql", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&file, "CREATE TABLE t (id int);\n").expect("write sql");
    let db = postgres_dsn().map(|_| TestDb::new());
    let spec = db.as_ref().map_or(ShadowSpec::Pgmem, |d| ShadowSpec::Postgres(d.conninfo.clone()));
    // Twice: the second run uses a shadow the first one wiped.
    for _ in 0..2 {
        let plan = pgmig::schema_inspect(&Source::Sql(file.clone()), &spec, &pgmig::Options::default()).expect("inspect");
        assert_eq!(plan.to_sql(), "CREATE TABLE public.t (\n    id integer\n);\n");
    }
    let _ = std::fs::remove_file(&file);
}

#[test]
fn constraints_outside_public_inspect_with_their_tables_schema() {
    use pgmig_model::{ConstraintKind, QualName};

    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch(
        "CREATE SCHEMA app; \
         CREATE TABLE app.items (id int CONSTRAINT items_pkey PRIMARY KEY, code text CONSTRAINT items_code_key UNIQUE); \
         CREATE TABLE public.lines (item int CONSTRAINT lines_item_fkey REFERENCES app.items DEFERRABLE, qty int CHECK (qty > 0)); \
         COMMENT ON CONSTRAINT items_pkey ON app.items IS 'key';",
    )
    .expect("load");
    let model = pgmig::inspect(&mut conn).expect("inspect");
    let items = &model.tables[&QualName::new("app", "items")];
    let names: Vec<(&str, ConstraintKind)> = items.constraints.iter().map(|c| (c.name.as_str(), c.kind)).collect();
    assert_eq!(names, [("items_code_key", ConstraintKind::Unique), ("items_pkey", ConstraintKind::PrimaryKey)]);
    let pkey = items.constraint("items_pkey").expect("items_pkey");
    assert_eq!(pkey.definition, "PRIMARY KEY (id)");
    assert_eq!(pkey.columns, ["id"]);
    assert_eq!(pkey.comment.as_deref(), Some("key"));

    let lines = &model.tables[&QualName::new("public", "lines")];
    let names: Vec<&str> = lines.constraints.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["lines_item_fkey", "lines_qty_check"]);
    let fk = lines.constraint("lines_item_fkey").expect("fk");
    assert_eq!(fk.definition, "FOREIGN KEY (item) REFERENCES app.items(id) DEFERRABLE");
    assert!(fk.deferrable && !fk.deferred && fk.validated);
    let r = fk.references.as_ref().expect("references");
    assert_eq!(
        (&r.table, &r.columns[..], r.constraint.as_deref()),
        (&QualName::new("app", "items"), &["id".to_owned()][..], Some("items_pkey"))
    );
    assert_eq!(lines.constraint("lines_qty_check").map(|c| c.columns.clone()), Some(vec!["qty".to_owned()]));
}
