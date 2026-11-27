//! Domains and composite types: what inspection reads, and the changes the planner refuses.

use pgmig::{Change, diff, inspect, plan};
use pgmig_model::QualName;

use crate::support::{TestDb, shadow};

#[test]
fn domains_and_composite_types_are_inspected_with_what_they_use() {
    let db = TestDb::new();
    let mut conn = db.connect();
    conn.batch(
        "CREATE SCHEMA app; \
         CREATE TYPE app.mood AS ENUM ('sad', 'happy'); \
         CREATE DOMAIN app.posint AS integer NOT NULL DEFAULT 1 CONSTRAINT posint_check CHECK (VALUE > 0); \
         ALTER DOMAIN app.posint ADD CONSTRAINT posint_small CHECK (VALUE < 100) NOT VALID; \
         CREATE DOMAIN public.moods AS app.mood[]; \
         CREATE TYPE public.reading AS (n app.posint, ms public.moods, at timestamp with time zone); \
         CREATE TABLE public.readings (r public.reading, rs public.reading[]); \
         COMMENT ON TYPE app.posint IS 'positive';",
    )
    .expect("load");
    let model = inspect(&mut conn).expect("inspect");

    let posint = &model.domains[&QualName::new("app", "posint")];
    assert_eq!((posint.base_type.as_str(), &posint.base_ref, &posint.collation), ("integer", &None, &None));
    assert_eq!((posint.default.as_deref(), posint.not_null, posint.comment.as_deref()), (Some("1"), true, Some("positive")));
    // NOT NULL is the flag, not a constraint (Postgres 17 records one).
    let checks: Vec<(&str, &str, bool)> = posint.constraints.iter().map(|c| (c.name.as_str(), c.body(), c.validated)).collect();
    assert_eq!(checks, [("posint_check", "CHECK ((VALUE > 0))", true), ("posint_small", "CHECK ((VALUE < 100))", false)]);

    let moods = &model.domains[&QualName::new("public", "moods")];
    assert_eq!((moods.base_type.as_str(), &moods.base_ref), ("app.mood[]", &Some(QualName::new("app", "mood"))));

    // Only the composite type made with CREATE TYPE: the table's row type is the table's.
    assert_eq!(model.composites.keys().collect::<Vec<_>>(), [&QualName::new("public", "reading")]);
    let reading = &model.composites[&QualName::new("public", "reading")];
    let attributes: Vec<(&str, &str, Option<&QualName>)> =
        reading.attributes.iter().map(|a| (a.name.as_str(), a.type_name.as_str(), a.type_ref.as_ref())).collect();
    assert_eq!(
        attributes,
        [
            ("n", "app.posint", Some(&QualName::new("app", "posint"))),
            ("ms", "public.moods", Some(&QualName::new("public", "moods"))),
            ("at", "timestamp with time zone", None),
        ]
    );
    let columns: Vec<Option<&QualName>> =
        model.tables[&QualName::new("public", "readings")].columns.iter().map(|c| c.type_ref.as_ref()).collect();
    assert_eq!(columns, [Some(&QualName::new("public", "reading")); 2]);
}

/// The plan of the changes from `from` (loaded into a target) to `to` (loaded into a shadow).
fn changes(from: &str, to: &str) -> Vec<Change> {
    let target = TestDb::new();
    let mut conn = target.connect();
    conn.batch(from).expect("load from");
    let a = inspect(&mut conn).expect("inspect target");
    let (mut sh, _db) = shadow();
    sh.load(to).expect("load to");
    let b = inspect(sh.conn()).expect("inspect shadow");
    diff(&a, &b)
}

#[test]
fn a_new_base_type_for_a_domain_a_column_keeps_using_is_refused() {
    let changes = changes(
        "CREATE DOMAIN public.amount AS integer; CREATE TABLE public.t (a public.amount);",
        "CREATE DOMAIN public.amount AS bigint; CREATE TABLE public.t (a public.amount);",
    );
    let err = plan(&changes).expect_err("unsupported");
    assert_eq!(
        err.to_string(),
        "unsupported changes: domain public.amount: the base type cannot change from integer to bigint; Postgres cannot change it in \
         place, and the domain cannot be dropped and created again while column public.t.a uses it"
    );
}

#[test]
fn retyping_an_attribute_of_a_composite_type_a_column_uses_is_refused() {
    let changes = changes(
        "CREATE TYPE public.pair AS (a integer); CREATE TABLE public.t (ps public.pair[]);",
        "CREATE TYPE public.pair AS (a bigint); CREATE TABLE public.t (ps public.pair[]);",
    );
    let err = plan(&changes).expect_err("unsupported");
    assert_eq!(
        err.to_string(),
        "unsupported changes: composite type public.pair: attribute a cannot change type from integer to bigint while column public.t.ps \
         uses the type"
    );
}
