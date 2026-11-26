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
        let plan = pgmig::schema_inspect(&Source::Sql(file.clone()), &spec).expect("inspect");
        assert_eq!(plan.to_sql(), "CREATE TABLE public.t (\n    id integer\n);\n");
    }
    let _ = std::fs::remove_file(&file);
}
