//! Turns [`Change`]s into ordered SQL.
//!
//! Each change declares the objects it creates, drops and references ([`graph::Deps`]); the
//! statements are ordered by that dependency graph, so creates run after what they reference,
//! drops run before what they reference, and an alter runs after the creates it needs and before
//! the drops it unblocks. Unrelated statements keep a fixed phase order (create schemas, create
//! tables, alter columns, drop columns, tables, schemas) and then the diff's order. See
//! [`graph`] for the rules and for how cycles are reported.
//!
//! # Transactions
//!
//! `ALTER TYPE ... ADD VALUE` may run in a transaction, but the new value cannot be used (as a
//! column default, say) until that transaction commits. So the plan puts every `ADD VALUE` first
//! and, when other statements follow, marks a commit point after them ([`Plan::commit_after`]):
//! [`Plan::units`] are the stretches to run in separate transactions, and [`Plan::to_sql`] writes
//! the point as a comment and the `-- pgmig:commit` directive ([`COMMIT_MARKER`]), which
//! `migrate apply` commits at when it runs a file `migrate diff` wrote. pgmig does not check which
//! later statements use a new value; any statement after an `ADD VALUE` waits for the commit.
//!
//! # Sequences
//!
//! `CREATE SEQUENCE` and identity columns print only the options that differ from what Postgres
//! would pick for the type and direction ([`SequenceOptions::defaults`]); `ALTER SEQUENCE` and
//! `ALTER COLUMN ... SET` print the options that changed (and both bounds when the type changes,
//! since Postgres resets bounds that were the old type's limits). `OWNED BY` is set after the
//! owning table exists, so a `serial` column plans as `CREATE SEQUENCE`, `CREATE TABLE` with its
//! `nextval` default, then `ALTER SEQUENCE ... OWNED BY`. Plans never `RESTART` a sequence: a
//! sequence whose current value falls outside new bounds fails to apply.
//!
//! # Constraints
//!
//! A new table's primary key, unique, check and exclusion constraints are written in its `CREATE
//! TABLE`; foreign keys are always `ALTER TABLE ... ADD CONSTRAINT` steps after the tables they
//! join (and the constraint whose index they use), so tables that reference each other plan
//! without a cycle. A foreign key is dropped before the table or constraint it references; a
//! constraint is dropped before the columns it uses change or go, and added after they are set.
//!
//! # Indexes
//!
//! `CREATE INDEX` is the statement the server printed for the index (schema-qualified, whatever
//! [`PlanOptions`] says), run after the table and the columns it uses exist and have settled.
//! `DROP INDEX` runs before the columns it uses change or go, and before a constraint or index of
//! the same name is created. A table's indexes are dropped with it.
//!
//! With [`PlanOptions::concurrent_indexes`] the plan creates and drops indexes `CONCURRENTLY`, so
//! writes to the table are not blocked while an index is built or removed. Postgres refuses these
//! statements inside a transaction block, so each one is a unit of its own that runs outside a
//! transaction (see [Units](#units)). Four kinds of index keep the plain statement, in the
//! transaction with the rest: one on a table the plan creates (the table is new and empty, so
//! there is nothing to wait for), one on a table the plan drops, one a constraint owns (the
//! constraint drops it, as without the policy, and creates it as below), and one on a partitioned
//! table (Postgres does not allow `CONCURRENTLY` there; see
//! [`pgmig_model::Index::on_partitioned_table`]). A `CONCURRENTLY` build that fails leaves an
//! `INVALID` index behind; `DROP INDEX CONCURRENTLY` removes it.
//!
//! [`PlanOptions::if_exists`] writes `IF NOT EXISTS` on the creates Postgres has it for (`CREATE
//! SCHEMA`, `TABLE`, `SEQUENCE`, `INDEX [CONCURRENTLY]`, `ADD COLUMN`) and `IF EXISTS` on drops
//! (`DROP SCHEMA`, `TABLE`, `SEQUENCE`, `INDEX`, `TYPE`, `DOMAIN`, `DROP COLUMN`, `DROP
//! CONSTRAINT`, `DROP ATTRIBUTE`); `CREATE TYPE`, `CREATE DOMAIN` and `ADD CONSTRAINT` have no such
//! form and stay as they are. [`PlanOptions::cascade`] writes `CASCADE` on the drops except `DROP
//! ATTRIBUTE` and `DROP INDEX CONCURRENTLY` (which has none); it can drop objects outside the diff
//! (views, functions) without listing them.
//!
//! The policy also adds a primary key or unique constraint to a table that exists before the plan
//! in two steps: `CREATE UNIQUE INDEX CONCURRENTLY` builds its index under the constraint's name,
//! in a unit of its own outside a transaction (the statement is
//! [`pgmig_model::Constraint::index_definition`], what the server printed for the index), then
//! `ALTER TABLE ... ADD CONSTRAINT <name> {PRIMARY KEY | UNIQUE} USING INDEX <name>` (with the
//! constraint's `DEFERRABLE` and `INITIALLY DEFERRED`) attaches it in a transaction, under a brief
//! lock instead of one held for the whole build. The two steps take the constraint's place in the
//! order: after the columns it uses are added and set (those statements commit before the build
//! starts), and before the foreign keys that reference it. The plain `ADD CONSTRAINT` stays for a
//! constraint on a table the plan creates or drops, on a table that loses a primary key or unique
//! constraint in the same plan (a replaced key stays in one transaction, so the table never goes
//! without it, or without the foreign keys to it, while an index builds), for a key in a
//! tablespace (`pg_get_indexdef` leaves the tablespace out), on a partitioned table (Postgres allows neither
//! `CONCURRENTLY` nor `USING INDEX` there; [`pgmig_model::Constraint::on_partitioned_table`]), for
//! an exclusion constraint, and for an index `USING INDEX` cannot take (not a b-tree, such as a key
//! `WITHOUT OVERLAPS`). A primary key needs its columns `NOT NULL`, and `USING INDEX` sets what is
//! not set with a scan under an `ACCESS EXCLUSIVE` lock; the diff already sets them before the key
//! (`SET NOT NULL`, which takes that scan itself), so the attach finds them set. A failed build
//! leaves an `INVALID` index under the constraint's name; a failed attach leaves a valid index
//! that the next diff drops before planning the constraint again.
//!
//! # Units
//!
//! A plan runs as an ordered list of units ([`Plan::units`]): a transactional unit is statements
//! that run in one transaction, a non-transactional unit is one statement that runs outside any
//! transaction and commits by itself. The statements keep their order; the units are where a
//! transaction has to end. An enum commit point ([`Plan::commit_after`]) ends one, and each
//! statement in [`Plan::non_transactional`] is one of its own. Earlier units stay committed when a
//! later one fails.
//!
//! # Objects outside the changes
//!
//! A change may reference an object no change creates or drops: one that already exists and is
//! left alone, or one pgmig does not manage (filtered out, or outside the schema in scope). Such a
//! reference orders nothing; the plan assumes the object exists.
//!
//! Changes Postgres cannot make in place (removing or reordering enum values, a new base type for a
//! domain something keeps using, a new attribute type for a composite type a table column uses, a
//! new partition key; see `pgmig_diff` for the partition changes) fail the plan with
//! [`Error::Unsupported`].
//!
//! # Partitions
//!
//! A partitioned table is `CREATE TABLE ... PARTITION BY <key>`; a partition is `CREATE TABLE ...
//! PARTITION OF <parent> [(<its own constraints>)] <bound>` (and `PARTITION BY` when it is
//! partitioned in turn), created after its parent and dropped before it. A `DETACH PARTITION` that
//! leaves the table standing alone runs before the changes to it and to its parent's columns,
//! constraints and indexes (which would reach it); `ATTACH PARTITION` runs after the table and its
//! parent exist, the parent's changes are made, and the table's columns and checks are those of
//! its parent. A re-bound partition (detached and attached again with a new bound) is detached
//! after its parent's changes, which reach it while it is attached, and attached right after
//! ([`graph::Object::Inherited`], [`graph::Object::Partition`]). An index on a partitioned table,
//! which Postgres prints `ON ONLY`, is created on its partitions too.
//!
//! # Domains and composite types
//!
//! A domain or composite type is created after the types it is over or its attributes use, and
//! before the tables, columns, domains and composite types that use it; it is dropped after them.
//! `CREATE DOMAIN` carries the validated checks, and `NOT VALID` ones are added after it.
//!
//! # Renames
//!
//! The renames of rename hints (`RenameTable`, `RenameColumn`, `RenameEnum`, `RenameDomain`,
//! `RenameCompositeType`, `RenameSequence`, `RenameIndex`; see [`pgmig_diff::rename`]) are not
//! sorted: they run first, before even the added enum values, because every other change names the
//! objects as they are called once the renames are made. Types, sequences and tables go first, then
//! columns and indexes, which are named by their table's new name. A rename never uses `IF EXISTS`,
//! and its statement is the same whatever [`PlanOptions::if_exists`] says. They are no commit point:
//! only an added enum value is. The policy ([`skip`]) still sees what each creates (the new name)
//! and drops (the old one), so a skipped rename takes with it the changes that name the new object.
//!
//! # Policy
//!
//! [`skip`] leaves chosen kinds of change out before planning, with the changes that depend on
//! them (see [`policy`]).

pub mod graph;
pub mod policy;

pub use policy::{Cause, Skipped, skip};

use std::collections::BTreeSet;

use graph::{Deps, Node, Object};
use pgmig_diff::{Change, CommentTarget, EnumPosition};
use pgmig_model::{
    Column, Constraint, ConstraintKind, DomainConstraint, Generated, Identity, OwnedBy, QualName, SequenceOptions, quote_ident,
    quote_literal,
};
use serde::{Deserialize, Serialize};

/// Serialized as `{"statements": [...], "commit_after": <index or null>}`, and
/// `"non_transactional": [...]` after them when some statement runs outside a transaction. Without
/// the field, as written before it existed, every statement is transactional. Reading refuses a
/// `non_transactional` that is not strictly ascending or names a statement the plan does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PlanWire")]
pub struct Plan {
    pub statements: Vec<String>,
    /// The index of the last statement that must be committed before the statements after it run
    /// (see the module doc); `None` when the whole plan can run in one transaction.
    pub commit_after: Option<usize>,
    /// The indexes (ascending) of the statements that cannot run in a transaction and run outside
    /// one, each as a unit of its own (`CREATE INDEX CONCURRENTLY`; see the module doc).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_transactional: Vec<usize>,
}

/// What [`Plan`] is read from, before its `non_transactional` is checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanWire {
    statements: Vec<String>,
    commit_after: Option<usize>,
    #[serde(default)]
    non_transactional: Vec<usize>,
}

impl TryFrom<PlanWire> for Plan {
    type Error = String;

    fn try_from(w: PlanWire) -> Result<Plan, String> {
        let PlanWire { statements, commit_after, non_transactional } = w;
        if let Some(i) = non_transactional.iter().find(|&&i| i >= statements.len()) {
            return Err(format!("non_transactional: statement {i} does not exist (the plan has {})", statements.len()));
        }
        if let Some(pair) = non_transactional.windows(2).find(|p| p[0] >= p[1]) {
            return Err(format!(
                "non_transactional: expected strictly ascending statement indexes, but {} is followed by {}",
                pair[0], pair[1]
            ));
        }
        Ok(Plan { statements, commit_after, non_transactional })
    }
}

/// A run of a plan's statements ([`Plan::units`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The statements, by index in [`Plan::statements`].
    pub statements: std::ops::Range<usize>,
    /// Whether they run in one transaction; if not, the unit is one statement and it commits by
    /// itself.
    pub transactional: bool,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// The plan as a SQL script: one statement per entry, each ending in `;`, with
    /// [`COMMIT_MARKER`] at the commit point, and each non-transactional statement preceded by
    /// [`NO_TRANSACTION_MARKER`].
    pub fn to_sql(&self) -> String {
        let mut out = String::new();
        for (i, statement) in self.statements.iter().enumerate() {
            if i > 0 && self.commit_after == Some(i - 1) {
                out.push_str(COMMIT_MARKER);
            }
            if self.is_non_transactional(i) {
                out.push_str(NO_TRANSACTION_MARKER);
            }
            out.push_str(statement);
            out.push_str(";\n");
        }
        out
    }

    fn is_non_transactional(&self, i: usize) -> bool {
        self.non_transactional.contains(&i)
    }

    /// The plan as the units that run in order, each committed before the next one starts: the
    /// stretches between commit points and non-transactional statements, and each of those
    /// statements. Empty for an empty plan.
    pub fn units(&self) -> Vec<Unit> {
        let n = self.statements.len();
        let mut out = Vec::new();
        let mut start = 0;
        let end_transaction = |out: &mut Vec<Unit>, start: &mut usize, end: usize| {
            if end > *start {
                out.push(Unit { statements: *start..end, transactional: true });
            }
            *start = end;
        };
        for i in 0..n {
            if self.is_non_transactional(i) {
                end_transaction(&mut out, &mut start, i);
                out.push(Unit { statements: i..i + 1, transactional: false });
                start = i + 1;
            } else if self.commit_after == Some(i) {
                end_transaction(&mut out, &mut start, i + 1);
            }
        }
        end_transaction(&mut out, &mut start, n);
        out
    }

    /// The SQL script of `unit`: its statements, each ending in `;`.
    pub fn unit_sql(&self, unit: &Unit) -> String {
        self.statements[unit.statements.clone()].iter().map(|s| format!("{s};\n")).collect()
    }
}

/// Marks [`Plan::commit_after`] in [`Plan::to_sql`]: a comment for the reader, then the
/// `-- pgmig:commit` directive. `migrate apply` keys on the directive, never on the comment.
pub const COMMIT_MARKER: &str = "-- New enum values cannot be used in the transaction that adds them.\n-- pgmig:commit\n";

/// Precedes a statement of [`Plan::non_transactional`] in [`Plan::to_sql`]: a comment for the
/// reader, then the `-- pgmig:notx` directive on the statement. An edited plan keeps the
/// statement outside a transaction as long as the directive stays above it.
pub const NO_TRANSACTION_MARKER: &str = "-- Runs outside a transaction: Postgres refuses it inside one.\n-- pgmig:notx\n";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Statements that each need the next one to run first (and the last needs the first).
    #[error("the changes depend on each other in a cycle: {}", .0.join("; "))]
    Cycle(Vec<String>),
    /// Changes that cannot be planned, one message each.
    #[error("unsupported changes: {}", .0.join("; "))]
    Unsupported(Vec<String>),
}

/// How a plan prints object names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Objects in this schema are printed unqualified (`CREATE TABLE t`, not `public.t`): the plan
    /// is meant to run with the schema first on the `search_path`. Names in other schemas, and
    /// SQL the server printed (types, defaults, constraint definitions), are left as they are:
    /// inspection prints those schema-qualified (`'a'::public.mood`, `REFERENCES public.b(id)`).
    pub unqualified_schema: Option<String>,
    /// Create and drop indexes `CONCURRENTLY`, each such statement in a unit outside a transaction,
    /// and add primary keys and unique constraints by building their index that way and attaching
    /// it with `USING INDEX` (see the module doc). Absent from older JSON: false.
    #[serde(default)]
    pub concurrent_indexes: bool,
    /// `IF NOT EXISTS` on the creating statements Postgres has it for (`CREATE SCHEMA`, `CREATE
    /// TABLE`, `CREATE SEQUENCE`, `CREATE INDEX`, `ADD COLUMN`) and `IF EXISTS` on dropping ones
    /// (`DROP SCHEMA`/`TABLE`/`SEQUENCE`/`INDEX`/`TYPE`/`DOMAIN`, `DROP COLUMN`, `DROP CONSTRAINT`,
    /// `DROP ATTRIBUTE`), so a plan can run again over a database it was partly applied to.
    /// Postgres has no `IF NOT EXISTS` for `CREATE TYPE`, `CREATE DOMAIN` or `ADD CONSTRAINT`:
    /// those stay as they are. Absent from older JSON: false.
    #[serde(default)]
    pub if_exists: bool,
    /// `CASCADE` on `DROP SCHEMA`/`TABLE`/`SEQUENCE`/`INDEX`/`TYPE`/`DOMAIN` and on `DROP COLUMN`
    /// and `DROP CONSTRAINT`: Postgres then also drops what depends on the object (views,
    /// functions, objects outside the diff's scope) without listing it. Not on `DROP INDEX
    /// CONCURRENTLY`, which has no `CASCADE`. Absent from older JSON: false.
    #[serde(default)]
    pub cascade: bool,
}

impl PlanOptions {
    /// `name` as statements print it.
    pub fn name(&self, name: &QualName) -> String {
        match &self.unqualified_schema {
            Some(s) if *s == name.schema => quote_ident(&name.name),
            _ => name.to_string(),
        }
    }
}

/// [`plan_with`] with qualified names.
pub fn plan(changes: &[Change]) -> Result<Plan, Error> {
    plan_with(changes, &PlanOptions::default())
}

pub fn plan_with(changes: &[Change], opts: &PlanOptions) -> Result<Plan, Error> {
    let unsupported: Vec<String> = changes.iter().filter_map(unsupported).collect();
    if !unsupported.is_empty() {
        return Err(Error::Unsupported(unsupported));
    }
    let whole: BTreeSet<&QualName> = changes
        .iter()
        .filter_map(|c| match c {
            Change::CreateTable(t) | Change::DropTable { table: t, .. } => Some(&t.name),
            _ => None,
        })
        .collect();
    // Tables that lose a primary key or unique constraint in the plan: a key added there keeps the
    // plain `ADD CONSTRAINT`, so the old key is replaced in one transaction instead of the table
    // going without it (and without foreign keys to it) while a new index builds.
    let rekeyed: BTreeSet<&QualName> = changes
        .iter()
        .filter_map(|c| match c {
            Change::DropConstraint { table, constraint }
                if matches!(constraint.kind, ConstraintKind::PrimaryKey | ConstraintKind::Unique) =>
            {
                Some(table)
            }
            _ => None,
        })
        .collect();
    let tables = Tables { whole, rekeyed, partitions: Partitions::of(changes) };
    // Hinted renames run first, whatever else is planned: every other change names the objects as
    // they are called once the renames are made (see [`pgmig_diff::rename`]).
    let mut renames: Vec<&Change> = changes.iter().filter(|c| rename_rank(c).is_some()).collect();
    renames.sort_by_key(|c| rename_rank(c));
    let (adds, rest): (Vec<&Change>, Vec<&Change>) =
        changes.iter().filter(|c| rename_rank(c).is_none()).partition(|c| matches!(c, Change::AddEnumValue { .. }));
    let mut plan = Plan::default();
    plan.statements.extend(renames.iter().map(|c| statement(c, opts)));
    let renamed = plan.statements.len();
    for step in sorted(&adds, opts, &tables)?.into_iter().flatten() {
        plan.statements.push(step.sql);
    }
    plan.commit_after = (plan.statements.len() > renamed && !rest.is_empty()).then(|| plan.statements.len() - 1);
    for step in sorted(&rest, opts, &tables)?.into_iter().flatten() {
        if step.non_transactional {
            plan.non_transactional.push(plan.statements.len());
        }
        plan.statements.push(step.sql);
    }
    Ok(plan)
}

/// Where a hinted rename runs among the renames: types and tables first, then the columns and
/// indexes, which are named by their table's new name. `None` for a change that is not one.
fn rename_rank(change: &Change) -> Option<u8> {
    match change {
        Change::RenameEnum { .. }
        | Change::RenameDomain { .. }
        | Change::RenameCompositeType { .. }
        | Change::RenameSequence { .. }
        | Change::RenameTable { .. } => Some(0),
        Change::RenameColumn { .. } => Some(1),
        Change::RenameIndex { .. } => Some(2),
        _ => None,
    }
}

/// What the plan does to whole tables, which decides where the concurrent-index policy applies.
struct Tables<'a> {
    /// Tables the plan creates or drops: their indexes are not built or dropped concurrently.
    whole: BTreeSet<&'a QualName>,
    /// Tables that lose a primary key or unique constraint (see [`plan_with`]).
    rekeyed: BTreeSet<&'a QualName>,
    partitions: Partitions<'a>,
}

/// The partitions a plan detaches and attaches, which order the changes to their parents (see
/// [`graph::Object::Inherited`]).
pub(crate) struct Partitions<'a> {
    /// Partitioned tables a partition is detached from or attached to.
    parents: BTreeSet<&'a QualName>,
    /// Partitions detached and attached again with a new bound.
    rebound: BTreeSet<&'a QualName>,
}

