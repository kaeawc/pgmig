//! Declarative partitioning: what a partition's model holds, refusals, and `schema clean`.

use pgmig::{Model, Options, apply, diff, inspect, plan, schema_plan_clean};
use pgmig_model::{ConstraintKind, PartitionOf, QualName};

use crate::support::{TestDb, shadow};

fn q(name: &str) -> QualName {
    QualName::new("public", name)
}

fn model(sql: &str) -> Model {
    let db = TestDb::new();
    db.connect().batch(sql).expect("load");
    inspect(&mut db.connect()).expect("inspect")
}

const PARENT: &str = "
    CREATE TABLE t (id int NOT NULL, v text DEFAULT 'x', CONSTRAINT t_pkey PRIMARY KEY (id), CONSTRAINT t_v_check CHECK (v <> ''))
        PARTITION BY RANGE (id);
    CREATE INDEX t_v_idx ON t (v);
";

#[test]
fn a_partition_holds_only_what_is_its_own_however_it_was_made() {
    let m = model(&format!(
        "{PARENT}
         CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100);
         -- Attached: its columns, key, check and index were its own before.
         CREATE TABLE t_2 (id int NOT NULL, v text DEFAULT 'x', CONSTRAINT t_2_pkey PRIMARY KEY (id), CONSTRAINT t_v_check CHECK (v <> ''));
         CREATE INDEX t_2_v_idx ON t_2 (v);
         ALTER TABLE t ATTACH PARTITION t_2 FOR VALUES FROM (100) TO (200);
         -- What is the partition's own: a check, an index, a column comment.
         CREATE TABLE t_3 PARTITION OF t (CONSTRAINT t_3_small CHECK (id < 300)) FOR VALUES FROM (200) TO (300);
         CREATE INDEX t_3_id_v_idx ON t_3 (id, v);
         COMMENT ON COLUMN t_3.v IS 'mine';
         CREATE TABLE t_other PARTITION OF t DEFAULT;"
    ));
    let t = &m.tables[&q("t")];
    assert_eq!(t.partition_key.as_deref(), Some("RANGE (id)"));
    assert_eq!(t.partition_of, None);
    assert_eq!(t.columns.len(), 2);
    for name in ["t_1", "t_2", "t_other"] {
        let p = &m.tables[&q(name)];
        assert!(p.columns.is_empty(), "{name}: {:?}", p.columns);
        assert!(p.constraints.is_empty(), "{name}: {:?}", p.constraints);
        assert!(p.indexes.is_empty(), "{name}: {:?}", p.indexes);
        assert_eq!(p.partition_key, None);
    }
    assert_eq!(m.tables[&q("t_1")].partition_of, Some(PartitionOf { parent: q("t"), bound: "FOR VALUES FROM (1) TO (100)".into() }));
    assert_eq!(m.tables[&q("t_other")].partition_of.as_ref().map(|p| p.bound.as_str()), Some("DEFAULT"));
    let own = &m.tables[&q("t_3")];
    assert_eq!(own.columns.iter().map(|c| (c.name.as_str(), c.comment.as_deref())).collect::<Vec<_>>(), [("v", Some("mine"))]);
    assert_eq!(own.constraints.iter().map(|c| (c.name.as_str(), c.kind)).collect::<Vec<_>>(), [("t_3_small", ConstraintKind::Check)]);
    assert_eq!(own.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["t_3_id_v_idx"]);
    // The partition's columns as the database has them: the parent's, with its own in place.
    let columns = m.table_columns(&q("t_3")).expect("columns");
    assert_eq!(
        columns.iter().map(|c| (c.name.as_str(), c.not_null, c.comment.as_deref())).collect::<Vec<_>>(),
        [("id", true, None), ("v", false, Some("mine"))]
    );

    // The JSON carries the new fields only where they are set, and reads back.
    let json = serde_json::to_string(&m).expect("serialize");
    assert_eq!(serde_json::from_str::<Model>(&json).expect("deserialize"), m);
    let plain = serde_json::to_string(&model("CREATE TABLE plain (id int);")).expect("serialize");
    assert!(!plain.contains("partition"), "{plain}");
}

