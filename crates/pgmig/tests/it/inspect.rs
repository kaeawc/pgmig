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

#[test]
fn indexes_inspect_without_those_constraints_own() {
    use pgmig_model::QualName;

    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch(
        "CREATE SCHEMA app; \
         CREATE TABLE app.items (id int PRIMARY KEY, code text UNIQUE, n int, note text); \
         CREATE UNIQUE INDEX items_n_idx ON app.items (n); \
         CREATE INDEX items_lower_note_idx ON app.items (lower(note)) WHERE n > 0; \
         COMMENT ON INDEX app.items_n_idx IS 'by n'; \
         CREATE TABLE app.lines (n int REFERENCES app.items (n));",
    )
    .expect("load");
    let model = pgmig::inspect(&mut conn).expect("inspect");
    let items = &model.tables[&QualName::new("app", "items")];
    let names: Vec<&str> = items.indexes.iter().map(|i| i.name.as_str()).collect();
    // The primary key's and the unique constraint's indexes are the constraints'.
    assert_eq!(names, ["items_lower_note_idx", "items_n_idx"]);
    let n = items.index("items_n_idx").expect("items_n_idx");
    assert_eq!(n.definition, "CREATE UNIQUE INDEX items_n_idx ON app.items USING btree (n)");
    assert!(n.unique);
    assert_eq!(n.columns, ["n"]);
    assert_eq!(n.comment.as_deref(), Some("by n"));
    let lower = items.index("items_lower_note_idx").expect("items_lower_note_idx");
    assert_eq!(lower.definition, "CREATE INDEX items_lower_note_idx ON app.items USING btree (lower(note)) WHERE (n > 0)");
    assert!(!lower.unique);
    // pgmem records no pg_depend rows for index expressions (kaeawc/pgmem#155) and leaves these
    // columns out; indkey has none of them.
    if postgres_dsn().is_some() {
        assert_eq!(lower.columns, ["n", "note"]);
    }
    // A foreign key's referenced index is the unique index, which stays an index.
    let fk = &model.tables[&QualName::new("app", "lines")].constraints[0];
    assert_eq!(fk.references.as_ref().and_then(|r| r.constraint.as_deref()), Some("items_n_idx"));
}
