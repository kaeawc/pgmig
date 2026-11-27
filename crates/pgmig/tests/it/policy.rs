//! The diff policy (`skip`): skipped changes are reported, changes that need them are skipped
//! too, and what is left plans and applies.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use pgmig::api::{self, DiffRequest, PlanApplyRequest};
use pgmig::{ChangeKind, Options, ShadowSpec, SkipCause, Skipped, Source};

use crate::support::{TestDb, postgres_dsn};

/// A temporary directory, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("pgmig-policy-it-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tmp(path)
    }

    fn sql(&self, name: &str, sql: &str) -> Source {
        let path = self.0.join(name);
        std::fs::write(&path, sql).expect("write sql");
        Source::Sql(path)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shadow() -> (ShadowSpec, Option<TestDb>) {
    match postgres_dsn() {
        None => (ShadowSpec::Pgmem, None),
        Some(_) => {
            let db = TestDb::new();
            (ShadowSpec::Postgres(db.conninfo.clone()), Some(db))
        }
    }
}

fn skipping(kinds: &[ChangeKind]) -> Options {
    Options { skip: kinds.to_vec(), ..Options::default() }
}

fn diff(from: &str, to: &str, kinds: &[ChangeKind]) -> api::DiffResponse {
    let t = Tmp::new();
    let (shadow, _keep) = shadow();
    let req = DiffRequest { from: t.sql("from.sql", from), to: t.sql("to.sql", to), shadow, options: skipping(kinds) };
    api::schema_diff(&req).expect("diff")
}

fn shown(skipped: &[Skipped]) -> Vec<String> {
    skipped.iter().map(ToString::to_string).collect()
}

#[test]
fn a_skipped_drop_table_is_reported_and_not_planned() {
    let res =
        diff("CREATE TABLE keep (id int); CREATE TABLE old (id int);", "CREATE TABLE keep (id int, name text);", &[ChangeKind::DropTable]);
    assert_eq!(res.plan.statements, ["ALTER TABLE public.keep ADD COLUMN name text"]);
    assert_eq!(
        res.skipped,
        [Skipped {
            kind: ChangeKind::DropTable,
            object: "public.old".into(),
            statement: Some("DROP TABLE public.old".into()),
            because: None
        }]
    );
    assert_eq!(shown(&res.skipped), ["drop_table public.old (policy)"]);
    assert!(res.changes.iter().all(|c| c.kind() != ChangeKind::DropTable), "{:?}", res.changes);
    let json = serde_json::to_value(&res).expect("json");
    assert_eq!(
        json["skipped"],
        serde_json::json!([{"kind": "drop_table", "object": "public.old", "statement": "DROP TABLE public.old", "because": null}])
    );
    // Without the policy the table is dropped.
    let all = diff("CREATE TABLE old (id int);", "SELECT 1;", &[]);
    assert_eq!((all.plan.statements, all.skipped), (vec!["DROP TABLE public.old".to_owned()], vec![]));
}

#[test]
fn a_schema_whose_table_is_kept_is_kept() {
    let res = diff("CREATE SCHEMA app; CREATE TABLE app.t (id int);", "SELECT 1;", &[ChangeKind::DropTable]);
    assert!(res.plan.is_empty(), "{:?}", res.plan);
    assert_eq!(shown(&res.skipped), ["drop_schema app (because drop_table app.t is skipped)", "drop_table app.t (policy)"]);
    assert_eq!(res.skipped[0].because, Some(SkipCause { kind: ChangeKind::DropTable, object: "app.t".into() }));
    let json = serde_json::to_value(&res).expect("json");
    assert_eq!(json["skipped"][0]["because"], serde_json::json!({"kind": "drop_table", "object": "app.t"}));
}

/// A name, the target's SQL, the desired SQL, the kinds skipped and the notices expected.
type Case = (&'static str, &'static str, &'static str, &'static [ChangeKind], &'static [&'static str]);