impl<'a> Partitions<'a> {
    pub(crate) fn of(changes: &'a [Change]) -> Partitions<'a> {
        let (mut detached, mut attached, mut parents) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for c in changes {
            match c {
                Change::DetachPartition { table, parent, .. } => {
                    detached.insert(table);
                    parents.insert(parent);
                }
                Change::AttachPartition { table, partition_of, .. } => {
                    attached.insert(table);
                    parents.insert(&partition_of.parent);
                }
                _ => {}
            }
        }
        let rebound = detached.intersection(&attached).copied().collect();
        Partitions { parents, rebound }
    }
}

/// A planned statement.
struct Step {
    sql: String,
    /// It runs outside a transaction.
    non_transactional: bool,
}

/// `change` as steps: one statement, or with the policy, two for a key built concurrently (see
/// [`using_index`]).
fn steps(change: &Change, opts: &PlanOptions, tables: &Tables) -> Vec<Step> {
    let whole = &tables.whole;
    if opts.concurrent_indexes
        && let Some(pair) = using_index(change, opts, tables)
    {
        return Vec::from(pair);
    }
    let sql = statement(change, opts);
    if opts.concurrent_indexes
        && let Some(sql) = concurrently(change, &sql, whole, opts)
    {
        return vec![Step { sql, non_transactional: true }];
    }
    vec![Step { sql, non_transactional: false }]
}

/// The `CONCURRENTLY` form of `sql`, the statement for `change`, when the policy applies to it
/// (see the module doc).
fn concurrently(change: &Change, sql: &str, whole: &BTreeSet<&QualName>, opts: &PlanOptions) -> Option<String> {
    match change {
        Change::CreateIndex { table, index } if !index.on_partitioned_table && !whole.contains(table) => {
            concurrent_create(sql, opts.if_exists)
        }
        Change::DropIndex { table, index } if !index.on_partitioned_table && !whole.contains(table) => {
            // `DROP INDEX CONCURRENTLY` has no `CASCADE`.
            let sql = sql.strip_suffix(" CASCADE").unwrap_or(sql);
            sql.strip_prefix("DROP INDEX ").map(|rest| format!("DROP INDEX CONCURRENTLY {rest}"))
        }
        _ => None,
    }
}

/// `CREATE [UNIQUE] INDEX CONCURRENTLY ...` for a `CREATE [UNIQUE] INDEX ...` statement in the form
/// the server prints; `None` for one that is not in that form.
fn concurrent_create(sql: &str, if_not_exists: bool) -> Option<String> {
    let rest = sql.strip_prefix("CREATE UNIQUE INDEX ").map(|r| ("CREATE UNIQUE INDEX", r));
    let (head, rest) = rest.or_else(|| sql.strip_prefix("CREATE INDEX ").map(|r| ("CREATE INDEX", r)))?;
    // `sql` may already carry `IF NOT EXISTS` (see `create_index_if_not_exists`).
    let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
    let ine = if if_not_exists { " IF NOT EXISTS" } else { "" };
    Some(format!("{head} CONCURRENTLY{ine} {rest}"))
}

/// A primary key or unique constraint added to a table that exists before the plan, as two steps:
/// its index built `CONCURRENTLY` under the constraint's name, outside a transaction, then `ADD
/// CONSTRAINT ... USING INDEX` (see the module doc). `None` when the constraint keeps the plain
/// `ADD CONSTRAINT`: another kind, a table the plan creates or drops, a table that loses a primary
/// key or unique constraint in the plan (the replacement stays in one transaction), a key in a
/// tablespace (`USING INDEX TABLESPACE`), a partitioned table, or an
/// index `USING INDEX` cannot take (not a b-tree, as a key `WITHOUT OVERLAPS` is) or not in the form
/// the server prints.
fn using_index(change: &Change, opts: &PlanOptions, tables: &Tables) -> Option<[Step; 2]> {
    let Change::AddConstraint { table, constraint: c } = change else { return None };
    let kind = match c.kind {
        ConstraintKind::PrimaryKey => "PRIMARY KEY",
        ConstraintKind::Unique => "UNIQUE",
        ConstraintKind::Check | ConstraintKind::ForeignKey | ConstraintKind::Exclusion => return None,
    };
    if c.on_partitioned_table || tables.whole.contains(table) || tables.rekeyed.contains(table) {
        return None;
    }
    // `pg_get_indexdef` leaves out the tablespace a key's index is in, so the index built from it
    // would not match the constraint.
    if c.definition.contains(" USING INDEX TABLESPACE ") {
        return None;
    }
    let index = c.index_definition.as_deref()?;
    let name = quote_ident(&c.name);
    // The index carries the constraint's name, so the constraint that adopts it keeps it.
    let on = index.strip_prefix("CREATE UNIQUE INDEX ")?.strip_prefix(name.as_str())?.strip_prefix(" ON ")?;
    if !on.contains(" USING btree (") {
        return None;
    }
    let timing = match (c.deferrable, c.deferred) {
        (false, _) => "",
        (true, false) => " DEFERRABLE",
        (true, true) => " DEFERRABLE INITIALLY DEFERRED",
    };
    Some([
        Step { sql: concurrent_create(index, opts.if_exists)?, non_transactional: true },
        Step {
            sql: format!("ALTER TABLE {} ADD CONSTRAINT {name} {kind} USING INDEX {name}{timing}", opts.name(table)),
            non_transactional: false,
        },
    ])
}

fn sorted(changes: &[&Change], opts: &PlanOptions, tables: &Tables) -> Result<Vec<Vec<Step>>, Error> {
    let nodes: Vec<Node> =
        changes.iter().map(|c| Node { phase: phase(c, &tables.partitions), deps: deps(c, &tables.partitions) }).collect();
    match graph::sort(&nodes) {
        Ok(order) => Ok(order.into_iter().map(|i| steps(changes[i], opts, tables)).collect()),
        Err(graph::Cycle(steps)) => Err(Error::Cycle(steps.into_iter().map(|i| statement(changes[i], opts)).collect())),
    }
}

/// Whether `change` is one the planner refuses ([`Error::Unsupported`]): it has no statement.
pub(crate) fn is_unsupported(change: &Change) -> bool {
    unsupported(change).is_some()
}

fn unsupported(change: &Change) -> Option<String> {
    match change {
        Change::UnsupportedEnumChange { name, from, to } => {
            let list = |labels: &[String]| labels.iter().map(|l| quote_literal(l)).collect::<Vec<_>>().join(", ");
            Some(format!(
                "enum {name}: values ({}) cannot become ({}); only adding values and renaming one value in place are supported, not removing or reordering values",
                list(from),
                list(to)
            ))
        }
        Change::UnsupportedDomainChange { name, from, to, used_by } => Some(format!(
            "domain {name}: the base type cannot change from {from} to {to}; Postgres cannot change it in place, and the domain cannot be dropped and created again while {} {} it",
            used_by.join(", "),
            if used_by.len() == 1 { "uses" } else { "use" }
        )),
        Change::UnsupportedCompositeChange { name, attribute, from, to, used_by } => Some(format!(
            "composite type {name}: attribute {} cannot change type from {from} to {to} while {} {} the type",
            quote_ident(attribute),
            used_by.join(", "),
            if used_by.len() == 1 { "uses" } else { "use" }
        )),
        Change::UnsupportedPartitionChange { name, detail } => Some(format!("table {name}: {detail}")),
        _ => None,
    }
}

/// The tie-break between statements the graph does not order.
fn phase(change: &Change, partitions: &Partitions) -> u8 {
    match change {
        // After the changes to its parent, which reach it while it is attached.
        Change::DetachPartition { table, .. } if partitions.rebound.contains(table) => 12,
        // Planned on their own, before everything else (see the module doc).
        Change::AddEnumValue { .. } | Change::UnsupportedEnumChange { .. } => 0,
        Change::UnsupportedDomainChange { .. } | Change::UnsupportedCompositeChange { .. } | Change::UnsupportedPartitionChange { .. } => 0,
        Change::CreateSchema(_) => 0,
        // Never sorted: they run before everything else (see `plan_with`).
        Change::RenameTable { .. }
        | Change::RenameColumn { .. }
        | Change::RenameEnum { .. }
        | Change::RenameSequence { .. }
        | Change::RenameDomain { .. }
        | Change::RenameCompositeType { .. }
        | Change::RenameIndex { .. } => 0,
        Change::CreateEnum(_) | Change::CreateSequence(_) | Change::CreateDomain(_) | Change::CreateCompositeType(_) => 1,
        // Before defaults that print the new name are set.
        Change::RenameEnumValue { .. } | Change::AlterSequence { .. } => 2,
        // Before the changes to the table standing alone, and to its parent, which would otherwise
        // reach it.
        Change::DetachPartition { .. } => 2,
        Change::CreateTable(_) => 3,
        Change::AddColumn { .. } | Change::AddAttribute { .. } => 4,
        // A column cannot have a default and an identity at once. A constraint goes before the
        // columns it uses change (a primary key column cannot drop NOT NULL).
        Change::DropIdentity { .. }
        | Change::DropDefault { .. }
        | Change::DropConstraint { .. }
        | Change::DropIndex { .. }
        | Change::DropDomainDefault { .. }
        | Change::DropDomainConstraint { .. } => 5,
        Change::AlterColumnType { .. } | Change::AlterAttributeType { .. } => 6,
        Change::SetDefault { .. } | Change::SetDomainDefault { .. } => 7,
        Change::DropNotNull { .. } | Change::SetNotNull { .. } | Change::DropDomainNotNull { .. } | Change::SetDomainNotNull { .. } => 8,
        // After the type, the dropped default and NOT NULL that adding an identity needs.
        Change::AddIdentity { .. } | Change::AlterIdentity { .. } => 9,
        // After the columns they use have settled.
        Change::SetSequenceOwner { .. }
        | Change::AddConstraint { .. }
        | Change::RenameConstraint { .. }
        | Change::AlterConstraint { .. }
        | Change::CreateIndex { .. }
        | Change::AddDomainConstraint { .. }
        | Change::ValidateDomainConstraint { .. } => 10,
        Change::DropColumn { .. } | Change::DropAttribute { .. } => 11,
        // After the table's columns match the parent's.
        Change::DropTable { .. } | Change::AttachPartition { .. } => 12,
        Change::DropSequence(_) => 13,
        Change::DropEnum(_) | Change::DropDomain(_) | Change::DropCompositeType(_) => 14,
        Change::DropSchema(_) => 15,
        // After the object it describes exists and has settled.
        Change::SetComment { .. } => 16,
    }
}

fn deps(change: &Change, partitions: &Partitions) -> Deps {
    let mut d = partition_deps(change, partitions).unwrap_or_else(|| object_deps(change));
    if let Some(t) = inherited_by_partitions(change)
        && partitions.parents.contains(t)
    {
        d.alters.push(Object::Inherited(t.clone()));
    }
    // Tables, sequences and indexes share the relation namespace (see [`Object::Relation`]).
    let relations = |objects: &[Object]| -> Vec<Object> {
        objects
            .iter()
            .filter_map(|o| match o {
                Object::Table(q) | Object::Sequence(q) | Object::Index(q) => Some(Object::Relation(q.clone())),
                _ => None,
            })
            .collect()
    };
    let (created, dropped) = (relations(&d.creates), relations(&d.drops));
    d.creates.extend(created);
    d.drops.extend(dropped);
    d
}

/// The table whose columns, constraints or indexes `change` changes, which its partitions have
/// from it too ([`Object::Inherited`]).
fn inherited_by_partitions(change: &Change) -> Option<&QualName> {
    match change {
        Change::AddColumn { table, .. }
        | Change::DropColumn { table, .. }
        | Change::AlterColumnType { table, .. }
        | Change::SetDefault { table, .. }
        | Change::DropDefault { table, .. }
        | Change::SetNotNull { table, .. }
        | Change::DropNotNull { table, .. }
        | Change::AddIdentity { table, .. }
        | Change::AlterIdentity { table, .. }
        | Change::DropIdentity { table, .. }
        | Change::AddConstraint { table, .. }
        | Change::DropConstraint { table, .. }
        | Change::RenameConstraint { table, .. }
        | Change::AlterConstraint { table, .. }
        | Change::CreateIndex { table, .. }
        | Change::DropIndex { table, .. } => Some(table),
        _ => None,
    }
}

/// A partition's detach or attach. A detach that leaves the table standing alone runs before the
/// changes to its parent (which would otherwise reach it); a re-bound partition's detach after
/// them (they reach it while it is attached), with its attach after the detach; an attach after
/// the changes to the parent and to the table that make its columns and checks the parent's.
fn partition_deps(change: &Change, partitions: &Partitions) -> Option<Deps> {
    let column = |t: &QualName, c: &str| Object::Column(t.clone(), c.to_owned());
    let of = |t: &QualName, columns: &[String]| columns.iter().map(|c| column(t, c)).collect::<Vec<_>>();
    match change {
        Change::DetachPartition { table, parent, .. } if partitions.rebound.contains(table) => Some(Deps {
            drops: vec![Object::Partition(table.clone())],
            alters: vec![Object::Inherited(parent.clone())],
            requires: vec![Object::Table(table.clone()), Object::Table(parent.clone())],
            releases: vec![Object::Table(parent.clone())],
            ..Deps::default()
        }),
        Change::DetachPartition { table, parent, columns } => Some(Deps {
            drops: vec![Object::Partition(table.clone())],
            // Before the table standing alone changes, and before the parent's columns change or go.
            alters: [of(table, columns), of(parent, columns), vec![Object::Inherited(parent.clone())]].concat(),
            requires: vec![Object::Table(table.clone()), Object::Table(parent.clone())],
            releases: [vec![Object::Table(parent.clone())], of(parent, columns)].concat(),
            ..Deps::default()
        }),
        Change::AttachPartition { table, partition_of, columns } => {
            let parent = &partition_of.parent;
            // A re-bound partition's attach follows its detach (`Object::Partition`), which follows
            // the changes to the parent.
            let inherited = if partitions.rebound.contains(table) { vec![] } else { vec![Object::Inherited(parent.clone())] };
            Some(Deps {
                creates: vec![Object::Partition(table.clone())],
                // After the table's columns are added and set as the parent's, and its checks added.
                alters: [of(table, columns), inherited].concat(),
                requires: [vec![Object::Table(table.clone()), Object::Table(parent.clone())], of(table, columns), of(parent, columns)]
                    .concat(),
                ..Deps::default()
            })
        }
        _ => None,
    }
}

fn object_deps(change: &Change) -> Deps {
    let column = |table: &QualName, column: &str| Object::Column(table.clone(), column.to_owned());
    let in_table = |table: &QualName| Object::Table(table.clone());
    match change {
        Change::CreateSchema(name) => Deps { creates: vec![Object::Schema(name.clone())], ..Deps::default() },
        Change::DropSchema(name) => Deps { drops: vec![Object::Schema(name.clone())], ..Deps::default() },
        Change::CreateTable(t) => {
            let mut requires = vec![Object::Schema(t.name.schema.clone())];
            requires.extend(types(t.columns.iter().map(|c| &c.type_ref)));
            requires.extend(sequences(t.columns.iter().flat_map(|c| &c.default_sequences)));
            requires.extend(new_values(t.columns.iter().map(|c| &c.type_ref)));
            let mut creates = vec![Object::Table(t.name.clone()), Object::Type(t.name.clone())];
            creates.extend(t.columns.iter().filter_map(|c| Some(identity_sequence(&t.name, &c.name, c.identity.as_ref()?))));
            creates.extend(t.constraints.iter().map(|c| constraint(&t.name, &c.name)));
            creates.extend(table_indexes(t));
            if let Some(p) = &t.partition_of {
                requires.push(in_table(&p.parent));
                creates.push(Object::Partition(t.name.clone()));
            }
            Deps { creates, requires, ..Deps::default() }
        }
        Change::DropTable { table: t, owned_sequences } => {
            let mut releases = vec![Object::Schema(t.name.schema.clone())];
            releases.extend(types(t.columns.iter().map(|c| &c.type_ref)));
            releases.extend(sequences(t.columns.iter().flat_map(|c| &c.default_sequences)));
            releases.extend(old_values(t.columns.iter().map(|c| &c.type_ref)));
            // With the table go the sequences its columns own and its identity sequences.
            let mut drops = vec![Object::Table(t.name.clone()), Object::Type(t.name.clone())];
            drops.extend(sequences(owned_sequences));
            // With the table go its constraints, and its foreign keys stop using what they reference.
            drops.extend(t.constraints.iter().map(|c| constraint(&t.name, &c.name)));
            drops.extend(table_indexes(t));
            releases.extend(t.constraints.iter().flat_map(referenced).filter(|o| !is_of_table(o, &t.name)));
            // A partition is dropped before its parent.
            if let Some(p) = &t.partition_of {
                releases.push(in_table(&p.parent));
                drops.push(Object::Partition(t.name.clone()));
            }
            Deps { drops, releases, ..Deps::default() }
        }
        Change::DetachPartition { .. } | Change::AttachPartition { .. } => Deps::default(),
        Change::CreateSequence(s) => Deps {
            creates: vec![Object::Sequence(s.name.clone())],
            requires: vec![Object::Schema(s.name.schema.clone())],
            ..Deps::default()
        },
        Change::DropSequence(s) => {
            Deps { drops: vec![Object::Sequence(s.name.clone())], releases: vec![Object::Schema(s.name.schema.clone())], ..Deps::default() }
        }
        Change::AlterSequence { to, .. } => Deps { alters: vec![Object::Sequence(to.name.clone())], ..Deps::default() },
        Change::SetSequenceOwner { name, owned_by, old } => {
            let owner = |o: &Option<OwnedBy>| match o {
                Some(o) => vec![in_table(&o.table), column(&o.table, &o.column)],
                None => Vec::new(),
            };
            Deps {
                alters: vec![Object::Sequence(name.clone())],
                requires: [vec![Object::Sequence(name.clone())], owner(owned_by)].concat(),
                releases: owner(old),
                ..Deps::default()
            }
        }
        Change::AddIdentity { table, column: c, identity, .. } => Deps {
            // The implicit sequence takes the name a dropped sequence may still hold.
            creates: vec![identity_sequence(table, c, identity)],
            alters: vec![column(table, c)],
            requires: vec![in_table(table)],
            ..Deps::default()
        },
        Change::DropIdentity { table, column: c, sequence } => Deps {
            drops: vec![Object::Sequence(sequence.clone())],
            alters: vec![column(table, c)],
            requires: vec![in_table(table)],
            ..Deps::default()
        },
        Change::AlterIdentity { table, column: c, .. } => {
            Deps { alters: vec![column(table, c)], requires: vec![in_table(table)], ..Deps::default() }
        }
        Change::CreateEnum(e) => {
            Deps { creates: vec![Object::Type(e.name.clone())], requires: vec![Object::Schema(e.name.schema.clone())], ..Deps::default() }
        }
        Change::DropEnum(e) => {
            Deps { drops: vec![Object::Type(e.name.clone())], releases: vec![Object::Schema(e.name.schema.clone())], ..Deps::default() }
        }
        Change::AddEnumValue { name, .. } | Change::RenameEnumValue { name, .. } => {
            Deps { alters: vec![Object::Type(name.clone())], ..Deps::default() }
        }
        Change::UnsupportedEnumChange { .. }
        | Change::UnsupportedDomainChange { .. }
        | Change::UnsupportedCompositeChange { .. }
        | Change::UnsupportedPartitionChange { .. } => Deps::default(),
        Change::CreateDomain(d) => {
            let mut creates = vec![Object::Type(d.name.clone())];
            creates.extend(d.constraints.iter().map(|c| constraint(&d.name, &c.name)));
            let mut requires = vec![Object::Schema(d.name.schema.clone())];
            requires.extend(types([&d.base_ref]));
            requires.extend(new_values([&d.base_ref]));
            Deps { creates, requires, ..Deps::default() }
        }
        Change::DropDomain(d) => {
            let mut drops = vec![Object::Type(d.name.clone())];
            drops.extend(d.constraints.iter().map(|c| constraint(&d.name, &c.name)));
            let mut releases = vec![Object::Schema(d.name.schema.clone())];
            releases.extend(types([&d.base_ref]));
            releases.extend(old_values([&d.base_ref]));
            Deps { drops, releases, ..Deps::default() }
        }
        Change::SetDomainDefault { name, .. }
        | Change::DropDomainDefault { name }
        | Change::SetDomainNotNull { name }
        | Change::DropDomainNotNull { name } => {
            Deps { alters: vec![Object::Type(name.clone())], requires: vec![Object::Type(name.clone())], ..Deps::default() }
        }
        Change::AddDomainConstraint { domain, constraint: c } => Deps {
            creates: vec![constraint(domain, &c.name)],
            alters: vec![Object::Type(domain.clone())],
            requires: vec![Object::Type(domain.clone())],
            ..Deps::default()
        },
        Change::DropDomainConstraint { domain, constraint: c } => {
            Deps { drops: vec![constraint(domain, &c.name)], alters: vec![Object::Type(domain.clone())], ..Deps::default() }
        }
        Change::ValidateDomainConstraint { domain, name } => Deps {
            alters: vec![Object::Type(domain.clone())],
            requires: vec![Object::Type(domain.clone()), constraint(domain, name)],
            ..Deps::default()
        },
        Change::CreateCompositeType(c) => {
            // A composite type has a relation of its name too (`pg_class.relkind = 'c'`).
            let mut creates = vec![Object::Type(c.name.clone()), Object::Relation(c.name.clone())];
            creates.extend(c.attributes.iter().map(|a| column(&c.name, &a.name)));
            let mut requires = vec![Object::Schema(c.name.schema.clone())];
            requires.extend(types(c.attributes.iter().map(|a| &a.type_ref)));
            requires.extend(new_values(c.attributes.iter().map(|a| &a.type_ref)));
            Deps { creates, requires, ..Deps::default() }
        }
        Change::DropCompositeType(c) => {
            let mut drops = vec![Object::Type(c.name.clone()), Object::Relation(c.name.clone())];
            drops.extend(c.attributes.iter().map(|a| column(&c.name, &a.name)));
            let mut releases = vec![Object::Schema(c.name.schema.clone())];
            releases.extend(types(c.attributes.iter().map(|a| &a.type_ref)));
            releases.extend(old_values(c.attributes.iter().map(|a| &a.type_ref)));
            Deps { drops, releases, ..Deps::default() }
        }
        Change::AddAttribute { composite, attribute } => {
            let mut requires = vec![Object::Type(composite.clone())];
            requires.extend(types([&attribute.type_ref]));
            requires.extend(new_values([&attribute.type_ref]));
            Deps {
                creates: vec![column(composite, &attribute.name)],
                alters: vec![Object::Type(composite.clone())],
                requires,
                ..Deps::default()
            }
        }
        Change::DropAttribute { composite, attribute, type_ref } => Deps {
            drops: vec![column(composite, attribute)],
            alters: vec![Object::Type(composite.clone())],
            requires: vec![Object::Type(composite.clone())],
            releases: [types([type_ref]), old_values([type_ref])].concat(),
            ..Deps::default()
        },
        Change::AlterAttributeType { composite, attribute, type_ref, old_type_ref, holders, .. } => {
            let mut requires = vec![Object::Type(composite.clone())];
            requires.extend(types([type_ref]));
            let mut releases = Vec::new();
            if type_ref != old_type_ref {
                requires.extend(new_values([type_ref]));
                releases.extend(types([old_type_ref]));
                releases.extend(old_values([old_type_ref]));
            }
            // Postgres refuses the change while a table column uses the composite type, directly
            // or through what holds it: the uses that end go first, the ones that start after.
            let held = std::iter::once(composite).chain(holders);
            Deps {
                creates: held.clone().map(|t| Object::NewValues(t.clone())).collect(),
                drops: held.map(|t| Object::OldValues(t.clone())).collect(),
                alters: vec![Object::Type(composite.clone()), column(composite, attribute)],
                requires,
                releases,
            }
        }
        Change::AddColumn { table, column: c } => {
            let mut requires = vec![in_table(table)];
            requires.extend(types([&c.type_ref]));
            requires.extend(sequences(&c.default_sequences));
            requires.extend(new_values([&c.type_ref]));
            let mut creates = vec![column(table, &c.name)];
            if let Some(identity) = &c.identity {
                creates.push(identity_sequence(table, &c.name, identity));
            }
            Deps { creates, requires, ..Deps::default() }
        }
        Change::DropColumn { table, column: c, type_ref, sequences: used, owned_sequences } => {
            let mut releases = types([type_ref]);
            releases.extend(old_values([type_ref]));
            releases.extend(sequences(used));
            // Postgres drops the sequences the column owns with it.
            let drops = [vec![column(table, c)], sequences(owned_sequences)].concat();
            Deps { drops, requires: vec![in_table(table)], releases, ..Deps::default() }
        }
        Change::AlterColumnType { table, column: c, type_ref, old_type_ref, .. } => {
            let mut requires = vec![in_table(table)];
            requires.extend(types([type_ref]));
            let mut releases = Vec::new();
            if type_ref != old_type_ref {
                requires.extend(new_values([type_ref]));
                releases.extend(types([old_type_ref]));
                releases.extend(old_values([old_type_ref]));
            }
            Deps { alters: vec![column(table, c), Object::ColumnType(table.clone(), c.clone())], requires, releases, ..Deps::default() }
        }
        Change::AddConstraint { table, constraint: c } => {
            let columns: Vec<Object> = c.columns.iter().map(|col| column(table, col)).collect();
            Deps {
                creates: [vec![constraint(table, &c.name)], constraint_index(table, c, &c.name)].concat(),
                // Added once its columns, and those a foreign key references, have their final type
                // and nullability.
                alters: [columns.clone(), referenced_column_types(c)].concat(),
                requires: [vec![in_table(table)], columns, referenced(c)].concat(),
                ..Deps::default()
            }
        }
        Change::DropConstraint { table, constraint: c } => {
            let columns: Vec<Object> = c.columns.iter().map(|col| column(table, col)).collect();
            Deps {
                drops: [vec![constraint(table, &c.name)], constraint_index(table, c, &c.name)].concat(),
                // Dropped before the columns it uses, or a foreign key references, change type.
                alters: [columns.clone(), referenced_column_types(c)].concat(),
                releases: [vec![in_table(table)], columns, referenced(c)].concat(),
                ..Deps::default()
            }
        }
        Change::RenameConstraint { table, from, to } => Deps {
            // The constraint's index is renamed with it.
            drops: [vec![constraint(table, from)], constraint_index(table, to, from)].concat(),
            creates: [vec![constraint(table, &to.name)], constraint_index(table, to, &to.name)].concat(),
            requires: vec![in_table(table)],
            ..Deps::default()
        },
        Change::AlterConstraint { table, to } => {
            Deps { alters: vec![constraint(table, &to.name)], requires: vec![in_table(table)], ..Deps::default() }
        }
        Change::CreateIndex { table, index } => {
            let columns = index.columns.iter().map(|col| column(table, col));
            Deps {
                creates: vec![index_object(table, &index.name)],
                requires: std::iter::once(in_table(table)).chain(columns).collect(),
                ..Deps::default()
            }
        }
        Change::DropIndex { table, index } => {
            let columns = index.columns.iter().map(|col| column(table, col));
            Deps {
                drops: vec![index_object(table, &index.name)],
                releases: std::iter::once(in_table(table)).chain(columns).collect(),
                ..Deps::default()
            }
        }
        // Not sorted (see `plan_with`); the policy uses these to leave out what needs a skipped
        // rename: whatever requires or alters the new name.
        Change::RenameTable { from, to } => {
            let new = QualName::new(from.schema.clone(), to.clone());
            Deps {
                creates: vec![Object::Table(new.clone()), Object::Type(new)],
                drops: vec![Object::Table(from.clone()), Object::Type(from.clone())],
                ..Deps::default()
            }
        }
        Change::RenameColumn { table, from, to } => {
            Deps { creates: vec![column(table, to)], drops: vec![column(table, from)], requires: vec![in_table(table)], ..Deps::default() }
        }
        Change::RenameEnum { from, to } | Change::RenameDomain { from, to } => Deps {
            creates: vec![Object::Type(QualName::new(from.schema.clone(), to.clone()))],
            drops: vec![Object::Type(from.clone())],
            ..Deps::default()
        },
        Change::RenameCompositeType { from, to } => {
            let new = QualName::new(from.schema.clone(), to.clone());
            Deps {
                creates: vec![Object::Type(new.clone()), Object::Relation(new)],
                drops: vec![Object::Type(from.clone()), Object::Relation(from.clone())],
                ..Deps::default()
            }
        }
        Change::RenameSequence { from, to } => Deps {
            creates: vec![Object::Sequence(QualName::new(from.schema.clone(), to.clone()))],
            drops: vec![Object::Sequence(from.clone())],
            ..Deps::default()
        },
        Change::RenameIndex { table, from, to } => Deps {
            creates: vec![index_object(table, to)],
            drops: vec![index_object(table, from)],
            requires: vec![in_table(table)],
            ..Deps::default()
        },
        Change::SetComment { target, .. } => {
            // A comment only waits for the object (and a column's table) to exist.
            let requires = match target {
                CommentTarget::Schema(s) => vec![Object::Schema(s.clone())],
                CommentTarget::Table(t) => vec![in_table(t)],
                CommentTarget::Column { table: t, column: c } => vec![in_table(t), column(t, c)],
                CommentTarget::Type(t) => vec![Object::Type(t.clone())],
                CommentTarget::Sequence(s) => vec![Object::Sequence(s.clone())],
                CommentTarget::Constraint { table: t, name: c } => vec![in_table(t), constraint(t, c)],
                CommentTarget::Index { table: t, name: i } => vec![in_table(t), index_object(t, i)],
                CommentTarget::DomainConstraint { domain: d, name: c } => vec![Object::Type(d.clone()), constraint(d, c)],
                CommentTarget::Attribute { composite: t, attribute: a } => vec![Object::Type(t.clone()), column(t, a)],
            };
            Deps { requires, ..Deps::default() }
        }
        Change::SetDefault { table, column: c, sequences: used, old_sequences, .. } => Deps {
            alters: vec![column(table, c)],
            requires: [vec![in_table(table)], sequences(used)].concat(),
            releases: sequences(old_sequences.iter().filter(|s| !used.contains(s))),
            ..Deps::default()
        },
        Change::DropDefault { table, column: c, sequences: used } => {
            Deps { alters: vec![column(table, c)], requires: vec![in_table(table)], releases: sequences(used), ..Deps::default() }
        }
        Change::SetNotNull { table, column: c } | Change::DropNotNull { table, column: c } => {
            Deps { alters: vec![column(table, c)], requires: vec![in_table(table)], ..Deps::default() }
        }
    }
}

/// A table's or domain's constraint (a domain and a table never share a name: the table's row type
/// has it).
fn constraint(table: &QualName, name: &str) -> Object {
    Object::Constraint(table.clone(), name.to_owned())
}

/// An index in `table`'s schema.
fn index_object(table: &QualName, name: &str) -> Object {
    Object::Index(QualName::new(table.schema.clone(), name))
}

/// The index a primary key, unique or exclusion constraint owns, under `name`; none for others.
fn constraint_index(table: &QualName, c: &Constraint, name: &str) -> Vec<Object> {
    match c.kind {
        ConstraintKind::PrimaryKey | ConstraintKind::Unique | ConstraintKind::Exclusion => vec![index_object(table, name)],
        ConstraintKind::Check | ConstraintKind::ForeignKey => Vec::new(),
    }
}

/// A table's indexes: its own and its constraints'.
fn table_indexes(t: &pgmig_model::Table) -> Vec<Object> {
    let mut out: Vec<Object> = t.constraints.iter().flat_map(|c| constraint_index(&t.name, c, &c.name)).collect();
    out.extend(t.indexes.iter().map(|i| index_object(&t.name, &i.name)));
    out
}

/// What a foreign key references: the table, its columns and the constraint or index whose index
/// it uses (by name, both). Empty for other constraints.
fn referenced(c: &Constraint) -> Vec<Object> {
    let Some(r) = &c.references else { return Vec::new() };
    let mut out = vec![Object::Table(r.table.clone())];
    out.extend(r.columns.iter().map(|col| Object::Column(r.table.clone(), col.clone())));
    out.extend(r.constraint.iter().map(|name| constraint(&r.table, name)));
    out.extend(r.constraint.iter().map(|name| index_object(&r.table, name)));
    out
}

/// The types of the columns a foreign key references: it is dropped before they change and added
/// after (see [`Object::ColumnType`]).
fn referenced_column_types(c: &Constraint) -> Vec<Object> {
    let Some(r) = &c.references else { return Vec::new() };
    r.columns.iter().map(|col| Object::ColumnType(r.table.clone(), col.clone())).collect()
}

/// Whether `o` is `table` or one of its columns or constraints.
fn is_of_table(o: &Object, table: &QualName) -> bool {
    match o {
        Object::Table(t) | Object::Column(t, _) | Object::Constraint(t, _) => t == table,
        _ => false,
    }
}

/// The user-defined types among column type references.
fn types<'a>(refs: impl IntoIterator<Item = &'a Option<QualName>>) -> Vec<Object> {
    refs.into_iter().flatten().map(|t| Object::Type(t.clone())).collect()
}