#[test]
fn a_partition_of_a_partition_holds_only_its_own_too() {
    let m = model(&format!(
        "{PARENT}
         CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100) PARTITION BY RANGE (id);
         CREATE TABLE t_1_a PARTITION OF t_1 FOR VALUES FROM (1) TO (50);"
    ));
    let sub = &m.tables[&q("t_1")];
    assert_eq!(sub.partition_key.as_deref(), Some("RANGE (id)"));
    for name in ["t_1", "t_1_a"] {
        let p = &m.tables[&q(name)];
        assert!(p.columns.is_empty() && p.constraints.is_empty() && p.indexes.is_empty(), "{name}: {p:?}");
    }
    assert_eq!(m.partition_ancestors(&q("t_1_a")).iter().map(|t| t.name.name.as_str()).collect::<Vec<_>>(), ["t_1", "t"]);
}

#[test]
fn a_new_partition_key_is_refused_naming_the_table() {
    let from = model("CREATE TABLE t (id int, k text) PARTITION BY RANGE (id);");
    let (mut sh, _db) = shadow();
    sh.load("CREATE TABLE t (id int, k text) PARTITION BY LIST (k);").expect("load");
    let to = inspect(sh.conn()).expect("inspect");
    let err = plan(&diff(&from, &to)).expect_err("refused");
    let pgmig_plan::Error::Unsupported(messages) = &err else { panic!("{err:?}") };
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].starts_with("table public.t: the partitioning cannot change from PARTITION BY RANGE (id) to PARTITION BY LIST (k)"),
        "{messages:?}"
    );
    // A table that becomes partitioned, or stops being, is refused the same way.
    let plain = model("CREATE TABLE t (id int, k text);");
    assert!(plan(&diff(&plain, &from)).is_err());
    assert!(plan(&diff(&from, &plain)).is_err());
}

#[test]
fn clean_drops_partitions_before_their_parents() {
    let db = TestDb::new();
    db.connect()
        .batch(&format!(
            "{PARENT}
             CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100) PARTITION BY RANGE (id);
             CREATE TABLE t_1_a PARTITION OF t_1 FOR VALUES FROM (1) TO (50);
             CREATE TABLE t_2 PARTITION OF t DEFAULT;"
        ))
        .expect("load");
    let c = schema_plan_clean(&db.conninfo, &Options::default()).expect("plan clean");
    assert_eq!(c.plan.statements, ["DROP TABLE public.t_1_a", "DROP TABLE public.t_1", "DROP TABLE public.t_2", "DROP TABLE public.t"]);
    apply(&db.conninfo, &c.plan).expect("apply");
    assert_eq!(Options::default().managed(&inspect(&mut db.connect()).expect("inspect")), Model::empty_database());
}

/// On Postgres the catalog marks what a partition has from its parent, so an index or unique
/// constraint of the partition's own stays in its model even when it is defined like its
/// parent's. pgmem leaves the marks unset (kaeawc/pgmem#205, #288, #289), and pgmig then takes
/// what is defined like the parent's for the parent's.
#[test]
fn a_partitions_own_index_and_key_defined_like_its_parents_stay_its_own() {
    let base = "
        CREATE TABLE t (id int NOT NULL, v text, CONSTRAINT t_id_key UNIQUE (id)) PARTITION BY RANGE (id);
        CREATE INDEX t_v_idx ON t (v);
        CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100);";
    let own = format!(
        "{base}
         CREATE INDEX t_1_dup ON t_1 (v);
         ALTER TABLE t_1 ADD CONSTRAINT t_1_own UNIQUE (id);"
    );
    let m = model(&own);
    let t_1 = &m.tables[&q("t_1")];
    let indexes: Vec<&str> = t_1.indexes.iter().map(|i| i.name.as_str()).collect();
    let constraints: Vec<&str> = t_1.constraints.iter().map(|c| c.name.as_str()).collect();
    let changes = diff(&model(base), &m);
    let mut statements = plan(&changes).expect("plan").statements;
    statements.sort();
    if crate::support::postgres_dsn().is_some() {
        assert_eq!(indexes, ["t_1_dup"]);
        assert_eq!(constraints, ["t_1_own"]);
        assert_eq!(
            statements,
            ["ALTER TABLE public.t_1 ADD CONSTRAINT t_1_own UNIQUE (id)", "CREATE INDEX t_1_dup ON public.t_1 USING btree (v)"]
        );
    } else {
        // kaeawc/pgmem#289 (the index), kaeawc/pgmem#205 (the constraint).
        assert!(indexes.is_empty(), "{indexes:?}");
        assert!(constraints.is_empty(), "{constraints:?}");
        assert!(statements.is_empty(), "{statements:?}");
    }
}