/// Applying the plan made under a policy succeeds, and planning again finds nothing but what the
/// policy skips (again: a skipped difference stays).
#[test]
fn plans_with_skipped_changes_apply() {
    let cases: &[Case] = &[
        (
            "schema with a kept table",
            "CREATE SCHEMA app; CREATE TABLE app.t (id int); CREATE TABLE gone (id int);",
            "SELECT 1;",
            &[ChangeKind::DropTable],
            &["drop_schema app (because drop_table app.t is skipped)", "drop_table app.t (policy)", "drop_table public.gone (policy)"],
        ),
        (
            "kept column with constraints",
            "CREATE TABLE p (id int PRIMARY KEY); CREATE TABLE t (id int, gone int REFERENCES p CHECK (gone > 0));",
            "CREATE TABLE t (id int);",
            &[ChangeKind::DropColumn],
            &["drop_column public.t.gone (policy)"],
        ),
        (
            "kept foreign key keeps the referenced table",
            "CREATE TABLE p (id int PRIMARY KEY); CREATE TABLE c (id int, p int CONSTRAINT c_p_fkey REFERENCES p);",
            "CREATE TABLE c (id int, p int);",
            &[ChangeKind::DropConstraint],
            &["drop_table public.p (because drop_constraint public.c.c_p_fkey is skipped)", "drop_constraint public.c.c_p_fkey (policy)"],
        ),
        (
            "redefined constraint",
            "CREATE TABLE t (n int CONSTRAINT n_pos CHECK (n > 0));",
            "CREATE TABLE t (n int CONSTRAINT n_pos CHECK (n > 1));",
            &[ChangeKind::DropConstraint],
            &[
                "drop_constraint public.t.n_pos (policy)",
                "add_constraint public.t.n_pos (because drop_constraint public.t.n_pos is skipped)",
            ],
        ),
        (
            "kept primary key keeps NOT NULL",
            "CREATE TABLE t (id int CONSTRAINT t_pkey PRIMARY KEY, n int NOT NULL);",
            "CREATE TABLE t (id int, n int);",
            &[ChangeKind::DropConstraint],
            &["drop_constraint public.t.t_pkey (policy)", "drop_not_null public.t.id (because drop_constraint public.t.t_pkey is skipped)"],
        ),
        (
            "type change using a cast keeps its default",
            "CREATE TABLE t (c text DEFAULT '1', d int);",
            "CREATE TABLE t (c integer DEFAULT 1, d int NOT NULL);",
            &[ChangeKind::DropDefault],
            &[
                "alter_column_type public.t.c (because drop_default public.t.c is skipped)",
                "set_default public.t.c (because alter_column_type public.t.c is skipped)",
                "drop_default public.t.c (policy)",
            ],
        ),
        (
            "removed serial column keeps its sequence",
            "CREATE TABLE t (id int, n serial); CREATE TABLE gone (n serial);",
            "CREATE TABLE t (id int);",
            &[ChangeKind::DropSequence],
            &[
                "drop_column public.t.n (because drop_sequence public.t_n_seq is skipped)",
                "drop_table public.gone (because drop_sequence public.gone_n_seq is skipped)",
            ],
        ),
        (
            "dropped identity keeps its sequence",
            "CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY, d int);",
            "CREATE TABLE t (id int NOT NULL, d int NOT NULL);",
            &[ChangeKind::DropSequence],
            &["drop_identity public.t.id (because drop_sequence public.t_id_seq is skipped)"],
        ),
        (
            "foreign key over a column keeping its type",
            "CREATE TABLE parent (id int PRIMARY KEY); CREATE TABLE child (pid text, n int);",
            "CREATE TABLE parent (id int PRIMARY KEY); CREATE TABLE child (pid int CONSTRAINT child_pid_fkey REFERENCES parent (id), n int NOT NULL);",
            &[ChangeKind::AlterColumnType],
            &[
                "alter_column_type public.child.pid (policy)",
                "add_constraint public.child.child_pid_fkey (because alter_column_type public.child.pid is skipped)",
            ],
        ),
        (
            "default naming an enum value not added",
            "CREATE TYPE mood AS ENUM ('a'); CREATE TABLE t (m mood); CREATE TABLE v (m mood);",
            "CREATE TYPE mood AS ENUM ('a', 'b'); CREATE TABLE t (m mood DEFAULT 'b'); CREATE TABLE v (m mood DEFAULT 'a'); \
             CREATE TABLE u (m mood DEFAULT 'b');",
            &[ChangeKind::AddEnumValue],
            &[
                "add_enum_value public.mood (policy)",
                "set_default public.t.m (because add_enum_value public.mood is skipped)",
                "create_table public.u (because add_enum_value public.mood is skipped)",
            ],
        ),
        (
            "default naming an enum value not renamed",
            "CREATE TYPE mood AS ENUM ('a', 'old'); CREATE TABLE t (m mood); CREATE TABLE v (m mood);",
            "CREATE TYPE mood AS ENUM ('a', 'new'); CREATE TABLE t (m mood DEFAULT 'new'); CREATE TABLE v (m mood DEFAULT 'a');",
            &[ChangeKind::RenameEnumValue],
            &["rename_enum_value public.mood (policy)", "set_default public.t.m (because rename_enum_value public.mood is skipped)"],
        ),
        (
            "replaced index under a kept foreign key",
            "CREATE TABLE p (id int); CREATE UNIQUE INDEX p_id_idx ON p (id); CREATE TABLE c (pid int REFERENCES p (id));",
            "CREATE TABLE p (id int); CREATE UNIQUE INDEX p_id_idx ON p (id) NULLS NOT DISTINCT; CREATE TABLE c (pid int REFERENCES p (id));",
            &[ChangeKind::DropIndex],
            &[
                "drop_index public.p_id_idx (policy)",
                "create_index public.p_id_idx (because drop_index public.p_id_idx is skipped)",
                "add_constraint public.c.c_pid_fkey (because create_index public.p_id_idx is skipped)",
                "drop_constraint public.c.c_pid_fkey (because add_constraint public.c.c_pid_fkey is skipped)",
            ],
        ),
        (
            "replaced unique constraint under a kept foreign key",
            "CREATE TABLE p (id int CONSTRAINT p_id_key UNIQUE); CREATE TABLE c (pid int REFERENCES p (id));",
            "CREATE TABLE p (id int CONSTRAINT p_id_key UNIQUE NULLS NOT DISTINCT); CREATE TABLE c (pid int REFERENCES p (id));",
            &[ChangeKind::DropConstraint],
            &[
                "drop_constraint public.p.p_id_key (policy)",
                "add_constraint public.p.p_id_key (because drop_constraint public.p.p_id_key is skipped)",
                "drop_constraint public.c.c_pid_fkey (policy)",
                "add_constraint public.c.c_pid_fkey (because add_constraint public.p.p_id_key is skipped)",
            ],
        ),
    ];
    for (name, from, to, kinds, want) in cases {
        let t = Tmp::new();
        let db = TestDb::new();
        db.connect().batch(from).expect("load target");
        let (shadow, _keep) = shadow();
        let req =
            PlanApplyRequest { target: db.conninfo.clone(), desired: t.sql("to.sql", to), shadow, options: skipping(kinds), verify: false };
        let res = api::schema_plan_apply(&req).unwrap_or_else(|e| panic!("{name}: plan: {e}"));
        let mut got = shown(&res.skipped);
        let mut want_sorted: Vec<String> = want.iter().map(|s| (*s).to_owned()).collect();
        got.sort();
        want_sorted.sort();
        assert_eq!(got, want_sorted, "{name}: {:?}", res.plan.statements);
        assert!(
            res.skipped.iter().all(|s| !res.plan.statements.iter().any(|st| Some(st) == s.statement.as_ref())),
            "{name}: skipped statement planned: {:?}",
            res.plan.statements
        );
        api::apply(&api::ApplyRequest { target: db.conninfo.clone(), plan: res.plan.clone() })
            .unwrap_or_else(|e| panic!("{name}: apply {:?}: {e}", res.plan.statements));
        let again = api::schema_plan_apply(&req).unwrap_or_else(|e| panic!("{name}: plan again: {e}"));
        assert!(again.plan.is_empty(), "{name}: left to change: {:?}", again.plan.statements);
        assert!(!again.skipped.is_empty(), "{name}: the skipped difference stays and is reported again");
        assert!(again.skipped.iter().all(|s| res.skipped.contains(s)), "{name}: {:?}", again.skipped);
    }
}