/// The values of the user-defined types among type references, as they were (see
/// [`Object::OldValues`]).
fn old_values<'a>(refs: impl IntoIterator<Item = &'a Option<QualName>>) -> Vec<Object> {
    refs.into_iter().flatten().map(|t| Object::OldValues(t.clone())).collect()
}

/// The values of the user-defined types among type references, as they become (see
/// [`Object::NewValues`]).
fn new_values<'a>(refs: impl IntoIterator<Item = &'a Option<QualName>>) -> Vec<Object> {
    refs.into_iter().flatten().map(|t| Object::NewValues(t.clone())).collect()
}

fn sequences<'a>(names: impl IntoIterator<Item = &'a QualName>) -> Vec<Object> {
    names.into_iter().map(|s| Object::Sequence(s.clone())).collect()
}

/// An identity column's sequence, by the name the catalog gave it ([`Identity::sequence_name`]).
/// Only used to order the identity against a drop or create of a sequence holding that name.
fn identity_sequence(table: &QualName, column: &str, identity: &Identity) -> Object {
    Object::Sequence(identity.sequence_name(table, column))
}

/// `CREATE SEQUENCE` / identity options that differ from Postgres' defaults for the type and
/// direction; every option when the type has no known defaults.
fn creation_options(data_type: &str, o: &SequenceOptions) -> Vec<String> {
    let Some(d) = SequenceOptions::defaults(data_type, o.increment) else {
        return option_clauses(None, o, true);
    };
    let mut out = Vec::new();
    if o.increment != 1 {
        out.push(format!("INCREMENT BY {}", o.increment));
    }
    if o.min != d.min {
        out.push(format!("MINVALUE {}", o.min));
    }
    if o.max != d.max {
        out.push(format!("MAXVALUE {}", o.max));
    }
    // Postgres starts at the minimum (ascending) or the maximum (descending) given.
    if o.start != if o.increment > 0 { o.min } else { o.max } {
        out.push(format!("START WITH {}", o.start));
    }
    if o.cache != d.cache {
        out.push(format!("CACHE {}", o.cache));
    }
    if o.cycle {
        out.push("CYCLE".to_owned());
    }
    out
}

/// The options that differ between `old` and `new` (all of them without `old`); `bounds` forces
/// `MINVALUE` and `MAXVALUE`.
fn option_clauses(old: Option<&SequenceOptions>, new: &SequenceOptions, bounds: bool) -> Vec<String> {
    let changed = |f: fn(&SequenceOptions) -> i64| old.is_none_or(|o| f(o) != f(new));
    let mut out = Vec::new();
    if changed(|o| o.increment) {
        out.push(format!("INCREMENT BY {}", new.increment));
    }
    if bounds || changed(|o| o.min) {
        out.push(format!("MINVALUE {}", new.min));
    }
    if bounds || changed(|o| o.max) {
        out.push(format!("MAXVALUE {}", new.max));
    }
    if changed(|o| o.start) {
        out.push(format!("START WITH {}", new.start));
    }
    if changed(|o| o.cache) {
        out.push(format!("CACHE {}", new.cache));
    }
    if old.is_none_or(|o| o.cycle != new.cycle) {
        out.push(if new.cycle { "CYCLE" } else { "NO CYCLE" }.to_owned());
    }
    out
}

fn generated(g: Generated) -> &'static str {
    match g {
        Generated::Always => "ALWAYS",
        Generated::ByDefault => "BY DEFAULT",
    }
}

/// `GENERATED ... AS IDENTITY [(options)]` for a column of `type_name`.
fn identity_clause(type_name: &str, identity: &Identity) -> String {
    let options = creation_options(type_name, &identity.options);
    let mut s = format!("GENERATED {} AS IDENTITY", generated(identity.generated));
    if !options.is_empty() {
        s.push_str(&format!(" ({})", options.join(" ")));
    }
    s
}

/// `sql`, a `CREATE [UNIQUE] INDEX name ...` statement as the server prints it, with `IF NOT EXISTS`
/// after the keywords when the policy asks for it (and the statement is in that form).
fn create_index_if_not_exists(sql: &str, opts: &PlanOptions) -> String {
    if !opts.if_exists {
        return sql.to_owned();
    }
    for head in ["CREATE UNIQUE INDEX ", "CREATE INDEX "] {
        if let Some(rest) = sql.strip_prefix(head) {
            return format!("{head}IF NOT EXISTS {rest}");
        }
    }
    sql.to_owned()
}

fn statement(change: &Change, opts: &PlanOptions) -> String {
    let n = |q: &QualName| opts.name(q);
    let ine = if opts.if_exists { " IF NOT EXISTS" } else { "" };
    let ie = if opts.if_exists { " IF EXISTS" } else { "" };
    let cascade = if opts.cascade { " CASCADE" } else { "" };
    let drop = |kind: &str, name: String| format!("DROP {kind}{ie} {name}{cascade}");
    match change {
        Change::CreateSchema(name) => format!("CREATE SCHEMA{ine} {}", quote_ident(name)),
        Change::DropSchema(name) => drop("SCHEMA", quote_ident(name)),
        Change::CreateTable(t) => {
            let mut cols: Vec<String> = t.columns.iter().map(|c| format!("    {}", column_def(c))).collect();
            cols.extend(t.constraints.iter().map(|c| format!("    {}", constraint_def(c))));
            let body = if cols.is_empty() { String::new() } else { format!("(\n{}\n)", cols.join(",\n")) };
            // A partition has its parent's columns: only its own constraints are written.
            let mut s = match &t.partition_of {
                Some(p) if body.is_empty() => format!("CREATE TABLE{ine} {} PARTITION OF {} {}", n(&t.name), n(&p.parent), p.bound),
                Some(p) => format!("CREATE TABLE{ine} {} PARTITION OF {} {body} {}", n(&t.name), n(&p.parent), p.bound),
                None if body.is_empty() => format!("CREATE TABLE{ine} {} ()", n(&t.name)),
                None => format!("CREATE TABLE{ine} {} {body}", n(&t.name)),
            };
            if let Some(key) = &t.partition_key {
                s.push_str(&format!(" PARTITION BY {key}"));
            }
            s
        }
        Change::DropTable { table: t, .. } => drop("TABLE", n(&t.name)),
        Change::DetachPartition { table, parent, .. } => format!("ALTER TABLE {} DETACH PARTITION {}", n(parent), n(table)),
        Change::AttachPartition { table, partition_of: p, .. } => {
            format!("ALTER TABLE {} ATTACH PARTITION {} {}", n(&p.parent), n(table), p.bound)
        }
        Change::UnsupportedPartitionChange { name, .. } => format!("-- unsupported change to the partitioning of table {}", n(name)),
        Change::CreateEnum(e) => {
            let labels: Vec<String> = e.labels.iter().map(|l| quote_literal(l)).collect();
            format!("CREATE TYPE {} AS ENUM ({})", n(&e.name), labels.join(", "))
        }
        Change::DropEnum(e) => drop("TYPE", n(&e.name)),
        Change::RenameTable { from, to } => format!("ALTER TABLE {} RENAME TO {}", n(from), quote_ident(to)),
        Change::RenameColumn { table, from, to } => {
            format!("ALTER TABLE {} RENAME COLUMN {} TO {}", n(table), quote_ident(from), quote_ident(to))
        }
        Change::RenameEnum { from, to } | Change::RenameCompositeType { from, to } => {
            format!("ALTER TYPE {} RENAME TO {}", n(from), quote_ident(to))
        }
        Change::RenameDomain { from, to } => format!("ALTER DOMAIN {} RENAME TO {}", n(from), quote_ident(to)),
        Change::RenameSequence { from, to } => format!("ALTER SEQUENCE {} RENAME TO {}", n(from), quote_ident(to)),
        Change::RenameIndex { table, from, to } => {
            format!("ALTER INDEX {} RENAME TO {}", n(&QualName::new(table.schema.clone(), from.clone())), quote_ident(to))
        }
        Change::CreateSequence(seq) => {
            let mut s = format!("CREATE SEQUENCE{ine} {}", n(&seq.name));
            if seq.data_type != "bigint" {
                s.push_str(&format!(" AS {}", seq.data_type));
            }
            for o in creation_options(&seq.data_type, &seq.options) {
                s.push(' ');
                s.push_str(&o);
            }
            s
        }
        Change::DropSequence(seq) => drop("SEQUENCE", n(&seq.name)),
        Change::AlterSequence { from, to } => {
            let retyped = from.data_type != to.data_type;
            let mut s = format!("ALTER SEQUENCE {}", n(&to.name));
            if retyped {
                s.push_str(&format!(" AS {}", to.data_type));
            }
            for o in option_clauses(Some(&from.options), &to.options, retyped) {
                s.push(' ');
                s.push_str(&o);
            }
            s
        }
        Change::SetSequenceOwner { name, owned_by, .. } => match owned_by {
            Some(o) => format!("ALTER SEQUENCE {} OWNED BY {}.{}", n(name), n(&o.table), quote_ident(&o.column)),
            None => format!("ALTER SEQUENCE {} OWNED BY NONE", n(name)),
        },
        Change::AddIdentity { table, column, type_name, identity } => {
            format!("ALTER TABLE {} ALTER COLUMN {} ADD {}", n(table), quote_ident(column), identity_clause(type_name, identity))
        }
        Change::DropIdentity { table, column, .. } => {
            format!("ALTER TABLE {} ALTER COLUMN {} DROP IDENTITY", n(table), quote_ident(column))
        }
        Change::AlterIdentity { table, column, from, to, type_changed, .. } => {
            let mut parts = Vec::new();
            if from.generated != to.generated {
                parts.push(format!("SET GENERATED {}", generated(to.generated)));
            }
            parts.extend(option_clauses(Some(&from.options), &to.options, *type_changed).into_iter().map(|o| format!("SET {o}")));
            format!("ALTER TABLE {} ALTER COLUMN {} {}", n(table), quote_ident(column), parts.join(" "))
        }
        Change::AddEnumValue { name, value, position } => {
            let at = match position {
                EnumPosition::Before(l) => format!(" BEFORE {}", quote_literal(l)),
                EnumPosition::After(l) => format!(" AFTER {}", quote_literal(l)),
                EnumPosition::End => String::new(),
            };
            format!("ALTER TYPE {} ADD VALUE {}{at}", n(name), quote_literal(value))
        }
        Change::RenameEnumValue { name, from, to } => {
            // The catalog cannot tell a rename from removing one value and adding another, so say
            // what this does to stored data.
            format!(
                "-- renames a value: rows holding {} will hold {}\nALTER TYPE {} RENAME VALUE {} TO {}",
                quote_literal(from),
                quote_literal(to),
                n(name),
                quote_literal(from),
                quote_literal(to)
            )
        }
        Change::UnsupportedEnumChange { name, .. } => format!("-- unsupported change to enum {}", n(name)),
        Change::UnsupportedDomainChange { name, .. } => format!("-- unsupported change to domain {}", n(name)),
        Change::UnsupportedCompositeChange { name, .. } => format!("-- unsupported change to composite type {}", n(name)),
        Change::CreateDomain(d) => {
            let mut s = format!("CREATE DOMAIN {} AS {}", n(&d.name), d.base_type_clause());
            // NOT NULL before DEFAULT: pgmem reads `DEFAULT <expr> NOT NULL` as a default of
            // `<expr> IS NOT NULL` (kaeawc/pgmem#261); Postgres takes either order.
            if d.not_null {
                s.push_str(" NOT NULL");
            }
            if let Some(default) = &d.default {
                s.push_str(&format!(" DEFAULT {default}"));
            }
            for c in &d.constraints {
                s.push_str(&format!(" {}", domain_constraint_def(c)));
            }
            s
        }
        Change::DropDomain(d) => drop("DOMAIN", n(&d.name)),
        Change::SetDomainDefault { name, default } => format!("ALTER DOMAIN {} SET DEFAULT {default}", n(name)),
        Change::DropDomainDefault { name } => format!("ALTER DOMAIN {} DROP DEFAULT", n(name)),
        Change::SetDomainNotNull { name } => format!("ALTER DOMAIN {} SET NOT NULL", n(name)),
        Change::DropDomainNotNull { name } => format!("ALTER DOMAIN {} DROP NOT NULL", n(name)),
        Change::AddDomainConstraint { domain, constraint } => {
            format!("ALTER DOMAIN {} ADD {}", n(domain), domain_constraint_def(constraint))
        }
        Change::DropDomainConstraint { domain, constraint } => {
            format!("ALTER DOMAIN {} DROP CONSTRAINT {}", n(domain), quote_ident(&constraint.name))
        }
        Change::ValidateDomainConstraint { domain, name } => {
            format!("ALTER DOMAIN {} VALIDATE CONSTRAINT {}", n(domain), quote_ident(name))
        }
        Change::CreateCompositeType(c) => {
            let attributes: Vec<String> =
                c.attributes.iter().map(|a| format!("    {} {}", quote_ident(&a.name), a.type_clause())).collect();
            if attributes.is_empty() {
                format!("CREATE TYPE {} AS ()", n(&c.name))
            } else {
                format!("CREATE TYPE {} AS (\n{}\n)", n(&c.name), attributes.join(",\n"))
            }
        }
        Change::DropCompositeType(c) => drop("TYPE", n(&c.name)),
        Change::AddAttribute { composite, attribute } => {
            format!("ALTER TYPE {} ADD ATTRIBUTE {} {}", n(composite), quote_ident(&attribute.name), attribute.type_clause())
        }
        Change::DropAttribute { composite, attribute, .. } => {
            format!("ALTER TYPE {} DROP ATTRIBUTE{ie} {}", n(composite), quote_ident(attribute))
        }
        Change::AlterAttributeType { composite, attribute, type_name, collation, .. } => {
            let collate = collation.as_ref().map(|c| format!(" COLLATE {c}")).unwrap_or_default();
            format!("ALTER TYPE {} ALTER ATTRIBUTE {} TYPE {type_name}{collate}", n(composite), quote_ident(attribute))
        }
        Change::AddColumn { table, column } => format!("ALTER TABLE {} ADD COLUMN{ine} {}", n(table), column_def(column)),
        Change::DropColumn { table, column, .. } => format!("ALTER TABLE {} DROP COLUMN{ie} {}{cascade}", n(table), quote_ident(column)),
        Change::AlterColumnType { table, column, type_name, using, .. } => {
            let using = using.as_ref().map(|u| format!(" USING {u}")).unwrap_or_default();
            format!("ALTER TABLE {} ALTER COLUMN {} TYPE {type_name}{using}", n(table), quote_ident(column))
        }
        Change::SetNotNull { table, column } => format!("ALTER TABLE {} ALTER COLUMN {} SET NOT NULL", n(table), quote_ident(column)),
        Change::DropNotNull { table, column } => format!("ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL", n(table), quote_ident(column)),
        Change::SetDefault { table, column, default, .. } => {
            format!("ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {default}", n(table), quote_ident(column))
        }
        Change::DropDefault { table, column, .. } => format!("ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT", n(table), quote_ident(column)),
        Change::AddConstraint { table, constraint } => format!("ALTER TABLE {} ADD {}", n(table), constraint_def(constraint)),
        Change::DropConstraint { table, constraint } => {
            format!("ALTER TABLE {} DROP CONSTRAINT{ie} {}{cascade}", n(table), quote_ident(&constraint.name))
        }
        Change::RenameConstraint { table, from, to } => {
            format!("ALTER TABLE {} RENAME CONSTRAINT {} TO {}", n(table), quote_ident(from), quote_ident(&to.name))
        }
        Change::AlterConstraint { table, to } => {
            let timing = match (to.deferrable, to.deferred) {
                (false, _) => "NOT DEFERRABLE",
                (true, false) => "DEFERRABLE INITIALLY IMMEDIATE",
                (true, true) => "DEFERRABLE INITIALLY DEFERRED",
            };
            format!("ALTER TABLE {} ALTER CONSTRAINT {} {timing}", n(table), quote_ident(&to.name))
        }
        // Postgres prints a partitioned table's index `ON ONLY`, which would build it without its
        // partitions' indexes (and leave it invalid): the plan builds it on all of them. pgmem prints no `ONLY` (kaeawc/pgmem#287).
        Change::CreateIndex { index, .. } if index.on_partitioned_table => {
            create_index_if_not_exists(&index.definition.replacen(" ON ONLY ", " ON ", 1), opts)
        }
        Change::CreateIndex { index, .. } => create_index_if_not_exists(&index.definition, opts),
        Change::DropIndex { table, index } => drop("INDEX", n(&QualName::new(table.schema.clone(), index.name.clone()))),
        Change::SetComment { target, comment } => {
            let object = match target {
                CommentTarget::Schema(s) => format!("SCHEMA {}", quote_ident(s)),
                CommentTarget::Table(t) => format!("TABLE {}", n(t)),
                CommentTarget::Column { table: t, column: c } => format!("COLUMN {}.{}", n(t), quote_ident(c)),
                CommentTarget::Type(t) => format!("TYPE {}", n(t)),
                CommentTarget::Sequence(s) => format!("SEQUENCE {}", n(s)),
                CommentTarget::Constraint { table: t, name: c } => format!("CONSTRAINT {} ON {}", quote_ident(c), n(t)),
                CommentTarget::Index { table: t, name: i } => format!("INDEX {}", n(&QualName::new(t.schema.clone(), i.clone()))),
                CommentTarget::DomainConstraint { domain: d, name: c } => format!("CONSTRAINT {} ON DOMAIN {}", quote_ident(c), n(d)),
                CommentTarget::Attribute { composite: t, attribute: a } => format!("COLUMN {}.{}", n(t), quote_ident(a)),
            };
            format!("COMMENT ON {object} IS {}", comment.as_deref().map_or("NULL".to_owned(), quote_literal))
        }
    }
}

/// `CONSTRAINT <name> <check>`, with `NOT VALID` when the constraint is not validated, as written
/// in `CREATE DOMAIN` (which only carries validated ones) and `ADD`. Postgres prints `NOT VALID` in
/// the definition, pgmem does not (kaeawc/pgmem#266): it is written from the flag either way.
fn domain_constraint_def(c: &DomainConstraint) -> String {
    let not_valid = if c.validated { "" } else { " NOT VALID" };
    format!("CONSTRAINT {} {}{not_valid}", quote_ident(&c.name), c.body())
}

/// `CONSTRAINT <name> <definition>`, as written in `CREATE TABLE` and `ADD`.
fn constraint_def(c: &Constraint) -> String {
    format!("CONSTRAINT {} {}", quote_ident(&c.name), c.definition)
}