/// A foreign key dropped to replace the index it uses is kept with that index: a skipped
/// `drop_index` (or `drop_constraint`) keeps the foreign key's drop out with its re-add.
#[test]
fn a_kept_replacement_keeps_the_foreign_key_using_it() {
    let cases: &[(&str, &str, ChangeKind)] = &[
        (
            "CREATE TABLE p (id int); CREATE UNIQUE INDEX p_id_idx ON p (id); CREATE TABLE c (pid int REFERENCES p (id));",
            "CREATE TABLE p (id int); CREATE UNIQUE INDEX p_id_idx ON p (id) NULLS NOT DISTINCT; CREATE TABLE c (pid int REFERENCES p (id));",
            ChangeKind::DropIndex,
        ),
        (
            "CREATE TABLE p (id int CONSTRAINT p_id_key UNIQUE); CREATE TABLE c (pid int REFERENCES p (id));",
            "CREATE TABLE p (id int CONSTRAINT p_id_key UNIQUE NULLS NOT DISTINCT); CREATE TABLE c (pid int REFERENCES p (id));",
            ChangeKind::DropConstraint,
        ),
    ];
    for (from, to, kind) in cases {
        let t = Tmp::new();
        let db = TestDb::new();
        db.connect().batch(from).expect("load target");
        let (shadow, _keep) = shadow();
        let req = PlanApplyRequest {
            target: db.conninfo.clone(),
            desired: t.sql("to.sql", to),
            shadow,
            options: skipping(&[*kind]),
            verify: false,
        };
        let res = api::schema_plan_apply(&req).expect("plan");
        assert!(
            res.plan.statements.iter().all(|s| !s.contains("c_pid_fkey")),
            "{kind}: the foreign key is dropped or added: {:?}",
            res.plan.statements
        );
        api::apply(&api::ApplyRequest { target: db.conninfo.clone(), plan: res.plan.clone() }).expect("apply");
        let rows = db.connect().query("SELECT count(*) FROM pg_constraint WHERE conname = 'c_pid_fkey' AND contype = 'f'").expect("query");
        assert_eq!(rows[0][0].as_deref(), Some("1"), "{kind}: the foreign key is gone");
    }
}