fn column_def(c: &Column) -> String {
    let mut s = format!("{} {}", quote_ident(&c.name), c.type_name);
    if let Some(identity) = &c.identity {
        s.push(' ');
        s.push_str(&identity_clause(&c.type_name, identity));
    }
    if let Some(d) = &c.default {
        s.push_str(" DEFAULT ");
        s.push_str(d);
    }
    if c.not_null {
        s.push_str(" NOT NULL");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgmig_model::{Enum, Sequence, Table};

    /// The JSON of a plan is part of pgmig's interface: a change here must be deliberate.
    #[test]
    fn plan_json_is_pinned_and_round_trips() {
        let plan = Plan {
            statements: vec!["ALTER TYPE e ADD VALUE 'b'".into(), "CREATE TABLE t (c e)".into()],
            commit_after: Some(0),
            ..Plan::default()
        };
        let json = serde_json::to_string(&plan).expect("serialize");
        assert_eq!(json, r#"{"statements":["ALTER TYPE e ADD VALUE 'b'","CREATE TABLE t (c e)"],"commit_after":0}"#);
        assert_eq!(serde_json::from_str::<Plan>(&json).expect("deserialize"), plan);
        let empty = Plan::default();
        assert_eq!(serde_json::to_string(&empty).expect("serialize"), r#"{"statements":[],"commit_after":null}"#);
        assert_eq!(serde_json::from_str::<Plan>(r#"{"statements":[],"commit_after":null}"#).expect("deserialize"), empty);
    }

    fn col(name: &str, type_ref: Option<QualName>) -> Column {
        let type_name = type_ref.as_ref().map_or("integer".to_owned(), |t| t.name.clone());
        Column {
            name: name.into(),
            type_name,
            type_ref,
            not_null: false,
            default: None,
            comment: None,
            default_sequences: Vec::new(),
            identity: None,
        }
    }

    fn table(schema: &str, name: &str, columns: Vec<Column>) -> Table {
        Table {
            name: QualName::new(schema, name),
            columns,
            comment: None,
            constraints: vec![],
            indexes: vec![],
            partition_key: None,
            partition_of: None,
        }
    }

    fn row_type(schema: &str, name: &str) -> Option<QualName> {
        Some(QualName::new(schema, name))
    }

    fn drop_table(table: Table) -> Change {
        Change::DropTable { table, owned_sequences: vec![] }
    }

    fn sql(changes: &[Change]) -> Vec<String> {
        plan(changes).expect("plan").statements
    }

    #[test]
    fn schema_is_created_before_its_table_and_dropped_after() {
        let t = table("app", "t", vec![col("id", None)]);
        let created = sql(&[Change::CreateTable(t.clone()), Change::CreateSchema("app".into())]);
        assert_eq!(created, ["CREATE SCHEMA app", "CREATE TABLE app.t (\n    id integer\n)"]);
        let dropped = sql(&[Change::DropSchema("app".into()), drop_table(t)]);
        assert_eq!(dropped, ["DROP TABLE app.t", "DROP SCHEMA app"]);
    }

    #[test]
    fn a_table_using_another_tables_row_type_is_created_after_it_and_dropped_before_it() {
        // Name order (and the order the diff emits) is the wrong one in both directions.
        let path = table("public", "apath", vec![col("start", row_type("public", "zpoint"))]);
        let point = table("public", "zpoint", vec![col("x", None)]);
        let created = sql(&[Change::CreateTable(path.clone()), Change::CreateTable(point.clone())]);
        assert_eq!(created, ["CREATE TABLE public.zpoint (\n    x integer\n)", "CREATE TABLE public.apath (\n    start zpoint\n)"]);

        let point = table("public", "apoint", vec![col("x", None)]);
        let path = table("public", "zpath", vec![col("start", row_type("public", "apoint"))]);
        let dropped = sql(&[drop_table(point), drop_table(path)]);
        assert_eq!(dropped, ["DROP TABLE public.zpath", "DROP TABLE public.apoint"]);
    }

    #[test]
    fn column_changes_wait_for_the_types_they_use() {
        let t = QualName::new("public", "t");
        let new = table("public", "znew", vec![col("x", None)]);
        let old = table("public", "aold", vec![col("x", None)]);
        let changes = [
            drop_table(old),
            Change::DropColumn {
                table: t.clone(),
                column: "gone".into(),
                type_ref: row_type("public", "aold"),
                sequences: vec![],
                owned_sequences: vec![],
            },
            Change::AlterColumnType {
                table: t.clone(),
                column: "c".into(),
                type_name: "znew".into(),
                type_ref: row_type("public", "znew"),
                old_type_ref: row_type("public", "aold"),
                using: None,
            },
            Change::AddColumn { table: t, column: col("added", row_type("public", "znew")) },
            Change::CreateTable(new),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE TABLE public.znew (\n    x integer\n)",
                "ALTER TABLE public.t ADD COLUMN added znew",
                "ALTER TABLE public.t ALTER COLUMN c TYPE znew",
                "ALTER TABLE public.t DROP COLUMN gone",
                "DROP TABLE public.aold",
            ]
        );
    }

    #[test]
    fn a_type_change_with_using_runs_between_dropping_and_setting_the_default() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault { table: t.clone(), column: "n".into(), default: "1".into(), sequences: vec![], old_sequences: vec![] },
            Change::AlterColumnType {
                table: t.clone(),
                column: "n".into(),
                type_name: "integer".into(),
                type_ref: None,
                old_type_ref: None,
                using: Some("n::integer".into()),
            },
            Change::DropDefault { table: t, column: "n".into(), sequences: vec![] },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ALTER COLUMN n DROP DEFAULT",
                "ALTER TABLE public.t ALTER COLUMN n TYPE integer USING n::integer",
                "ALTER TABLE public.t ALTER COLUMN n SET DEFAULT 1",
            ]
        );
    }

    #[test]
    fn unrelated_statements_keep_phase_then_diff_order() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::DropSchema("old".into()),
            Change::SetNotNull { table: t.clone(), column: "a".into() },
            Change::CreateTable(table("public", "b", vec![])),
            Change::DropDefault { table: t.clone(), column: "a".into(), sequences: vec![] },
            Change::CreateTable(table("public", "a", vec![])),
            Change::CreateSchema("new".into()),
        ];
        let expected = [
            "CREATE SCHEMA new",
            "CREATE TABLE public.b ()",
            "CREATE TABLE public.a ()",
            "ALTER TABLE public.t ALTER COLUMN a DROP DEFAULT",
            "ALTER TABLE public.t ALTER COLUMN a SET NOT NULL",
            "DROP SCHEMA old",
        ];
        assert_eq!(sql(&changes), expected);
        assert_eq!(sql(&changes), expected, "planning twice gives the same order");
    }

    #[test]
    fn a_cycle_is_reported_with_its_statements() {
        let a = table("public", "a", vec![col("b", row_type("public", "b"))]);
        let b = table("public", "b", vec![col("a", row_type("public", "a"))]);
        let err = plan(&[Change::CreateTable(a), Change::CreateTable(b)]).expect_err("cycle");
        let Error::Cycle(statements) = &err else { panic!("expected a cycle: {err}") };
        assert_eq!(statements.len(), 2);
        assert!(err.to_string().starts_with("the changes depend on each other in a cycle: CREATE TABLE"), "{err}");
    }

    #[test]
    fn comments_follow_the_objects_they_describe_and_are_quoted() {
        let t = QualName::new("app", "t");
        let mut created = table("app", "t", vec![col("id", None)]);
        created.comment = Some("it's".into());
        let changes = [
            Change::SetComment { target: CommentTarget::Column { table: t.clone(), column: "id".into() }, comment: Some("a\\b\nc".into()) },
            Change::SetComment { target: CommentTarget::Table(t.clone()), comment: Some("it's".into()) },
            Change::SetComment { target: CommentTarget::Schema("app".into()), comment: None },
            Change::CreateTable(created),
            Change::CreateSchema("app".into()),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE SCHEMA app",
                "CREATE TABLE app.t (\n    id integer\n)",
                "COMMENT ON COLUMN app.t.id IS E'a\\\\b\nc'",
                "COMMENT ON TABLE app.t IS 'it''s'",
                "COMMENT ON SCHEMA app IS NULL",
            ]
        );
    }

    fn mood(labels: &[&str]) -> Enum {
        Enum { name: QualName::new("app", "mood"), labels: labels.iter().map(|l| l.to_string()).collect(), comment: None }
    }

    fn add(value: &str, position: EnumPosition) -> Change {
        Change::AddEnumValue { name: QualName::new("app", "mood"), value: value.into(), position }
    }

    #[test]
    fn an_enum_is_created_before_its_columns_and_dropped_after_them() {
        let m = row_type("app", "mood");
        let t = QualName::new("public", "t");
        let users = table("public", "users", vec![col("m", m.clone())]);
        let created = sql(&[
            Change::AddColumn { table: t.clone(), column: col("m", m.clone()) },
            Change::CreateTable(users.clone()),
            Change::SetComment { target: CommentTarget::Type(QualName::new("app", "mood")), comment: Some("it's".into()) },
            Change::CreateEnum(mood(&["a", "it's"])),
            Change::CreateSchema("app".into()),
        ]);
        assert_eq!(
            created,
            [
                "CREATE SCHEMA app",
                "CREATE TYPE app.mood AS ENUM ('a', 'it''s')",
                "CREATE TABLE public.users (\n    m mood\n)",
                "ALTER TABLE public.t ADD COLUMN m mood",
                "COMMENT ON TYPE app.mood IS 'it''s'",
            ]
        );
        let dropped = sql(&[
            Change::DropSchema("app".into()),
            Change::DropEnum(mood(&["a"])),
            drop_table(users),
            Change::DropColumn { table: t, column: "m".into(), type_ref: m, sequences: vec![], owned_sequences: vec![] },
        ]);
        assert_eq!(dropped, ["ALTER TABLE public.t DROP COLUMN m", "DROP TABLE public.users", "DROP TYPE app.mood", "DROP SCHEMA app"]);
    }

    #[test]
    fn added_values_come_first_in_diff_order_and_are_committed_before_the_rest() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault {
                table: t.clone(),
                column: "m".into(),
                default: "'b'::app.mood".into(),
                sequences: vec![],
                old_sequences: vec![],
            },
            add("a", EnumPosition::Before("c".into())),
            Change::CreateSchema("other".into()),
            add("b", EnumPosition::After("a".into())),
            add("z", EnumPosition::End),
        ];
        let p = plan(&changes).expect("plan");
        assert_eq!(
            p.statements,
            [
                "ALTER TYPE app.mood ADD VALUE 'a' BEFORE 'c'",
                "ALTER TYPE app.mood ADD VALUE 'b' AFTER 'a'",
                "ALTER TYPE app.mood ADD VALUE 'z'",
                "CREATE SCHEMA other",
                "ALTER TABLE public.t ALTER COLUMN m SET DEFAULT 'b'::app.mood",
            ]
        );
        assert_eq!(p.commit_after, Some(2));
        let units = p.units();
        assert_eq!(units.len(), 2);
        assert!(p.unit_sql(&units[1]).starts_with("CREATE SCHEMA other;\n"), "{units:?}");
        assert_eq!(p.to_sql(), units.iter().map(|u| p.unit_sql(u)).collect::<Vec<_>>().join(COMMIT_MARKER));

        let only_adds = plan(&changes[1..2]).expect("plan");
        assert_eq!(only_adds.commit_after, None);
        assert_eq!(only_adds.to_sql(), "ALTER TYPE app.mood ADD VALUE 'a' BEFORE 'c';\n");
    }

    fn renames() -> Vec<Change> {
        let t = QualName::new("public", "accounts");
        vec![
            Change::RenameIndex { table: t.clone(), from: "i".into(), to: "j".into() },
            Change::RenameColumn { table: t.clone(), from: "name".into(), to: "Full Name".into() },
            Change::RenameTable { from: QualName::new("public", "users"), to: "accounts".into() },
            Change::RenameEnum { from: QualName::new("app", "mood"), to: "feeling".into() },
            Change::RenameDomain { from: QualName::new("public", "d"), to: "e".into() },
            Change::RenameCompositeType { from: QualName::new("public", "p"), to: "q".into() },
            Change::RenameSequence { from: QualName::new("public", "s"), to: "r".into() },
        ]
    }

    #[test]
    fn renames_run_first_types_and_tables_then_columns_then_indexes() {
        let mut changes = vec![Change::CreateSchema("other".into())];
        changes.extend(renames());
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.users RENAME TO accounts",
                "ALTER TYPE app.mood RENAME TO feeling",
                "ALTER DOMAIN public.d RENAME TO e",
                "ALTER TYPE public.p RENAME TO q",
                "ALTER SEQUENCE public.s RENAME TO r",
                "ALTER TABLE public.accounts RENAME COLUMN name TO \"Full Name\"",
                "ALTER INDEX public.i RENAME TO j",
                "CREATE SCHEMA other",
            ]
        );
        let opts = PlanOptions { unqualified_schema: Some("public".into()), ..PlanOptions::default() };
        let p = plan_with(&renames(), &opts).expect("plan");
        assert!(p.statements.contains(&"ALTER TABLE users RENAME TO accounts".to_owned()), "{:?}", p.statements);
        assert!(p.statements.contains(&"ALTER INDEX i RENAME TO j".to_owned()), "{:?}", p.statements);
    }

    #[test]
    fn renames_come_before_added_enum_values_and_are_not_a_commit_point() {
        let t = QualName::new("public", "t");
        let rename = Change::RenameEnum { from: QualName::new("app", "old_mood"), to: "mood".into() };
        let changes = [
            add("a", EnumPosition::End),
            rename.clone(),
            Change::SetDefault { table: t, column: "m".into(), default: "'a'::app.mood".into(), sequences: vec![], old_sequences: vec![] },
        ];
        let p = plan(&changes).expect("plan");
        assert_eq!(
            p.statements,
            [
                "ALTER TYPE app.old_mood RENAME TO mood",
                "ALTER TYPE app.mood ADD VALUE 'a'",
                "ALTER TABLE public.t ALTER COLUMN m SET DEFAULT 'a'::app.mood",
            ]
        );
        // The rename needs no commit before the rest; the new value does.
        assert_eq!(p.commit_after, Some(1));
        let only = plan(&[rename.clone(), Change::CreateSchema("x".into())]).expect("plan");
        assert_eq!(only.commit_after, None);
        assert_eq!(plan(&[rename]).expect("plan").commit_after, None);
    }

    #[test]
    fn a_renamed_value_runs_before_defaults_that_use_it() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::SetDefault {
                table: t,
                column: "m".into(),
                default: "'new'::app.mood".into(),
                sequences: vec![],
                old_sequences: vec![],
            },
            Change::RenameEnumValue { name: QualName::new("app", "mood"), from: "it's".into(), to: "new".into() },
        ];
        let p = plan(&changes).expect("plan");
        assert_eq!(
            p.statements,
            [
                "-- renames a value: rows holding 'it''s' will hold 'new'\nALTER TYPE app.mood RENAME VALUE 'it''s' TO 'new'",
                "ALTER TABLE public.t ALTER COLUMN m SET DEFAULT 'new'::app.mood"
            ]
        );
        assert_eq!(p.commit_after, None);
    }

    #[test]
    fn removing_or_reordering_values_is_an_unsupported_change_error() {
        let changes = [
            Change::CreateSchema("app".into()),
            Change::UnsupportedEnumChange { name: QualName::new("app", "mood"), from: vec!["a".into(), "b".into()], to: vec!["b".into()] },
        ];
        let err = plan(&changes).expect_err("unsupported");
        assert_eq!(
            err.to_string(),
            "unsupported changes: enum app.mood: values ('a', 'b') cannot become ('b'); only adding values and renaming one value \
             in place are supported, not removing or reordering values"
        );
    }

    fn opts(data_type: &str, increment: i64) -> SequenceOptions {
        SequenceOptions::defaults(data_type, increment).expect("integer type")
    }

    fn sequence(name: &str, data_type: &str, options: SequenceOptions) -> Sequence {
        Sequence { name: QualName::new("public", name), data_type: data_type.into(), options, owned_by: None, comment: None }
    }

    #[test]
    fn create_sequence_prints_only_options_that_differ_from_the_defaults() {
        let create = |data_type: &str, options| sql(&[Change::CreateSequence(sequence("s", data_type, options))]);
        assert_eq!(create("bigint", opts("bigint", 1)), ["CREATE SEQUENCE public.s"]);
        assert_eq!(create("integer", opts("integer", -1)), ["CREATE SEQUENCE public.s AS integer INCREMENT BY -1"]);
        let custom = SequenceOptions { start: 5, increment: 2, min: 3, max: 100, cache: 4, cycle: true };
        assert_eq!(
            create("smallint", custom),
            ["CREATE SEQUENCE public.s AS smallint INCREMENT BY 2 MINVALUE 3 MAXVALUE 100 START WITH 5 CACHE 4 CYCLE"]
        );
        // The start follows the minimum given.
        assert_eq!(create("bigint", SequenceOptions { start: 3, min: 3, ..opts("bigint", 1) }), ["CREATE SEQUENCE public.s MINVALUE 3"]);
    }

    #[test]
    fn alter_sequence_prints_changed_options_and_both_bounds_on_a_new_type() {
        let from = sequence("s", "smallint", opts("smallint", 1));
        let to = sequence("s", "smallint", SequenceOptions { cache: 10, cycle: true, ..opts("smallint", 1) });
        assert_eq!(sql(&[Change::AlterSequence { from: from.clone(), to }]), ["ALTER SEQUENCE public.s CACHE 10 CYCLE"]);
        let to = sequence("s", "integer", opts("integer", 1));
        assert_eq!(
            sql(&[Change::AlterSequence { from: from.clone(), to }]),
            ["ALTER SEQUENCE public.s AS integer MINVALUE 1 MAXVALUE 2147483647"]
        );
        let back = sequence("s", "smallint", SequenceOptions { cycle: false, ..opts("smallint", 1) });
        let cycling = sequence("s", "smallint", SequenceOptions { cycle: true, ..opts("smallint", 1) });
        assert_eq!(sql(&[Change::AlterSequence { from: cycling, to: back }]), ["ALTER SEQUENCE public.s NO CYCLE"]);
    }

    fn serial_column(seq: &str) -> Column {
        Column {
            default: Some(format!("nextval('{seq}'::regclass)")),
            default_sequences: vec![QualName::new("public", seq)],
            not_null: true,
            ..col("id", None)
        }
    }

    #[test]
    fn a_serial_table_creates_its_sequence_first_and_owns_it_last() {
        let t = QualName::new("public", "t");
        let owned = Some(OwnedBy { table: t.clone(), column: "id".into() });
        let changes = [
            Change::SetSequenceOwner { name: QualName::new("public", "t_id_seq"), owned_by: owned, old: None },
            Change::CreateTable(table("public", "t", vec![serial_column("t_id_seq")])),
            Change::CreateSequence(sequence("t_id_seq", "integer", opts("integer", 1))),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE SEQUENCE public.t_id_seq AS integer",
                "CREATE TABLE public.t (\n    id integer DEFAULT nextval('t_id_seq'::regclass) NOT NULL\n)",
                "ALTER SEQUENCE public.t_id_seq OWNED BY public.t.id",
            ]
        );
    }

    #[test]
    fn serial_to_identity_drops_the_default_then_the_sequence_then_adds_the_identity() {
        let t = QualName::new("public", "t");
        let seq = QualName::new("public", "t_id_seq");
        let identity = Identity { generated: Generated::Always, options: opts("integer", 1), sequence: None };
        let changes = [
            Change::AddIdentity { table: t.clone(), column: "id".into(), type_name: "integer".into(), identity },
            Change::DropSequence(sequence("t_id_seq", "integer", opts("integer", 1))),
            Change::DropDefault { table: t.clone(), column: "id".into(), sequences: vec![seq] },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ALTER COLUMN id DROP DEFAULT",
                "DROP SEQUENCE public.t_id_seq",
                "ALTER TABLE public.t ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY",
            ]
        );
    }

    #[test]
    fn identity_to_serial_drops_the_identity_before_creating_the_sequence() {
        let t = QualName::new("public", "t");
        let seq = QualName::new("public", "t_id_seq");
        let changes = [
            Change::CreateSequence(sequence("t_id_seq", "integer", opts("integer", 1))),
            Change::SetDefault {
                table: t.clone(),
                column: "id".into(),
                default: "nextval('t_id_seq'::regclass)".into(),
                sequences: vec![seq.clone()],
                old_sequences: vec![],
            },
            Change::SetSequenceOwner { name: seq, owned_by: Some(OwnedBy { table: t.clone(), column: "id".into() }), old: None },
            Change::DropIdentity { table: t, column: "id".into(), sequence: QualName::new("public", "t_id_seq") },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ALTER COLUMN id DROP IDENTITY",
                "CREATE SEQUENCE public.t_id_seq AS integer",
                "ALTER TABLE public.t ALTER COLUMN id SET DEFAULT nextval('t_id_seq'::regclass)",
                "ALTER SEQUENCE public.t_id_seq OWNED BY public.t.id",
            ]
        );
    }

    #[test]
    fn identity_changes_set_what_changed() {
        let t = QualName::new("public", "t");
        let always = Identity { generated: Generated::Always, options: opts("integer", 1), sequence: None };
        let by_default =
            Identity { generated: Generated::ByDefault, options: SequenceOptions { start: 7, ..opts("integer", 1) }, sequence: None };
        let alter = |from: &Identity, to: &Identity, type_changed| Change::AlterIdentity {
            table: t.clone(),
            column: "id".into(),
            type_name: "bigint".into(),
            from: from.clone(),
            to: to.clone(),
            type_changed,
        };
        assert_eq!(
            sql(&[alter(&always, &by_default, false)]),
            ["ALTER TABLE public.t ALTER COLUMN id SET GENERATED BY DEFAULT SET START WITH 7"]
        );
        let wide = Identity { options: opts("bigint", 1), ..always.clone() };
        assert_eq!(
            sql(&[alter(&always, &wide, true)]),
            ["ALTER TABLE public.t ALTER COLUMN id SET MINVALUE 1 SET MAXVALUE 9223372036854775807"]
        );
        let mut c = col("id", None);
        c.identity = Some(Identity { options: SequenceOptions { start: 10, increment: 5, ..opts("integer", 5) }, ..always });
        c.not_null = true;
        assert_eq!(
            sql(&[Change::AddColumn { table: t.clone(), column: c }]),
            ["ALTER TABLE public.t ADD COLUMN id integer GENERATED ALWAYS AS IDENTITY (INCREMENT BY 5 START WITH 10) NOT NULL"]
        );
        assert_eq!(
            sql(&[Change::DropIdentity { table: t, column: "id".into(), sequence: QualName::new("public", "t_id_seq") }]),
            ["ALTER TABLE public.t ALTER COLUMN id DROP IDENTITY"]
        );
    }

    #[test]
    fn dropping_a_table_runs_before_dropping_a_sequence_its_defaults_use() {
        let changes = [
            Change::DropSequence(sequence("free", "bigint", opts("bigint", 1))),
            drop_table(table("public", "t", vec![serial_column("free")])),
            Change::SetComment { target: CommentTarget::Sequence(QualName::new("public", "kept")), comment: Some("c".into()) },
        ];
        assert_eq!(sql(&changes), ["DROP TABLE public.t", "DROP SEQUENCE public.free", "COMMENT ON SEQUENCE public.kept IS 'c'"]);
    }

    #[test]
    fn dropping_the_owner_of_a_shared_sequence_runs_after_what_still_uses_it() {
        // t.id is serial; z.x takes values from t's sequence too. Dropping t.id drops the
        // sequence, so z (and its default) must go first (2BP01 otherwise).
        let t = QualName::new("public", "t");
        let seq = QualName::new("public", "t_id_seq");
        let mut x = serial_column("t_id_seq");
        x.name = "x".into();
        let z = table("public", "z", vec![x]);
        let drop_id = Change::DropColumn {
            table: t.clone(),
            column: "id".into(),
            type_ref: None,
            sequences: vec![seq.clone()],
            owned_sequences: vec![seq.clone()],
        };
        assert_eq!(sql(&[drop_id, drop_table(z.clone())]), ["DROP TABLE public.z", "ALTER TABLE public.t DROP COLUMN id"]);

        // Both tables dropped, the owner first in diff order.
        let owner = Change::DropTable { table: table("public", "t", vec![serial_column("t_id_seq")]), owned_sequences: vec![seq] };
        assert_eq!(sql(&[owner, drop_table(z)]), ["DROP TABLE public.z", "DROP TABLE public.t"]);
    }

    #[test]
    fn identity_to_serial_orders_by_the_identity_sequences_real_name() {
        // Postgres truncates `<table>_<column>_seq` to 63 bytes; the catalog's name is what counts.
        let long = "a".repeat(40);
        let t = QualName::new("public", long.clone());
        let truncated = format!("{long}_{}", "b".repeat(18));
        assert_eq!(truncated.len(), 59);
        let name = QualName::new("public", truncated.clone());
        let changes = [
            Change::CreateSequence(sequence(&truncated, "integer", opts("integer", 1))),
            Change::DropIdentity { table: t.clone(), column: "b".repeat(30), sequence: name },
        ];
        let planned = sql(&changes);
        assert!(planned[0].ends_with("DROP IDENTITY"), "{planned:?}");
        assert!(planned[1].starts_with("CREATE SEQUENCE"), "{planned:?}");

        // Adding an identity whose sequence takes a dropped sequence's name waits for the drop.
        let identity =
            Identity { generated: Generated::Always, options: opts("integer", 1), sequence: Some(QualName::new("public", "t_id_seq1")) };
        let changes = [
            Change::AddIdentity { table: QualName::new("public", "t"), column: "id".into(), type_name: "integer".into(), identity },
            Change::DropSequence(sequence("t_id_seq1", "integer", opts("integer", 1))),
        ];
        assert_eq!(sql(&changes)[0], "DROP SEQUENCE public.t_id_seq1");
    }

    fn constraint(name: &str, kind: pgmig_model::ConstraintKind, definition: &str, columns: &[&str]) -> Constraint {
        Constraint {
            name: name.into(),
            kind,
            definition: definition.into(),
            deferrable: false,
            deferred: false,
            validated: true,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            references: None,
            comment: None,
            index_definition: None,
            on_partitioned_table: false,
        }
    }

    fn pkey(table: &str) -> Constraint {
        constraint(&format!("{table}_pkey"), pgmig_model::ConstraintKind::PrimaryKey, "PRIMARY KEY (id)", &["id"])
    }

    /// `<from>_<column>_fkey`: `from.<column>` references `to.id` through `to_pkey`.
    fn fkey(from: &str, column: &str, to: &str) -> Constraint {
        let mut c = constraint(
            &format!("{from}_{column}_fkey"),
            pgmig_model::ConstraintKind::ForeignKey,
            &format!("FOREIGN KEY ({column}) REFERENCES {to}(id)"),
            &[column],
        );
        c.references = Some(pgmig_model::References {
            table: QualName::new("public", to),
            columns: vec!["id".into()],
            constraint: Some(format!("{to}_pkey")),
        });
        c
    }

    fn keyed(name: &str, other: &str, constraints: Vec<Constraint>) -> Table {
        let mut t = table("public", name, vec![Column { not_null: true, ..col("id", None) }, col(other, None)]);
        t.constraints = constraints;
        t
    }

    #[test]
    fn tables_referencing_each_other_are_created_first_and_their_foreign_keys_added_after() {
        let changes = [
            Change::CreateTable(keyed("a", "b_id", vec![pkey("a")])),
            Change::CreateTable(keyed("b", "a_id", vec![pkey("b")])),
            Change::AddConstraint { table: QualName::new("public", "a"), constraint: fkey("a", "b_id", "b") },
            Change::AddConstraint { table: QualName::new("public", "b"), constraint: fkey("b", "a_id", "a") },
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE TABLE public.a (\n    id integer NOT NULL,\n    b_id integer,\n    CONSTRAINT a_pkey PRIMARY KEY (id)\n)",
                "CREATE TABLE public.b (\n    id integer NOT NULL,\n    a_id integer,\n    CONSTRAINT b_pkey PRIMARY KEY (id)\n)",
                "ALTER TABLE public.a ADD CONSTRAINT a_b_id_fkey FOREIGN KEY (b_id) REFERENCES b(id)",
                "ALTER TABLE public.b ADD CONSTRAINT b_a_id_fkey FOREIGN KEY (a_id) REFERENCES a(id)",
            ]
        );
    }

    #[test]
    fn a_foreign_key_waits_for_the_key_it_uses_and_goes_before_it() {
        let p = QualName::new("public", "p");
        let t = QualName::new("public", "t");
        // In diff order the foreign key comes first both ways.
        let added = sql(&[
            Change::AddConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
            Change::AddConstraint { table: p.clone(), constraint: pkey("p") },
        ]);
        assert_eq!(
            added,
            [
                "ALTER TABLE public.p ADD CONSTRAINT p_pkey PRIMARY KEY (id)",
                "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)"
            ]
        );
        let dropped = sql(&[
            Change::DropConstraint { table: p.clone(), constraint: pkey("p") },
            Change::DropConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
        ]);
        assert_eq!(dropped, ["ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey", "ALTER TABLE public.p DROP CONSTRAINT p_pkey"]);
        // A dropped table goes after the foreign keys into it, whether they are dropped on their
        // own or with their table.
        let parent = keyed("p", "x", vec![pkey("p")]);
        let child = keyed("t", "pid", vec![fkey("t", "pid", "p")]);
        assert_eq!(
            sql(&[drop_table(parent.clone()), Change::DropConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") }]),
            ["ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey", "DROP TABLE public.p"]
        );
        assert_eq!(sql(&[drop_table(parent), drop_table(child)]), ["DROP TABLE public.t", "DROP TABLE public.p"]);
        // A table referencing itself has no cycle.
        assert_eq!(sql(&[drop_table(keyed("p", "up", vec![pkey("p"), fkey("p", "up", "p")]))]), ["DROP TABLE public.p"]);
    }

    #[test]
    fn a_key_is_dropped_before_its_column_changes_and_added_after() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::DropNotNull { table: t.clone(), column: "id".into() },
            Change::AddConstraint { table: t.clone(), constraint: pkey("t") },
            Change::AlterColumnType {
                table: t.clone(),
                column: "id".into(),
                type_name: "bigint".into(),
                type_ref: None,
                old_type_ref: None,
                using: None,
            },
            Change::DropConstraint { table: t.clone(), constraint: pkey("t") },
            Change::AddColumn { table: t.clone(), column: col("x", None) },
            Change::AddConstraint {
                table: t.clone(),
                constraint: constraint("x_pos", pgmig_model::ConstraintKind::Check, "CHECK ((x > 0))", &["x"]),
            },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t ADD COLUMN x integer",
                "ALTER TABLE public.t DROP CONSTRAINT t_pkey",
                "ALTER TABLE public.t ALTER COLUMN id TYPE bigint",
                "ALTER TABLE public.t ALTER COLUMN id DROP NOT NULL",
                "ALTER TABLE public.t ADD CONSTRAINT t_pkey PRIMARY KEY (id)",
                "ALTER TABLE public.t ADD CONSTRAINT x_pos CHECK ((x > 0))",
            ]
        );
    }

    #[test]
    fn a_foreign_key_is_dropped_before_the_referenced_column_changes_type_and_added_after() {
        let (p, t) = (QualName::new("public", "p"), QualName::new("public", "t"));
        let alter = |table: &QualName, column: &str| Change::AlterColumnType {
            table: table.clone(),
            column: column.into(),
            type_name: "text".into(),
            type_ref: None,
            old_type_ref: None,
            using: None,
        };
        let changes = [
            alter(&p, "id"),
            alter(&t, "pid"),
            Change::DropConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
            Change::AddConstraint { table: t.clone(), constraint: fkey("t", "pid", "p") },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey",
                "ALTER TABLE public.p ALTER COLUMN id TYPE text",
                "ALTER TABLE public.t ALTER COLUMN pid TYPE text",
                "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)",
            ]
        );
    }

    #[test]
    fn names_in_the_unqualified_schema_print_without_it() {
        let scoped = PlanOptions { unqualified_schema: Some("public".into()), ..PlanOptions::default() };
        let mut t = keyed("t", "pid", vec![pkey("t")]);
        t.columns.push(col("m", row_type("app", "mood")));
        let owned = Some(OwnedBy { table: t.name.clone(), column: "id".into() });
        let changes = [
            Change::CreateTable(t.clone()),
            Change::CreateSequence(sequence("Seq", "bigint", opts("bigint", 1))),
            Change::SetSequenceOwner { name: QualName::new("public", "Seq"), owned_by: owned, old: None },
            Change::SetComment { target: CommentTarget::Column { table: t.name.clone(), column: "id".into() }, comment: None },
            Change::AddConstraint { table: t.name.clone(), constraint: fkey("t", "pid", "p") },
            Change::SetComment { target: CommentTarget::Type(QualName::new("app", "mood")), comment: None },
        ];
        assert_eq!(
            plan_with(&changes, &scoped).expect("plan").statements,
            [
                "CREATE SEQUENCE \"Seq\"",
                "CREATE TABLE t (\n    id integer NOT NULL,\n    pid integer,\n    m mood,\n    CONSTRAINT t_pkey PRIMARY KEY (id)\n)",
                "ALTER SEQUENCE \"Seq\" OWNED BY t.id",
                "ALTER TABLE t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)",
                "COMMENT ON COLUMN t.id IS NULL",
                "COMMENT ON TYPE app.mood IS NULL",
            ]
        );
    }

    #[test]
    fn objects_referenced_but_not_planned_are_assumed_to_exist() {
        // `p`, its key and the `app.mood` type are not managed: nothing creates them.
        let mut t = keyed("t", "pid", vec![]);
        t.columns.push(col("m", row_type("app", "mood")));
        let changes = [Change::AddConstraint { table: t.name.clone(), constraint: fkey("t", "pid", "p") }, Change::CreateTable(t.clone())];
        assert_eq!(sql(&changes)[1], "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)");
        assert_eq!(sql(&[drop_table(keyed("t", "pid", vec![fkey("t", "pid", "p")]))]), ["DROP TABLE public.t"]);
    }

    #[test]
    fn renames_deferrability_and_comments_on_constraints() {
        let t = QualName::new("App", "t");
        let mut deferred = fkey("t", "pid", "p");
        deferred.name = "Odd".into();
        let alter = |deferrable, initially| {
            let to = Constraint { deferrable, deferred: initially, ..deferred.clone() };
            sql(&[Change::AlterConstraint { table: t.clone(), to }])
        };
        assert_eq!(alter(true, true), ["ALTER TABLE \"App\".t ALTER CONSTRAINT \"Odd\" DEFERRABLE INITIALLY DEFERRED"]);
        assert_eq!(alter(true, false), ["ALTER TABLE \"App\".t ALTER CONSTRAINT \"Odd\" DEFERRABLE INITIALLY IMMEDIATE"]);
        assert_eq!(alter(false, false), ["ALTER TABLE \"App\".t ALTER CONSTRAINT \"Odd\" NOT DEFERRABLE"]);
        let renamed = sql(&[
            Change::SetComment { target: CommentTarget::Constraint { table: t.clone(), name: "Odd".into() }, comment: Some("it's".into()) },
            Change::RenameConstraint { table: t.clone(), from: "old".into(), to: deferred.clone() },
        ]);
        assert_eq!(
            renamed,
            ["ALTER TABLE \"App\".t RENAME CONSTRAINT old TO \"Odd\"", "COMMENT ON CONSTRAINT \"Odd\" ON \"App\".t IS 'it''s'"]
        );
    }
    fn index(table: &str, name: &str, key: &str, columns: &[&str]) -> pgmig_model::Index {
        pgmig_model::Index {
            name: name.into(),
            definition: format!("CREATE INDEX {name} ON public.{table} USING btree {key}"),
            unique: false,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            comment: None,
            on_partitioned_table: false,
        }
    }

    #[test]
    fn an_index_is_created_after_its_table_and_columns_and_commented_after_that() {
        let t = QualName::new("public", "t");
        let i = index("t", "t_x_idx", "(lower(x))", &["x"]);
        let changes = [
            Change::SetComment { target: CommentTarget::Index { table: t.clone(), name: i.name.clone() }, comment: Some("it's".into()) },
            Change::CreateIndex { table: t.clone(), index: i },
            Change::AddColumn { table: t.clone(), column: col("x", None) },
            Change::CreateIndex { table: QualName::new("public", "n"), index: index("n", "n_idx", "(id)", &["id"]) },
            Change::CreateTable(table("public", "n", vec![col("id", None)])),
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE TABLE public.n (\n    id integer\n)",
                "ALTER TABLE public.t ADD COLUMN x integer",
                "CREATE INDEX t_x_idx ON public.t USING btree (lower(x))",
                "CREATE INDEX n_idx ON public.n USING btree (id)",
                "COMMENT ON INDEX public.t_x_idx IS 'it''s'",
            ]
        );
        let opts = PlanOptions { unqualified_schema: Some("public".into()), ..PlanOptions::default() };
        let drop = Change::DropIndex { table: t.clone(), index: index("t", "Odd", "(x)", &["x"]) };
        assert_eq!(plan_with(std::slice::from_ref(&drop), &opts).expect("plan").statements, ["DROP INDEX \"Odd\""]);
        assert_eq!(sql(&[drop]), ["DROP INDEX public.\"Odd\""]);
    }

    #[test]
    fn an_index_is_dropped_before_its_columns_change_or_go_and_before_its_name_is_reused() {
        let t = QualName::new("public", "t");
        let changes = [
            Change::CreateIndex { table: t.clone(), index: index("t", "t_idx", "(y)", &["y"]) },
            Change::DropColumn { table: t.clone(), column: "x".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
            Change::AlterColumnType {
                table: t.clone(),
                column: "y".into(),
                type_name: "bigint".into(),
                type_ref: None,
                old_type_ref: None,
                using: None,
            },
            // A constraint taking the index's name waits for the drop too.
            Change::AddConstraint {
                table: t.clone(),
                constraint: constraint("t_x_key", pgmig_model::ConstraintKind::Unique, "UNIQUE (y)", &["y"]),
            },
            Change::DropIndex { table: t.clone(), index: index("t", "t_x_key", "(x)", &["x"]) },
            Change::DropIndex { table: t.clone(), index: index("t", "t_idx", "(x, y)", &["x", "y"]) },
        ];
        assert_eq!(
            sql(&changes),
            [
                "DROP INDEX public.t_x_key",
                "DROP INDEX public.t_idx",
                "ALTER TABLE public.t ALTER COLUMN y TYPE bigint",
                "CREATE INDEX t_idx ON public.t USING btree (y)",
                "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE (y)",
                "ALTER TABLE public.t DROP COLUMN x",
            ]
        );
    }

    #[test]
    fn a_foreign_key_using_a_unique_index_is_dropped_before_it_and_added_after_it() {
        let mut f = fkey("t", "pid", "p");
        f.references.as_mut().expect("references").constraint = Some("p_id_idx".into());
        let t = QualName::new("public", "t");
        let p = QualName::new("public", "p");
        let unique = pgmig_model::Index { unique: true, ..index("p", "p_id_idx", "(id)", &["id"]) };
        let changes = [
            Change::DropIndex { table: p.clone(), index: unique.clone() },
            Change::CreateIndex { table: p, index: unique },
            Change::AddConstraint { table: t.clone(), constraint: f.clone() },
            Change::DropConstraint { table: t, constraint: f },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t DROP CONSTRAINT t_pid_fkey",
                "DROP INDEX public.p_id_idx",
                "CREATE INDEX p_id_idx ON public.p USING btree (id)",
                "ALTER TABLE public.t ADD CONSTRAINT t_pid_fkey FOREIGN KEY (pid) REFERENCES p(id)",
            ]
        );
    }

    #[test]
    fn tables_sequences_and_indexes_reuse_each_others_names_after_the_drop() {
        let t = QualName::new("public", "t");
        let foo = || index("t", "foo", "(id)", &["id"]);
        let drop_index = || Change::DropIndex { table: t.clone(), index: foo() };
        let create_index = || Change::CreateIndex { table: t.clone(), index: foo() };
        // Index `foo` dropped, a table or sequence `foo` created.
        assert_eq!(
            sql(&[drop_index(), Change::CreateTable(table("public", "foo", vec![col("id", None)]))]),
            ["DROP INDEX public.foo", "CREATE TABLE public.foo (\n    id integer\n)"]
        );
        assert_eq!(
            sql(&[drop_index(), Change::CreateSequence(sequence("foo", "bigint", opts("bigint", 1)))]),
            ["DROP INDEX public.foo", "CREATE SEQUENCE public.foo"]
        );
        // A table or sequence `foo` dropped, index `foo` created.
        assert_eq!(
            sql(&[create_index(), drop_table(table("public", "foo", vec![col("id", None)]))]),
            ["DROP TABLE public.foo", "CREATE INDEX foo ON public.t USING btree (id)"]
        );
        assert_eq!(
            sql(&[create_index(), Change::DropSequence(sequence("foo", "bigint", opts("bigint", 1)))]),
            ["DROP SEQUENCE public.foo", "CREATE INDEX foo ON public.t USING btree (id)"]
        );
        // Another schema's name is another relation.
        let other = Change::CreateTable(table("app", "foo", vec![col("id", None)]));
        assert_eq!(sql(&[other, drop_index()])[0], "CREATE TABLE app.foo (\n    id integer\n)");
    }

    fn domain(schema: &str, name: &str, base: &str, base_ref: Option<QualName>) -> pgmig_model::Domain {
        pgmig_model::Domain {
            name: QualName::new(schema, name),
            base_type: base.into(),
            base_ref,
            collation: None,
            default: None,
            not_null: false,
            constraints: vec![],
            comment: None,
        }
    }

    fn check(name: &str, definition: &str, validated: bool) -> DomainConstraint {
        DomainConstraint { name: name.into(), definition: definition.into(), validated, comment: None }
    }

    #[test]
    fn domains_and_composite_types_are_created_before_their_users_and_dropped_after_them() {
        let mood_type = row_type("app", "mood");
        // Name order is the wrong one: a domain over a domain over the enum, a composite type over
        // the outer domain, a table column of an array of the composite type.
        let inner = domain("public", "z_inner", "app.mood", mood_type);
        let outer = domain("public", "a_outer", "public.z_inner", row_type("public", "z_inner"));
        let attribute = pgmig_model::Attribute {
            name: "d".into(),
            type_name: "public.a_outer".into(),
            type_ref: row_type("public", "a_outer"),
            collation: None,
            comment: None,
        };
        let pair = pgmig_model::Composite { name: QualName::new("public", "b_pair"), attributes: vec![attribute], comment: None };
        let users =
            table("public", "a_users", vec![Column { type_name: "public.b_pair[]".into(), ..col("ps", row_type("public", "b_pair")) }]);
        let created = sql(&[
            Change::CreateTable(users.clone()),
            Change::CreateCompositeType(pair.clone()),
            Change::CreateDomain(outer.clone()),
            Change::CreateDomain(inner.clone()),
            Change::CreateEnum(mood(&["a"])),
        ]);
        assert_eq!(
            created,
            [
                "CREATE TYPE app.mood AS ENUM ('a')",
                "CREATE DOMAIN public.z_inner AS app.mood",
                "CREATE DOMAIN public.a_outer AS public.z_inner",
                "CREATE TYPE public.b_pair AS (\n    d public.a_outer\n)",
                "CREATE TABLE public.a_users (\n    ps public.b_pair[]\n)",
            ]
        );
        let dropped = sql(&[
            Change::DropEnum(mood(&["a"])),
            Change::DropDomain(inner),
            Change::DropDomain(outer),
            Change::DropCompositeType(pair),
            drop_table(users),
        ]);
        assert_eq!(
            dropped,
            [
                "DROP TABLE public.a_users",
                "DROP TYPE public.b_pair",
                "DROP DOMAIN public.a_outer",
                "DROP DOMAIN public.z_inner",
                "DROP TYPE app.mood"
            ]
        );
    }

    #[test]
    fn domain_statements_write_checks_and_not_valid_from_the_flag() {
        let mut d = domain("public", "posint", "integer", None);
        d.not_null = true;
        d.default = Some("1".into());
        d.collation = Some(QualName::new("pg_catalog", "C"));
        d.constraints = vec![check("posint_check", "CHECK ((VALUE > 0))", true)];
        let name = d.name.clone();
        let changes = [
            Change::AddDomainConstraint { domain: name.clone(), constraint: check("posint_small", "CHECK ((VALUE < 9)) NOT VALID", false) },
            // As pgmem prints it.
            Change::AddDomainConstraint { domain: name.clone(), constraint: check("posint_tiny", "CHECK ((VALUE < 5))", false) },
            Change::CreateDomain(d),
            Change::ValidateDomainConstraint { domain: name.clone(), name: "posint_small".into() },
        ];
        assert_eq!(
            sql(&changes),
            [
                "CREATE DOMAIN public.posint AS integer COLLATE pg_catalog.\"C\" NOT NULL DEFAULT 1 CONSTRAINT posint_check CHECK ((VALUE > 0))",
                "ALTER DOMAIN public.posint ADD CONSTRAINT posint_small CHECK ((VALUE < 9)) NOT VALID",
                "ALTER DOMAIN public.posint ADD CONSTRAINT posint_tiny CHECK ((VALUE < 5)) NOT VALID",
                "ALTER DOMAIN public.posint VALIDATE CONSTRAINT posint_small",
            ]
        );
        let opts = PlanOptions { unqualified_schema: Some("public".into()), ..PlanOptions::default() };
        let comment =
            Change::SetComment { target: CommentTarget::DomainConstraint { domain: name, name: "posint_check".into() }, comment: None };
        assert_eq!(plan_with(&[comment], &opts).expect("plan").statements, ["COMMENT ON CONSTRAINT posint_check ON DOMAIN posint IS NULL"]);
    }

    #[test]
    fn a_replaced_domain_check_is_dropped_before_it_is_added() {
        let name = QualName::new("public", "d");
        let changes = [
            Change::AddDomainConstraint { domain: name.clone(), constraint: check("d_check", "CHECK ((VALUE > 1))", true) },
            Change::DropDomainConstraint { domain: name, constraint: check("d_check", "CHECK ((VALUE > 0))", true) },
        ];
        assert_eq!(
            sql(&changes),
            ["ALTER DOMAIN public.d DROP CONSTRAINT d_check", "ALTER DOMAIN public.d ADD CONSTRAINT d_check CHECK ((VALUE > 1))"]
        );
    }

    #[test]
    fn attribute_changes_add_before_they_alter_and_drop_last() {
        let p = QualName::new("public", "pair");
        let attribute =
            pgmig_model::Attribute { name: "c".into(), type_name: "text".into(), type_ref: None, collation: None, comment: None };
        let changes = [
            Change::DropAttribute { composite: p.clone(), attribute: "b".into(), type_ref: row_type("public", "gone") },
            Change::DropDomain(domain("public", "gone", "integer", None)),
            Change::AlterAttributeType {
                composite: p.clone(),
                attribute: "a".into(),
                type_name: "text".into(),
                collation: Some(QualName::new("pg_catalog", "C")),
                type_ref: None,
                old_type_ref: None,
                holders: Vec::new(),
            },
            Change::AddAttribute { composite: p.clone(), attribute },
            Change::SetComment { target: CommentTarget::Attribute { composite: p, attribute: "c".into() }, comment: Some("c".into()) },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TYPE public.pair ADD ATTRIBUTE c text",
                "ALTER TYPE public.pair ALTER ATTRIBUTE a TYPE text COLLATE pg_catalog.\"C\"",
                "ALTER TYPE public.pair DROP ATTRIBUTE b",
                "DROP DOMAIN public.gone",
                "COMMENT ON COLUMN public.pair.c IS 'c'",
            ]
        );
    }

    fn retype_pair(holders: Vec<QualName>) -> Change {
        Change::AlterAttributeType {
            composite: QualName::new("public", "pair"),
            attribute: "a".into(),
            type_name: "bigint".into(),
            collation: None,
            type_ref: None,
            old_type_ref: None,
            holders,
        }
    }

    const RETYPE_PAIR: &str = "ALTER TYPE public.pair ALTER ATTRIBUTE a TYPE bigint";

    #[test]
    fn a_column_using_a_composite_type_is_dropped_before_its_attribute_type_changes() {
        let t = table("public", "t", vec![col("p", row_type("public", "pair"))]);
        assert_eq!(sql(&[retype_pair(vec![]), drop_table(t)]), ["DROP TABLE public.t", RETYPE_PAIR]);
        let dropped = Change::DropColumn {
            table: QualName::new("public", "t"),
            column: "p".into(),
            type_ref: row_type("public", "pair"),
            sequences: vec![],
            owned_sequences: vec![],
        };
        assert_eq!(sql(&[retype_pair(vec![]), dropped]), ["ALTER TABLE public.t DROP COLUMN p", RETYPE_PAIR]);
    }

    #[test]
    fn a_column_using_a_composite_type_is_created_after_its_attribute_type_changes() {
        let mut p = col("p", row_type("public", "pair"));
        p.type_name = "public.pair[]".into();
        let t = table("public", "t", vec![p.clone()]);
        assert_eq!(sql(&[retype_pair(vec![]), Change::CreateTable(t)]), [RETYPE_PAIR, "CREATE TABLE public.t (\n    p public.pair[]\n)"]);
        let retyped = Change::AlterColumnType {
            table: QualName::new("public", "u"),
            column: "p".into(),
            type_name: "public.pair[]".into(),
            type_ref: row_type("public", "pair"),
            old_type_ref: None,
            using: None,
        };
        assert_eq!(sql(&[retyped, retype_pair(vec![])]), [RETYPE_PAIR, "ALTER TABLE public.u ALTER COLUMN p TYPE public.pair[]"]);
    }

    #[test]
    fn uses_through_a_domain_or_a_nesting_composite_end_before_the_attribute_type_change_and_start_after() {
        let (d, outer) = (QualName::new("public", "d"), QualName::new("public", "nest"));
        let t = QualName::new("public", "t");
        let mut new = col("new", Some(d.clone()));
        new.type_name = "public.d[]".into();
        let held = pgmig_model::Attribute {
            name: "p".into(),
            type_name: "public.pair".into(),
            type_ref: row_type("public", "pair"),
            collation: None,
            comment: None,
        };
        let changes = [
            Change::AddColumn { table: t.clone(), column: new },
            Change::AddColumn { table: t.clone(), column: col("o", Some(outer.clone())) },
            Change::AddAttribute { composite: outer.clone(), attribute: held },
            Change::DropColumn { table: t, column: "old".into(), type_ref: Some(d.clone()), sequences: vec![], owned_sequences: vec![] },
            retype_pair(vec![d, outer]),
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.t DROP COLUMN old",
                RETYPE_PAIR,
                "ALTER TABLE public.t ADD COLUMN new public.d[]",
                "ALTER TABLE public.t ADD COLUMN o nest",
                "ALTER TYPE public.nest ADD ATTRIBUTE p public.pair",
            ]
        );
    }

    #[test]
    fn a_skipped_drop_of_a_composite_types_user_skips_the_attribute_type_change() {
        let t = table("public", "t", vec![col("p", row_type("public", "pair"))]);
        let changes = vec![retype_pair(vec![]), drop_table(t)];
        let kinds = [pgmig_diff::ChangeKind::DropTable].into_iter().collect();
        let (kept, skipped) = skip(&changes, &kinds, &PlanOptions::default());
        assert!(kept.is_empty());
        let shown: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["alter_attribute_type public.pair.a (because drop_table public.t is skipped)", "drop_table public.t (policy)"]);
    }

    #[test]
    fn domain_and_composite_changes_postgres_cannot_make_are_unsupported_change_errors() {
        let err = plan(&[
            Change::UnsupportedDomainChange {
                name: QualName::new("public", "d"),
                from: "integer".into(),
                to: "bigint".into(),
                used_by: vec!["column public.t.a".into(), "domain public.e".into()],
            },
            Change::UnsupportedCompositeChange {
                name: QualName::new("public", "p"),
                attribute: "x".into(),
                from: "integer".into(),
                to: "text".into(),
                used_by: vec!["column public.t.p".into()],
            },
        ])
        .expect_err("unsupported");
        assert_eq!(
            err.to_string(),
            "unsupported changes: domain public.d: the base type cannot change from integer to bigint; Postgres cannot change it in place, \
             and the domain cannot be dropped and created again while column public.t.a, domain public.e use it; \
             composite type public.p: attribute x cannot change type from integer to text while column public.t.p uses the type"
        );
    }

    #[test]
    fn a_skipped_create_domain_skips_what_uses_it() {
        let d = domain("public", "posint", "integer", None);
        let t = table("public", "t", vec![col("n", row_type("public", "posint"))]);
        let changes = vec![Change::CreateDomain(d), Change::CreateTable(t)];
        let kinds = [pgmig_diff::ChangeKind::CreateDomain].into_iter().collect();
        let (kept, skipped) = skip(&changes, &kinds, &PlanOptions::default());
        assert!(kept.is_empty());
        let shown: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            ["create_domain public.posint (policy)", "create_table public.t (because create_domain public.posint is skipped)"]
        );
    }

    fn concurrent() -> PlanOptions {
        PlanOptions { concurrent_indexes: true, ..PlanOptions::default() }
    }

    fn create_index(table: &str, name: &str) -> Change {
        Change::CreateIndex { table: QualName::new("public", table), index: index(table, name, "(x)", &["x"]) }
    }

    fn drop_index(table: &str, name: &str) -> Change {
        Change::DropIndex { table: QualName::new("public", table), index: index(table, name, "(x)", &["x"]) }
    }

    #[test]
    fn indexes_are_concurrent_only_with_the_policy() {
        let changes = [create_index("t", "t_new"), drop_index("t", "t_old")];
        let plain = plan(&changes).expect("plan");
        assert_eq!(plain.statements, ["DROP INDEX public.t_old", "CREATE INDEX t_new ON public.t USING btree (x)"]);
        assert!(plain.non_transactional.is_empty());
        assert_eq!(plain.units(), [Unit { statements: 0..2, transactional: true }]);

        let planned = plan_with(&changes, &concurrent()).expect("plan");
        assert_eq!(
            planned.statements,
            ["DROP INDEX CONCURRENTLY public.t_old", "CREATE INDEX CONCURRENTLY t_new ON public.t USING btree (x)"]
        );
        assert_eq!(planned.non_transactional, [0, 1]);
        // Each concurrent statement is a unit of its own.
        assert_eq!(planned.units(), [Unit { statements: 0..1, transactional: false }, Unit { statements: 1..2, transactional: false }]);
    }

    #[test]
    fn a_unique_index_is_built_concurrently_and_a_definition_not_in_the_usual_form_is_left_alone() {
        let unique = pgmig_model::Index {
            unique: true,
            definition: "CREATE UNIQUE INDEX t_x_key ON public.t USING btree (x)".into(),
            ..index("t", "t_x_key", "(x)", &["x"])
        };
        let odd = pgmig_model::Index { definition: "create index odd on public.t (x)".into(), ..index("t", "odd", "(x)", &["x"]) };
        let t = QualName::new("public", "t");
        let planned = plan_with(
            &[Change::CreateIndex { table: t.clone(), index: unique }, Change::CreateIndex { table: t, index: odd }],
            &concurrent(),
        )
        .expect("plan");
        assert_eq!(
            planned.statements,
            ["CREATE UNIQUE INDEX CONCURRENTLY t_x_key ON public.t USING btree (x)", "create index odd on public.t (x)"]
        );
        assert_eq!(planned.non_transactional, [0]);
    }

    #[test]
    fn indexes_of_tables_created_or_dropped_in_the_plan_stay_plain() {
        let mut new = table("public", "n", vec![col("x", None)]);
        new.indexes.push(index("n", "n_idx", "(x)", &["x"]));
        let mut gone = table("public", "g", vec![col("x", None)]);
        gone.indexes.push(index("g", "g_idx", "(x)", &["x"]));
        // A `CreateIndex` for a table created in the same plan (the diff folds these into the table,
        // but a caller may pass them apart), and the index of a dropped table.
        let changes = [
            Change::CreateTable(new),
            create_index("n", "n_extra"),
            drop_table(gone),
            drop_index("g", "g_extra"),
            create_index("t", "t_idx"),
        ];
        let planned = plan_with(&changes, &concurrent()).expect("plan");
        assert_eq!(planned.non_transactional.len(), 1, "{:#?}", planned.statements);
        let concurrent_ones: Vec<&String> = planned.non_transactional.iter().map(|&i| &planned.statements[i]).collect();
        assert_eq!(concurrent_ones, ["CREATE INDEX CONCURRENTLY t_idx ON public.t USING btree (x)"]);
        assert!(planned.statements.iter().any(|s| s == "CREATE INDEX n_extra ON public.n USING btree (x)"));
        assert!(planned.statements.iter().any(|s| s == "DROP INDEX public.g_extra"));
    }

    #[test]
    fn indexes_on_partitioned_tables_stay_plain() {
        let partitioned = |name: &str| pgmig_model::Index { on_partitioned_table: true, ..index("p", name, "(x)", &["x"]) };
        let p = QualName::new("public", "p");
        let planned = plan_with(
            &[
                Change::CreateIndex { table: p.clone(), index: partitioned("p_new") },
                Change::DropIndex { table: p, index: partitioned("p_old") },
            ],
            &concurrent(),
        )
        .expect("plan");
        assert_eq!(planned.statements, ["DROP INDEX public.p_old", "CREATE INDEX p_new ON public.p USING btree (x)"]);
        assert!(planned.non_transactional.is_empty());
    }

    #[test]
    fn constraint_indexes_are_left_to_their_constraints() {
        let t = QualName::new("public", "t");
        let unique = constraint("t_x_key", pgmig_model::ConstraintKind::Unique, "UNIQUE (x)", &["x"]);
        let changes =
            [Change::DropConstraint { table: t.clone(), constraint: pkey("t") }, Change::AddConstraint { table: t, constraint: unique }];
        let planned = plan_with(&changes, &concurrent()).expect("plan");
        assert!(planned.non_transactional.is_empty());
        assert!(planned.statements.iter().all(|s| !s.contains("CONCURRENTLY")), "{:?}", planned.statements);
    }

    /// A unique constraint `<table>_x_key` on `x`, with the index the server prints for it.
    fn unique_key(table: &str) -> Constraint {
        let mut c = constraint(&format!("{table}_x_key"), pgmig_model::ConstraintKind::Unique, "UNIQUE (x)", &["x"]);
        c.index_definition = Some(format!("CREATE UNIQUE INDEX {table}_x_key ON public.{table} USING btree (x)"));
        c
    }

    fn add_constraint(table: &str, constraint: Constraint) -> Change {
        Change::AddConstraint { table: QualName::new("public", table), constraint }
    }

    #[test]
    fn a_key_on_an_existing_table_is_built_concurrently_then_attached_with_using_index() {
        let mut pk = pkey("t");
        pk.index_definition = Some("CREATE UNIQUE INDEX t_pkey ON public.t USING btree (id)".into());
        let changes = [add_constraint("t", pk), add_constraint("t", unique_key("t"))];
        assert_eq!(
            plan(&changes).expect("plan").statements,
            ["ALTER TABLE public.t ADD CONSTRAINT t_pkey PRIMARY KEY (id)", "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE (x)"]
        );

        let planned = plan_with(&changes, &concurrent()).expect("plan");
        assert_eq!(
            planned.statements,
            [
                "CREATE UNIQUE INDEX CONCURRENTLY t_pkey ON public.t USING btree (id)",
                "ALTER TABLE public.t ADD CONSTRAINT t_pkey PRIMARY KEY USING INDEX t_pkey",
                "CREATE UNIQUE INDEX CONCURRENTLY t_x_key ON public.t USING btree (x)",
                "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE USING INDEX t_x_key",
            ]
        );
        assert_eq!(planned.non_transactional, [0, 2]);
        let units: Vec<bool> = planned.units().iter().map(|u| u.transactional).collect();
        assert_eq!(units, [false, true, false, true]);
    }

    #[test]
    fn a_key_built_concurrently_waits_for_its_column_and_goes_before_the_foreign_keys_using_it() {
        let mut fk = fkey("c", "tx", "t");
        fk.definition = "FOREIGN KEY (tx) REFERENCES public.t(x)".into();
        fk.references = Some(pgmig_model::References {
            table: QualName::new("public", "t"),
            columns: vec!["x".into()],
            constraint: Some("t_x_key".into()),
        });
        // In the diff's order, the foreign key first: the graph orders them.
        let changes = [
            add_constraint("c", fk),
            add_constraint("t", unique_key("t")),
            Change::AddColumn { table: QualName::new("public", "t"), column: col("x", None) },
        ];
        let planned = plan_with(&changes, &concurrent()).expect("plan");
        assert_eq!(
            planned.statements,
            [
                "ALTER TABLE public.t ADD COLUMN x integer",
                "CREATE UNIQUE INDEX CONCURRENTLY t_x_key ON public.t USING btree (x)",
                "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE USING INDEX t_x_key",
                "ALTER TABLE public.c ADD CONSTRAINT c_tx_fkey FOREIGN KEY (tx) REFERENCES public.t(x)",
            ]
        );
        // The column commits before the build; the attach and the foreign key share a transaction.
        assert_eq!(
            planned.units(),
            [
                Unit { statements: 0..1, transactional: true },
                Unit { statements: 1..2, transactional: false },
                Unit { statements: 2..4, transactional: true },
            ]
        );
    }

    #[test]
    fn a_deferrable_key_is_attached_deferrable() {
        let mut c = unique_key("t");
        c.definition = "UNIQUE (x) DEFERRABLE INITIALLY DEFERRED".into();
        (c.deferrable, c.deferred) = (true, true);
        let planned = plan_with(&[add_constraint("t", c.clone())], &concurrent()).expect("plan");
        assert_eq!(
            planned.statements[1],
            "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE USING INDEX t_x_key DEFERRABLE INITIALLY DEFERRED"
        );
        c.deferred = false;
        let planned = plan_with(&[add_constraint("t", c)], &concurrent()).expect("plan");
        assert_eq!(planned.statements[1], "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE USING INDEX t_x_key DEFERRABLE");
    }

    #[test]
    fn keys_using_index_cannot_take_keep_the_plain_add_constraint() {
        let plain = |change: Change| {
            let planned = plan_with(&[change], &concurrent()).expect("plan");
            assert!(planned.non_transactional.is_empty(), "{:?}", planned.statements);
            assert_eq!(planned.statements.len(), 1, "{:?}", planned.statements);
        };
        // On a partitioned table.
        plain(add_constraint("t", Constraint { on_partitioned_table: true, ..unique_key("t") }));
        // Not a b-tree (a key `WITHOUT OVERLAPS` is a GiST index).
        plain(add_constraint(
            "t",
            Constraint {
                index_definition: Some("CREATE UNIQUE INDEX t_x_key ON public.t USING gist (x, during)".into()),
                ..unique_key("t")
            },
        ));
        // No index definition (older JSON), or one under another name.
        plain(add_constraint("t", Constraint { index_definition: None, ..unique_key("t") }));
        plain(add_constraint(
            "t",
            Constraint { index_definition: Some("CREATE UNIQUE INDEX other ON public.t USING btree (x)".into()), ..unique_key("t") },
        ));
        // Not a key.
        let mut exclusion = constraint("t_x_excl", pgmig_model::ConstraintKind::Exclusion, "EXCLUDE USING gist (x WITH =)", &["x"]);
        exclusion.index_definition = None;
        plain(add_constraint("t", exclusion));
        // In a tablespace, which `pg_get_indexdef` leaves out.
        plain(add_constraint("t", Constraint { definition: "UNIQUE (x) USING INDEX TABLESPACE ts".into(), ..unique_key("t") }));
    }

    #[test]
    fn a_key_replacing_another_on_the_same_table_stays_in_one_transaction() {
        let old = Constraint { definition: "UNIQUE (y)".into(), ..unique_key("t") };
        let changes =
            [Change::DropConstraint { table: QualName::new("public", "t"), constraint: old }, add_constraint("t", unique_key("t"))];
        let planned = plan_with(&changes, &concurrent()).expect("plan");
        assert!(planned.non_transactional.is_empty(), "{:?}", planned.statements);
        assert_eq!(planned.statements.last().map(String::as_str), Some("ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE (x)"));
    }

    #[test]
    fn a_key_on_a_table_created_in_the_plan_stays_plain() {
        // The diff writes a new table's keys in its `CREATE TABLE`; a caller may pass them apart.
        let t = table("public", "t", vec![col("x", None)]);
        let planned = plan_with(&[Change::CreateTable(t), add_constraint("t", unique_key("t"))], &concurrent()).expect("plan");
        assert!(planned.non_transactional.is_empty(), "{:?}", planned.statements);
        assert_eq!(planned.statements[1], "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE (x)");
    }

    #[test]
    fn the_index_of_a_key_built_concurrently_is_skipped_with_the_key_not_with_create_index() {
        use pgmig_diff::ChangeKind;
        let changes = [add_constraint("t", unique_key("t")), create_index("t", "t_idx")];
        let (kept, skipped) = skip(&changes, &[ChangeKind::CreateIndex].into(), &concurrent());
        assert_eq!(skipped.len(), 1);
        assert_eq!(
            plan_with(&kept, &concurrent()).expect("plan").statements,
            [
                "CREATE UNIQUE INDEX CONCURRENTLY t_x_key ON public.t USING btree (x)",
                "ALTER TABLE public.t ADD CONSTRAINT t_x_key UNIQUE USING INDEX t_x_key",
            ]
        );
        let (kept, skipped) = skip(&changes, &[ChangeKind::AddConstraint].into(), &concurrent());
        assert_eq!(skipped.iter().map(ToString::to_string).collect::<Vec<_>>(), ["add_constraint public.t.t_x_key (policy)"]);
        assert_eq!(
            plan_with(&kept, &concurrent()).expect("plan").statements,
            ["CREATE INDEX CONCURRENTLY t_idx ON public.t USING btree (x)"]
        );
    }

    #[test]
    fn a_quoted_key_name_is_matched_and_kept() {
        let mut c = constraint("T key", pgmig_model::ConstraintKind::Unique, "UNIQUE (x)", &["x"]);
        c.index_definition = Some("CREATE UNIQUE INDEX \"T key\" ON public.t USING btree (x) INCLUDE (y) NULLS NOT DISTINCT".into());
        let opts = PlanOptions { unqualified_schema: Some("public".into()), concurrent_indexes: true, ..PlanOptions::default() };
        let planned = plan_with(&[add_constraint("t", c)], &opts).expect("plan");
        assert_eq!(
            planned.statements,
            [
                "CREATE UNIQUE INDEX CONCURRENTLY \"T key\" ON public.t USING btree (x) INCLUDE (y) NULLS NOT DISTINCT",
                "ALTER TABLE t ADD CONSTRAINT \"T key\" UNIQUE USING INDEX \"T key\"",
            ]
        );
    }

    #[test]
    fn units_split_a_plan_at_commit_points_and_statements_outside_a_transaction() {
        let plan = |n: usize, commit_after: Option<usize>, non_transactional: &[usize]| Plan {
            statements: (0..n).map(|i| format!("SELECT {i}")).collect(),
            commit_after,
            non_transactional: non_transactional.to_vec(),
        };
        let tx = |r: std::ops::Range<usize>| Unit { statements: r, transactional: true };
        let outside = |i: usize| Unit { statements: i..i + 1, transactional: false };
        assert_eq!(plan(0, None, &[]).units(), []);
        assert_eq!(plan(3, None, &[]).units(), [tx(0..3)]);
        assert_eq!(plan(3, Some(0), &[]).units(), [tx(0..1), tx(1..3)]);
        // A commit point after the last statement ends nothing.
        assert_eq!(plan(3, Some(2), &[]).units(), [tx(0..3)]);
        assert_eq!(plan(1, Some(0), &[]).units(), [tx(0..1)]);
        assert_eq!(plan(3, None, &[1]).units(), [tx(0..1), outside(1), tx(2..3)]);
        assert_eq!(plan(3, None, &[0, 2]).units(), [outside(0), tx(1..2), outside(2)]);
        assert_eq!(plan(2, None, &[0, 1]).units(), [outside(0), outside(1)]);
        // An enum commit point next to a statement outside a transaction is one boundary.
        assert_eq!(plan(3, Some(0), &[1]).units(), [tx(0..1), outside(1), tx(2..3)]);
        assert_eq!(plan(4, Some(1), &[2]).units(), [tx(0..2), outside(2), tx(3..4)]);
        // Units cover every statement once, in order.
        for p in [plan(5, Some(1), &[2, 4]), plan(5, None, &[0]), plan(5, Some(3), &[])] {
            let covered: Vec<usize> = p.units().into_iter().flat_map(|u| u.statements).collect();
            assert_eq!(covered, (0..5).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_plan_with_a_concurrent_index_after_enum_values_has_three_kinds_of_boundary() {
        let add = add("ok", EnumPosition::End);
        let planned = plan_with(&[add, create_index("t", "t_idx"), Change::CreateSchema("s".into())], &concurrent()).expect("plan");
        assert_eq!(planned.commit_after, Some(0));
        assert_eq!(planned.units().len(), 3, "{:?}", planned.units());
        let sql = planned.to_sql();
        assert!(sql.contains(COMMIT_MARKER) && sql.contains(NO_TRANSACTION_MARKER), "{sql}");
    }

    #[test]
    fn to_sql_marks_statements_outside_a_transaction() {
        let plan = Plan {
            statements: vec![
                "ALTER TABLE t ADD x int".into(),
                "CREATE INDEX CONCURRENTLY i ON t (x)".into(),
                "COMMENT ON INDEX i IS 'c'".into(),
            ],
            commit_after: None,
            non_transactional: vec![1],
        };
        assert_eq!(
            plan.to_sql(),
            "ALTER TABLE t ADD x int;\n\
             -- Runs outside a transaction: Postgres refuses it inside one.\n\
             -- pgmig:notx\n\
             CREATE INDEX CONCURRENTLY i ON t (x);\n\
             COMMENT ON INDEX i IS 'c';\n"
        );
        let units = plan.units();
        assert_eq!(plan.unit_sql(&units[1]), "CREATE INDEX CONCURRENTLY i ON t (x);\n");
        // Without a statement outside a transaction the script is as it always was.
        let plain = Plan { non_transactional: vec![], ..plan };
        assert_eq!(plain.to_sql(), "ALTER TABLE t ADD x int;\nCREATE INDEX CONCURRENTLY i ON t (x);\nCOMMENT ON INDEX i IS 'c';\n");
    }

    #[test]
    fn plan_json_without_the_new_field_reads_as_before_and_is_left_out_when_empty() {
        let old = r#"{"statements":["CREATE INDEX i ON t (x)"],"commit_after":null}"#;
        let plan: Plan = serde_json::from_str(old).expect("old shape");
        assert_eq!(plan, Plan { statements: vec!["CREATE INDEX i ON t (x)".into()], commit_after: None, non_transactional: vec![] });
        assert_eq!(serde_json::to_string(&plan).expect("serialize"), old);

        let with = Plan { non_transactional: vec![0], ..plan };
        let json = serde_json::to_string(&with).expect("serialize");
        assert_eq!(json, r#"{"statements":["CREATE INDEX i ON t (x)"],"commit_after":null,"non_transactional":[0]}"#);
        assert_eq!(serde_json::from_str::<Plan>(&json).expect("deserialize"), with);
    }

    #[test]
    fn plan_json_with_a_bad_non_transactional_list_is_refused() {
        let read = |list: &str| {
            serde_json::from_str::<Plan>(&format!(r#"{{"statements":["a","b","c"],"commit_after":null,"non_transactional":{list}}}"#))
        };
        assert_eq!(read("[0,2]").expect("ascending").non_transactional, [0, 2]);
        assert!(read("[]").expect("empty").non_transactional.is_empty());
        for (list, message) in [
            ("[2,0]", "strictly ascending statement indexes, but 2 is followed by 0"),
            ("[1,1]", "strictly ascending statement indexes, but 1 is followed by 1"),
            ("[3]", "statement 3 does not exist (the plan has 3)"),
            ("[0,7]", "statement 7 does not exist"),
        ] {
            let err = read(list).expect_err(list).to_string();
            assert!(err.contains(message), "{list}: {err}");
        }
        // Other fields are still checked, and unknown ones refused.
        assert!(serde_json::from_str::<Plan>(r#"{"statements":[],"commit_after":null,"x":1}"#).is_err());
        // A plan built in code with the list out of order still lays out its units by statement.
        let plan = Plan { statements: vec!["a".into(), "b".into(), "c".into()], non_transactional: vec![2, 0], ..Plan::default() };
        let outside: Vec<bool> = plan.units().iter().map(|u| u.transactional).collect();
        assert_eq!(outside, [false, true, false]);
    }

    #[test]
    fn plan_options_json_without_the_new_field_reads_as_before() {
        let opts: PlanOptions = serde_json::from_str(r#"{"unqualified_schema":"app"}"#).expect("old shape");
        assert_eq!(opts, PlanOptions { unqualified_schema: Some("app".into()), ..PlanOptions::default() });
        let opts: PlanOptions = serde_json::from_str(r#"{"unqualified_schema":null,"concurrent_indexes":true}"#).expect("new shape");
        assert!(opts.concurrent_indexes);
    }

    fn partition(name: &str, parent: &str, bound: &str) -> Table {
        let of = pgmig_model::PartitionOf { parent: QualName::new("public", parent), bound: bound.into() };
        Table { partition_of: Some(of), ..table("public", name, vec![]) }
    }

    fn partitioned(name: &str) -> Table {
        Table { partition_key: Some("RANGE (id)".into()), ..table("public", name, vec![col("id", None)]) }
    }

    #[test]
    fn partitions_are_created_after_their_parent_and_dropped_before_it() {
        let (p, c, d) = (partitioned("p"), partition("c", "p", "FOR VALUES FROM (1) TO (10)"), partition("d", "p", "DEFAULT"));
        // A sub-partition: a partition that is partitioned.
        let s = Table { partition_key: Some("LIST (id)".into()), ..partition("s", "p", "FOR VALUES FROM (10) TO (20)") };
        let s1 = partition("s1", "s", "FOR VALUES IN (10)");
        let created = sql(&[
            Change::CreateTable(s1.clone()),
            Change::CreateTable(c.clone()),
            Change::CreateTable(s.clone()),
            Change::CreateTable(p.clone()),
            Change::CreateTable(d.clone()),
        ]);
        assert_eq!(
            created,
            [
                "CREATE TABLE public.p (\n    id integer\n) PARTITION BY RANGE (id)",
                "CREATE TABLE public.c PARTITION OF public.p FOR VALUES FROM (1) TO (10)",
                "CREATE TABLE public.s PARTITION OF public.p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (id)",
                "CREATE TABLE public.s1 PARTITION OF public.s FOR VALUES IN (10)",
                "CREATE TABLE public.d PARTITION OF public.p DEFAULT",
            ]
        );
        let dropped = sql(&[drop_table(p), drop_table(s), drop_table(c), drop_table(s1)]);
        assert_eq!(dropped, ["DROP TABLE public.c", "DROP TABLE public.s1", "DROP TABLE public.s", "DROP TABLE public.p"]);
    }

    #[test]
    fn a_partition_with_constraints_of_its_own_writes_only_those() {
        let c = Table {
            constraints: vec![constraint("c_v", ConstraintKind::Check, "CHECK ((v > 0))", &["v"])],
            ..partition("c", "p", "DEFAULT")
        };
        assert_eq!(
            sql(&[Change::CreateTable(c)]),
            ["CREATE TABLE public.c PARTITION OF public.p (\n    CONSTRAINT c_v CHECK ((v > 0))\n) DEFAULT"]
        );
    }

    #[test]
    fn a_rebound_partition_is_detached_before_it_is_attached_again() {
        let (c, p) = (QualName::new("public", "c"), QualName::new("public", "p"));
        let columns = vec!["id".to_owned()];
        let of = pgmig_model::PartitionOf { parent: p.clone(), bound: "FOR VALUES FROM (1) TO (20)".into() };
        let changes = [
            Change::AttachPartition { table: c.clone(), partition_of: of, columns: columns.clone() },
            Change::DetachPartition { table: c, parent: p, columns },
        ];
        assert_eq!(
            sql(&changes),
            [
                "ALTER TABLE public.p DETACH PARTITION public.c",
                "ALTER TABLE public.p ATTACH PARTITION public.c FOR VALUES FROM (1) TO (20)"
            ]
        );
        // The two stay together under a policy.
        let kinds = [pgmig_diff::ChangeKind::DetachPartition].into_iter().collect();
        let (kept, skipped) = skip(&changes, &kinds, &PlanOptions::default());
        assert!(kept.is_empty(), "{kept:?}");
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn a_rebound_partition_is_detached_after_its_parent_changes_and_a_detached_one_before() {
        let (c, p) = (QualName::new("public", "c"), QualName::new("public", "p"));
        let columns = vec!["id".to_owned(), "v".to_owned()];
        let of = pgmig_model::PartitionOf { parent: p.clone(), bound: "FOR VALUES FROM (1) TO (20)".into() };
        let detach = Change::DetachPartition { table: c.clone(), parent: p.clone(), columns: columns.clone() };
        let attach = Change::AttachPartition { table: c.clone(), partition_of: of, columns: [columns.clone(), vec!["w".into()]].concat() };
        // The parent's changes, which reach the partition while it is attached.
        let parent = [
            Change::AddColumn { table: p.clone(), column: col("w", None) },
            Change::SetNotNull { table: p.clone(), column: "v".into() },
            Change::DropColumn { table: p.clone(), column: "x".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
            Change::AddConstraint { table: p.clone(), constraint: constraint("p_v", ConstraintKind::Check, "CHECK ((v > 0))", &["v"]) },
            Change::DropIndex { table: p.clone(), index: index("p", "p_v_idx", "(v)", &["v"]) },
        ];
        let parent_sql = sql(&parent);
        let rebound = sql(&[vec![attach.clone(), detach.clone()], parent.to_vec()].concat());
        assert_eq!(rebound[..parent.len()], parent_sql[..], "{rebound:?}");
        assert_eq!(
            rebound[parent.len()..],
            [
                "ALTER TABLE public.p DETACH PARTITION public.c",
                "ALTER TABLE public.p ATTACH PARTITION public.c FOR VALUES FROM (1) TO (20)"
            ]
        );
        // Detached to stand alone, the partition leaves before the parent changes.
        let detached = sql(&[parent.to_vec(), vec![detach]].concat());
        assert_eq!(detached[0], "ALTER TABLE public.p DETACH PARTITION public.c");
        assert_eq!(detached[1..], parent_sql[..]);
        // Attached, it joins once the parent has changed and it has its parent's checks.
        let check =
            Change::AddConstraint { table: c.clone(), constraint: constraint("p_v", ConstraintKind::Check, "CHECK ((v > 0))", &["v"]) };
        let attached = sql(&[vec![attach, check], parent.to_vec()].concat());
        assert_eq!(attached.last().map(String::as_str), Some("ALTER TABLE public.p ATTACH PARTITION public.c FOR VALUES FROM (1) TO (20)"));
        assert!(attached.contains(&"ALTER TABLE public.c ADD CONSTRAINT p_v CHECK ((v > 0))".to_owned()), "{attached:?}");
    }

    #[test]
    fn a_table_is_attached_after_its_columns_match_and_detached_before_they_change() {
        let (c, p) = (QualName::new("public", "c"), QualName::new("public", "p"));
        let columns = vec!["id".to_owned()];
        let of = pgmig_model::PartitionOf { parent: p.clone(), bound: "DEFAULT".into() };
        let attach = Change::AttachPartition { table: c.clone(), partition_of: of, columns: columns.clone() };
        let add = Change::AddColumn { table: c.clone(), column: col("id", None) };
        let not_null = Change::SetNotNull { table: c.clone(), column: "id".into() };
        assert_eq!(
            sql(&[attach, not_null.clone(), add]),
            [
                "ALTER TABLE public.c ADD COLUMN id integer",
                "ALTER TABLE public.c ALTER COLUMN id SET NOT NULL",
                "ALTER TABLE public.p ATTACH PARTITION public.c DEFAULT"
            ]
        );
        let detach = Change::DetachPartition { table: c.clone(), parent: p.clone(), columns };
        let drop_parent_column =
            Change::DropColumn { table: p.clone(), column: "id".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] };
        let drop_parent = drop_table(partitioned("p"));
        assert_eq!(
            sql(&[drop_parent, drop_parent_column, not_null, detach]),
            [
                "ALTER TABLE public.p DETACH PARTITION public.c",
                "ALTER TABLE public.c ALTER COLUMN id SET NOT NULL",
                "ALTER TABLE public.p DROP COLUMN id",
                "DROP TABLE public.p"
            ]
        );
    }

    #[test]
    fn unsupported_partition_changes_fail_the_plan_naming_the_table() {
        let change =
            Change::UnsupportedPartitionChange { name: QualName::new("public", "p"), detail: "the partitioning cannot change".into() };
        assert_eq!(plan(&[change]), Err(Error::Unsupported(vec!["table public.p: the partitioning cannot change".into()])));
    }

    #[test]
    fn an_index_on_a_partitioned_table_is_built_on_its_partitions_too() {
        let i = pgmig_model::Index {
            definition: "CREATE INDEX p_x_idx ON ONLY public.p USING btree (x)".into(),
            on_partitioned_table: true,
            ..index("p", "p_x_idx", "(x)", &["x"])
        };
        assert_eq!(
            sql(&[Change::CreateIndex { table: QualName::new("public", "p"), index: i }]),
            ["CREATE INDEX p_x_idx ON public.p USING btree (x)"]
        );
    }

    fn one(change: Change, if_exists: bool, cascade: bool) -> String {
        let opts = PlanOptions { if_exists, cascade, ..PlanOptions::default() };
        let mut statements = plan_with(&[change], &opts).expect("plan").statements;
        assert_eq!(statements.len(), 1, "{statements:?}");
        statements.remove(0)
    }

    #[test]
    fn if_exists_and_cascade_apply_to_the_statements_that_take_them() {
        let t = QualName::new("public", "t");
        let pair = pgmig_model::Composite { name: QualName::new("public", "pair"), attributes: vec![], comment: None };
        let unique = constraint("t_x_key", pgmig_model::ConstraintKind::Unique, "UNIQUE (x)", &["x"]);
        let cases: Vec<(Change, [&str; 4])> = vec![
            (Change::CreateSchema("app".into()), ["CREATE SCHEMA app", "CREATE SCHEMA IF NOT EXISTS app", "", ""]),
            (
                Change::DropSchema("app".into()),
                ["DROP SCHEMA app", "DROP SCHEMA IF EXISTS app", "DROP SCHEMA app CASCADE", "DROP SCHEMA IF EXISTS app CASCADE"],
            ),
            (
                Change::CreateTable(table("public", "t", vec![])),
                ["CREATE TABLE public.t ()", "CREATE TABLE IF NOT EXISTS public.t ()", "", ""],
            ),
            (
                Change::CreateTable(partition("p", "t", "FOR VALUES IN (1)")),
                [
                    "CREATE TABLE public.p PARTITION OF public.t FOR VALUES IN (1)",
                    "CREATE TABLE IF NOT EXISTS public.p PARTITION OF public.t FOR VALUES IN (1)",
                    "",
                    "",
                ],
            ),
            (
                drop_table(table("public", "t", vec![])),
                [
                    "DROP TABLE public.t",
                    "DROP TABLE IF EXISTS public.t",
                    "DROP TABLE public.t CASCADE",
                    "DROP TABLE IF EXISTS public.t CASCADE",
                ],
            ),
            (
                Change::CreateSequence(sequence("s", "bigint", opts("bigint", 1))),
                ["CREATE SEQUENCE public.s", "CREATE SEQUENCE IF NOT EXISTS public.s", "", ""],
            ),
            (
                Change::DropSequence(sequence("s", "bigint", opts("bigint", 1))),
                [
                    "DROP SEQUENCE public.s",
                    "DROP SEQUENCE IF EXISTS public.s",
                    "DROP SEQUENCE public.s CASCADE",
                    "DROP SEQUENCE IF EXISTS public.s CASCADE",
                ],
            ),
            (
                Change::DropEnum(mood(&["a"])),
                [
                    "DROP TYPE app.mood",
                    "DROP TYPE IF EXISTS app.mood",
                    "DROP TYPE app.mood CASCADE",
                    "DROP TYPE IF EXISTS app.mood CASCADE",
                ],
            ),
            (
                Change::DropCompositeType(pair.clone()),
                [
                    "DROP TYPE public.pair",
                    "DROP TYPE IF EXISTS public.pair",
                    "DROP TYPE public.pair CASCADE",
                    "DROP TYPE IF EXISTS public.pair CASCADE",
                ],
            ),
            (
                Change::DropDomain(domain("public", "d", "integer", None)),
                [
                    "DROP DOMAIN public.d",
                    "DROP DOMAIN IF EXISTS public.d",
                    "DROP DOMAIN public.d CASCADE",
                    "DROP DOMAIN IF EXISTS public.d CASCADE",
                ],
            ),
            (
                Change::AddColumn { table: t.clone(), column: col("x", None) },
                ["ALTER TABLE public.t ADD COLUMN x integer", "ALTER TABLE public.t ADD COLUMN IF NOT EXISTS x integer", "", ""],
            ),
            (
                Change::DropColumn { table: t.clone(), column: "x".into(), type_ref: None, sequences: vec![], owned_sequences: vec![] },
                [
                    "ALTER TABLE public.t DROP COLUMN x",
                    "ALTER TABLE public.t DROP COLUMN IF EXISTS x",
                    "ALTER TABLE public.t DROP COLUMN x CASCADE",
                    "ALTER TABLE public.t DROP COLUMN IF EXISTS x CASCADE",
                ],
            ),
            (
                Change::DropConstraint { table: t.clone(), constraint: unique.clone() },
                [
                    "ALTER TABLE public.t DROP CONSTRAINT t_x_key",
                    "ALTER TABLE public.t DROP CONSTRAINT IF EXISTS t_x_key",
                    "ALTER TABLE public.t DROP CONSTRAINT t_x_key CASCADE",
                    "ALTER TABLE public.t DROP CONSTRAINT IF EXISTS t_x_key CASCADE",
                ],
            ),
            (
                Change::DropAttribute { composite: pair.name.clone(), attribute: "b".into(), type_ref: None },
                [
                    "ALTER TYPE public.pair DROP ATTRIBUTE b",
                    "ALTER TYPE public.pair DROP ATTRIBUTE IF EXISTS b",
                    "ALTER TYPE public.pair DROP ATTRIBUTE b",
                    "ALTER TYPE public.pair DROP ATTRIBUTE IF EXISTS b",
                ],
            ),
            (
                create_index("t", "t_i"),
                ["CREATE INDEX t_i ON public.t USING btree (x)", "CREATE INDEX IF NOT EXISTS t_i ON public.t USING btree (x)", "", ""],
            ),
            (
                drop_index("t", "t_i"),
                [
                    "DROP INDEX public.t_i",
                    "DROP INDEX IF EXISTS public.t_i",
                    "DROP INDEX public.t_i CASCADE",
                    "DROP INDEX IF EXISTS public.t_i CASCADE",
                ],
            ),
        ];
        for (change, [plain, ie, casc, both]) in cases {
            assert_eq!(one(change.clone(), false, false), plain);
            assert_eq!(one(change.clone(), true, false), ie);
            // Create statements have no CASCADE: the same as without it.
            assert_eq!(one(change.clone(), false, true), if casc.is_empty() { plain } else { casc });
            assert_eq!(one(change, true, true), if both.is_empty() { ie } else { both });
        }
    }

    #[test]
    fn statements_without_an_if_not_exists_form_are_left_alone() {
        let t = QualName::new("public", "t");
        let unchanged = [
            Change::CreateEnum(mood(&["a"])),
            Change::CreateDomain(domain("public", "d", "integer", None)),
            Change::AddConstraint {
                table: t,
                constraint: constraint("t_x_check", pgmig_model::ConstraintKind::Check, "CHECK ((x > 0))", &["x"]),
            },
        ];
        for change in unchanged {
            assert_eq!(one(change.clone(), true, true), one(change, false, false));
        }
    }

    #[test]
    fn concurrent_indexes_take_if_exists_but_not_cascade() {
        let opts = PlanOptions { concurrent_indexes: true, if_exists: true, cascade: true, ..PlanOptions::default() };
        let planned = plan_with(&[create_index("t", "t_new"), drop_index("t", "t_old")], &opts).expect("plan");
        assert_eq!(
            planned.statements,
            ["DROP INDEX CONCURRENTLY IF EXISTS public.t_old", "CREATE INDEX CONCURRENTLY IF NOT EXISTS t_new ON public.t USING btree (x)"]
        );
        // A plain drop in the transaction (the table goes too) keeps CASCADE.
        let t = table("public", "t", vec![]);
        let with_table = Table { indexes: vec![index("t", "t_i", "(x)", &["x"])], ..t };
        let planned = plan_with(&[drop_table(with_table)], &opts).expect("plan");
        assert_eq!(planned.statements, ["DROP TABLE IF EXISTS public.t CASCADE"]);
    }

    #[test]
    fn plan_options_json_reads_if_exists_and_cascade() {
        let opts: PlanOptions = serde_json::from_str(r#"{"unqualified_schema":null,"if_exists":true,"cascade":true}"#).expect("shape");
        assert!(opts.if_exists && opts.cascade && !opts.concurrent_indexes);
    }
}