#[test]
fn migrate_diff_leaves_skipped_changes_out_of_the_file() {
    let t = Tmp::new();
    let dir = t.0.join("migrations");
    let (shadow, _keep) = shadow();
    let opts = skipping(&[ChangeKind::DropTable]);
    let first =
        pgmig::migrate_diff(&dir, "init", &t.sql("a.sql", "CREATE TABLE keep (id int); CREATE TABLE old (id int);"), &shadow, &opts)
            .expect("first");
    assert!(!first.written.is_empty() && first.skipped.is_empty());
    let to = t.sql("b.sql", "CREATE TABLE keep (id int, name text);");
    let res = pgmig::migrate_diff(&dir, "next", &to, &shadow, &opts).expect("second");
    let file = res.written.into_iter().next().expect("written").path;
    let sql = std::fs::read_to_string(&file).expect("read");
    assert_eq!(sql, "ALTER TABLE public.keep ADD COLUMN name text;\n");
    assert_eq!(shown(&res.skipped), ["drop_table public.old (policy)"]);
    // Only skipped changes left: nothing is written, and they are still reported.
    let res = pgmig::migrate_diff(&dir, "again", &to, &shadow, &opts).expect("third");
    assert!(res.written.is_empty() && res.plan.is_empty());
    assert_eq!(shown(&res.skipped), ["drop_table public.old (policy)"]);
    // The response carries them too.
    let req = api::MigrateDiffRequest { dir, name: "again".into(), to, shadow, options: opts, editor: None, verify: false };
    let json = serde_json::to_value(api::migrate_diff(&req).expect("api")).expect("json");
    assert_eq!(json["skipped"][0]["kind"], "drop_table");
    assert_eq!(json["file"], serde_json::Value::Null);
}
