# pgmig feature list

What pgmig will do, grouped by area. Each feature has an ID (used in issues and the roadmap) and the
milestone it lands in (see [ROADMAP.md](ROADMAP.md)). Nothing here is implemented yet.

Everything is open source under Apache-2.0. Nothing needs a hosted service: anything that would
usually live in a hosted service (plan storage, drift history, migration registries) is a file in
your repository or a URL you control.

Directives inside SQL files are comments with the prefix `-- pgmig:`.

## 1. Schema sources

| ID | Feature | Milestone |
|---|---|---|
| SRC-1 | Desired state from a single `.sql` file. | M1 |
| SRC-2 | Desired state from a directory of `.sql` files, loaded in lexical order. | M1 |
| SRC-3 | `-- pgmig:import <path>` at the top of a file to declare that it depends on other files or directories; each file is loaded once; cycles are an error. | M1 |
| SRC-4 | Desired state from a live database URL (database-to-database diff, baselining). | M1 |
| SRC-5 | Desired state from a migration directory (replay every file, then inspect). | M2 |
| SRC-6 | Desired state from an external command whose stdout is SQL (for ORM schema dumps). | M5 |
| SRC-7 | Merge several sources into one desired state. | M5 |
| SRC-8 | Include/exclude globs over `schema.object[.column]`, optionally filtered by object kind; applied identically to inspect, diff, lint and drift. | M1 |
| SRC-9 | Scope: one schema (unqualified output) or the whole database (qualified output, schemas managed). | M1 |
| SRC-10 | Ownership modes: manage objects only (default), or also roles, grants and default privileges. | M4 |
| SRC-11 | `-- pgmig:sensitive` marks statements whose text must never be logged or echoed (for example role passwords). | M4 |
| SRC-12 | Reference data as code: declare rows for small lookup tables and sync them (insert-only, upsert, or full sync with deletes). | M7 |

## 2. Shadow database

The shadow database is where pgmig executes SQL to learn what it means. Desired-state SQL is never
parsed into a model by pgmig itself; it is executed and then read back from the catalog, so Postgres
semantics (defaults, type aliases, normalized expressions) come for free.

| ID | Feature | Milestone |
|---|---|---|
| SHD-1 | Embedded in-memory Postgres (pgmem) as the default shadow database: no Docker, no server binary, starts in milliseconds. | M1 |
| SHD-2 | A real Postgres DSN as the shadow database, for extensions or features pgmem does not support. | M1 |
| SHD-3 | The shadow database is wiped before each use; pgmig refuses to use a non-empty real database as a shadow. | M1 |
| SHD-4 | Fast fork of a loaded schema to try several candidate plans without replaying DDL. | M3 |

## 3. Inspection

| ID | Feature | Milestone |
|---|---|---|
| INS-1 | Inspect a live database into pgmig's schema model using catalog queries. The same queries run against pgmem and real Postgres. | M1 |
| INS-2 | Print a schema as deterministic, dependency-ordered SQL that recreates it. | M1 |
| INS-3 | Print a schema as JSON. | M1 |
| INS-4 | Canonical schema fingerprint: a hash of the model that is identical whether computed from a live database or a shadow replay. | M2 |
| INS-5 | `schema validate`: load the desired state into the shadow database and report errors with file and line. | M1 |
| INS-6 | `schema fmt`: rewrite schema files in a canonical SQL format. | M6 |
| INS-7 | Schema statistics (object counts, table and index sizes) in OpenMetrics format. | M7 |
| INS-8 | Entity-relationship diagram as Mermaid or Graphviz. | M7 |
| INS-9 | Exclude objects owned by extensions from inspection and diff. | M1 |

## 4. Diff and plan

| ID | Feature | Milestone |
|---|---|---|
| DIF-1 | Diff any two states (file, directory, migration directory, live database) and print the plan as SQL. | M1 |
| DIF-2 | Dependency-aware ordering: create types before tables, tables before foreign keys, views and functions after what they reference; drop in reverse. | M1 |
| DIF-3 | Drop and recreate dependent views, functions, policies and triggers when an object they depend on must change. | M3 |
| DIF-4 | Column type changes with an explicit `USING` cast when there is no implicit cast. | M1 |
| DIF-5 | Enum changes: add values at a position and rename values; removing a value swaps in a new type. | M3 |
| DIF-6 | Function and procedure diffs by signature (overloads), with `CREATE OR REPLACE` when allowed and drop/create when the return type or arguments change. | M3 |
| DIF-7 | Policy: skip chosen kinds of change (for example never drop tables or columns). | M2 |
| DIF-8 | Policy: create and drop indexes `CONCURRENTLY`, and put those statements in their own non-transactional unit. | M3 |
| DIF-9 | Policy: add constraints as `NOT VALID` then `VALIDATE` in a separate step. | M3 |
| DIF-10 | Policy: lock-safe `SET NOT NULL` via a validated `CHECK (col IS NOT NULL)` constraint. | M3 |
| DIF-11 | Policy: add a primary key or unique constraint by building the index concurrently and then attaching it with `USING INDEX`. | M3 |
| DIF-12 | Policy: build indexes on partitioned tables with `ON ONLY`, then build each partition's index concurrently and attach it. | M3 |
| DIF-13 | Policy: create materialized views `WITH NO DATA`. | M3 |
| DIF-14 | Policy: `IF [NOT] EXISTS` and `CASCADE` on generated statements. | M3 |
| DIF-15 | Rename detection: offer a rename instead of drop and add, interactively or through a `-- pgmig:rename` hint. Never guessed silently. | M5 |
| DIF-16 | Qualified or unqualified names in output; pretty-printed SQL. | M1 |
| DIF-17 | Open the planned SQL in `$EDITOR` before applying or writing it. | M2 |

## 5. Declarative workflow

| ID | Feature | Milestone |
|---|---|---|
| DEC-1 | `schema apply`: inspect the target, load the desired state into the shadow database, diff, show the plan, ask for confirmation, execute. | M1 |
| DEC-2 | `--dry-run`: print the plan and run pre-checks without changing anything. | M1 |
| DEC-3 | `--auto-approve`, and a review policy that auto-approves only when lint finds nothing at or above a chosen severity. In a non-interactive shell, anything not approved by policy fails. | M2 |
| DEC-4 | Saved plans: `schema plan` writes a plan file containing SQL plus the from- and to-fingerprints. `schema apply --plan` runs it only if the target's fingerprint still matches, so the reviewed SQL is exactly the executed SQL. | M5 |
| DEC-5 | Pre-apply policy checks: deny or allow rules over the planned statements (for example a cap on statements, or no index builds in a time window). | M6 |
| DEC-6 | `schema clean`: drop every object in a database, with confirmation. For local use. | M2 |
| DEC-7 | Schema lint: rules that judge the desired state itself (naming, missing primary keys, unindexed foreign keys). | M4 |

## 6. Versioned workflow

| ID | Feature | Milestone |
|---|---|---|
| MIG-1 | `migrate diff <name>`: replay the migration directory in the shadow database, diff it against the desired state, write the next timestamped migration file and update the sum file. | M2 |
| MIG-2 | `migrate new <name>`: create an empty migration file. | M2 |
| MIG-3 | Sum file: a directory hash on line one, then one line per file with a hash that chains every earlier file, so any edit or insertion invalidates later lines and two branches adding files always conflict in version control. | M2 |
| MIG-4 | `migrate hash`, `migrate validate` (checksums, plus a replay in the shadow database). | M2 |
| MIG-5 | `migrate edit`, `migrate rm`, `migrate ls`, `migrate show` (all rehash as needed). | M2 |
| MIG-6 | `migrate rebase <version>`: move out-of-order files to the end and rehash, to resolve branch conflicts. | M2 |
| MIG-7 | `migrate apply [n]`: apply pending files; refuse if the directory doesn't match its sum file or the database history doesn't match the directory. | M2 |
| MIG-8 | Revisions table in a configurable schema: version, description, kind (baseline, checkpoint, normal), statements applied and total, timings, error and failing statement, file hash, per-statement hashes, pgmig version. | M2 |
| MIG-9 | Advisory lock during apply, with a timeout, a configurable lock name, and an opt-out. | M2 |
| MIG-10 | Transaction modes: one transaction per file, one for all files, or none; per-file override with `-- pgmig:txmode none`. | M2 |
| MIG-11 | Resume after a partial failure in non-transactional mode from the first unapplied statement, verified with per-statement hashes. | M2 |
| MIG-12 | Execution order: strictly linear, linear skipping older files, or non-linear. | M2 |
| MIG-13 | Baseline an existing database: write a baseline file from it, and mark it applied without running it. Allow applying onto a non-empty database when asked. | M2 |
| MIG-14 | `migrate status` (current version, pending files, last error) and `migrate set <version>` (fix up the revisions table by hand). | M2 |
| MIG-15 | Computed down migrations: `migrate down [n] / --to <version>` inspects the database as it is now (even after a partial failure), computes the plan back to the target version's state, and adds safety checks (for example a table must be empty before it is dropped). | M5 |
| MIG-16 | Checkpoints: a file holding the full state at a version, so a fresh database starts from the latest checkpoint instead of replaying everything. | M5 |
| MIG-17 | Pre-migration assertions in a file (`-- pgmig:assert` followed by a boolean `SELECT`), all or one-of, evaluated before the migration runs. | M5 |
| MIG-18 | Session hooks: SQL run after `BEGIN` and before `COMMIT` (for example `SET lock_timeout`). | M2 |
| MIG-19 | Read other directory layouts (up/down pairs, prefixed versions) and `migrate import` to convert them. | M6 |
| MIG-20 | Migration directory from a remote URL (git ref, tarball or OCI artifact). | M7 |
| MIG-21 | Fan-out across many databases or tenants, with staged rollout. | M7 |
| MIG-22 | Machine-readable apply report (JSON): pending, applied, per-statement timings, errors. | M2 |

## 7. Lint

Lint runs on migration files (the last *n*, or everything new since a git base ref) and on declarative
plans. It works on the semantic change each statement makes, using the shadow database, not on text
patterns. Rules can be suppressed per file or per statement with `-- pgmig:nolint [rule...]`; a rule
can be configured as error, warning or off, and can be forced so that it can't be suppressed.
Output: text, JSON, and GitHub annotations.

Rules (ID = `category/name`):

| Rule | Detects | Milestone |
|---|---|---|
| `destructive/drop-schema` | A schema is dropped. | M4 |
| `destructive/drop-table` | A table is dropped. | M4 |
| `destructive/drop-column` | A stored column is dropped. | M4 |
| `compat/rename-table` | A table is renamed (breaks running code). | M4 |
| `compat/rename-column` | A column is renamed. | M4 |
| `compat/narrowed-type` | A column type is narrowed. | M4 |
| `compat/restrictive` | A change rejects reads or writes from the previous app version (new NOT NULL without default, new CHECK). Off by default. | M6 |
| `constraint/drop-foreign-key` | A foreign key is dropped. | M4 |
| `constraint/drop-check` | A check constraint is dropped. | M4 |
| `constraint/drop-primary-key` | A primary key is dropped. | M4 |
| `data/unique-on-existing` | A unique index is added over existing columns (fails on duplicates). | M4 |
| `data/make-unique` | A non-unique index becomes unique. | M4 |
| `data/not-null-no-default` | A NOT NULL column without a default is added to an existing table. | M4 |
| `data/set-not-null` | A nullable column becomes NOT NULL. | M4 |
| `lock/index-not-concurrent` | `CREATE INDEX` on an existing table without `CONCURRENTLY`. | M4 |
| `lock/drop-index-not-concurrent` | `DROP INDEX` without `CONCURRENTLY`. | M4 |
| `lock/concurrent-in-transaction` | A `CONCURRENTLY` statement in a transactional file. | M4 |
| `lock/add-primary-key` | `ADD PRIMARY KEY` building its index under an exclusive lock. | M4 |
| `lock/add-unique` | `ADD UNIQUE` building its index under an exclusive lock. | M4 |
| `lock/partitioned-index` | An index on a partitioned table that blocks writes on every partition. | M4 |
| `lock/add-exclusion` | An exclusion constraint (exclusive lock and full scan). | M4 |
| `lock/create-trigger` | A trigger created on an existing table. | M4 |
| `rewrite/alter-type` | A column type or collation change that rewrites the table (binary-coercible changes are allowed). | M4 |
| `rewrite/volatile-default` | A column added with a volatile default. | M4 |
| `rewrite/stored-generated` | A stored generated column added to an existing table. | M4 |
| `rewrite/identity` | An identity column added to an existing table. | M4 |
| `rewrite/set-logged` | `SET LOGGED` / `SET UNLOGGED`. | M4 |
| `rewrite/access-method` | A table access method change. | M4 |
| `scan/set-not-null` | `SET NOT NULL` scanning the table under an exclusive lock. | M4 |
| `scan/check-not-valid` | `ADD CHECK` without `NOT VALID`. | M4 |
| `scan/foreign-key-not-valid` | `ADD FOREIGN KEY` without `NOT VALID`. | M4 |
| `scan/primary-key-nullable` | A primary key on nullable columns (implies a NOT NULL scan). | M4 |
| `scan/replace-primary-key` | Replacing a primary key. | M4 |
| `tx/mixed-modes` | A file mixes statements that can't share a transaction (`CONCURRENTLY`, `ALTER TYPE ... ADD VALUE` followed by a use of the value, `VACUUM`). | M4 |
| `tx/nested` | `BEGIN`/`COMMIT` inside a file pgmig wraps in a transaction. | M4 |
| `ops/replica-identity` | `REPLICA IDENTITY FULL` or `NOTHING` (logical replication risk). | M4 |
| `ops/autovacuum-off` | Autovacuum disabled on a table. | M4 |
| `ops/int-key-overflow` | A 32-bit integer key or foreign key that should be `bigint`. | M6 |
| `ops/column-alignment` | A new table's column order wastes space to alignment padding. | M6 |
| `naming/*` | Schema, table, column, index, foreign key and check names that don't match configured patterns. | M4 |
| `security/dynamic-sql` | Dynamic SQL built by string concatenation in a function body (injection risk). | M6 |
| `history/non-linear` | A new migration not at the end relative to the base branch, or an edit to an already-merged file. | M4 |
| `statement/*` | User-defined allow/deny rules over statements, with conditions on neighbouring statements (for example require `SET lock_timeout` before DDL). | M6 |

## 8. Drift

| ID | Feature | Milestone |
|---|---|---|
| DRF-1 | `drift check`: compare a live database to what it should be (the migration directory at its applied version, or the desired state) and report the difference as SQL and JSON, with a non-zero exit code on drift. | M3 |
| DRF-2 | Compare two live databases (for example staging and production). | M3 |
| DRF-3 | Check that the revisions table and the actual schema agree, and explain how to recover after a partial failure. | M3 |
| DRF-4 | Optional webhook on drift, for cron or CI. | M7 |

## 9. Testing

| ID | Feature | Milestone |
|---|---|---|
| TST-1 | Schema tests in `.test.sql` files: run SQL against a fresh copy of the desired state and assert results, expected errors, or boolean conditions; table-driven cases; parallel by default. | M6 |
| TST-2 | Migration tests: migrate to a version, seed data, migrate further, assert the data survived. | M6 |
| TST-3 | `--run <regex>` to select tests. | M6 |
| TST-4 | Plan profiling against real Postgres: locks taken, rewrites, and duration per statement, with bounds in tests. | M8 |

## 10. Configuration

| ID | Feature | Milestone |
|---|---|---|
| CFG-1 | `pgmig.toml` with named environments (target URL, shadow, schema sources, migration directory and options, diff policy, lint rules); `--env`, `--config`. | M2 |
| CFG-2 | Environment variable interpolation and `--var key=value`. | M2 |
| CFG-3 | Global settings inherited by environments, with per-environment overrides. | M2 |
| CFG-4 | `config validate`. | M2 |

## 11. Output and integrations

| ID | Feature | Milestone |
|---|---|---|
| OUT-1 | JSON output for every command; stable exit codes. | M1 |
| OUT-2 | GitHub Action: lint with annotations and suggested fixes, then apply. | M6 |
| OUT-3 | Editor integration through a language server: diagnostics from `schema validate` and lint. | M8 |

## 12. Libraries

| ID | Feature | Milestone |
|---|---|---|
| LIB-1 | Rust library: every CLI command is a library call. | M1 |
| LIB-2 | Go module over a C ABI, with prebuilt static libraries per platform. | M5 |
| LIB-3 | Python package (PyO3). | M7 |
| LIB-4 | TypeScript/Node package (napi-rs). | M7 |

## 13. Postgres objects

Every object kind below is in scope. "Model" is when pgmig can inspect and diff it; most are M1
(core tables) or M3 (everything else).

| Object | Notes | Milestone |
|---|---|---|
| Schemas | create, drop, owner, comment | M1 |
| Tables | `UNLOGGED`, storage parameters, row-level security enable/force, replica identity | M1 |
| Columns | type, collation, default, nullability, identity and its sequence options, serial, stored and virtual generated columns, storage and compression. Column order can't be altered; pgmig warns. | M1 |
| Constraints | primary key, unique (`NULLS NOT DISTINCT`, `INCLUDE`), check (`NO INHERIT`, `NOT VALID`), foreign key (match type, actions, deferrable), exclusion, `USING INDEX` | M1 |
| Indexes | every method, expressions, partial, `INCLUDE`, operator classes, collation, ordering, storage parameters | M1 |
| Sequences | type and options, `OWNED BY`; current value is never diffed | M1 |
| Enums | add, position, rename values | M1 |
| Extensions | create, drop, version update, schema | M1 |
| Comments | every object kind | M1 |
| Domains | base type, default, NOT NULL, named checks | M3 |
| Composite types | add, drop, alter attributes | M3 |
| Views | body, `security_barrier`, `security_invoker`, `check_option` | M3 |
| Materialized views | body, `WITH [NO] DATA`, indexes | M3 |
| Functions and procedures | overloads, language, volatility, strictness, security definer, `SET` options, parallel safety, cost and rows, body | M3 |
| Triggers | timing, events, columns, row/statement, `WHEN`, transition tables, constraint triggers, enabled state | M3 |
| Partitions | range, list, hash, sub-partitions, default partition, attach and detach, partition indexes | M3 |
| Row-level security policies | permissive/restrictive, command, roles, `USING`, `WITH CHECK` | M4 |
| Roles | attributes and membership; passwords are never inspected | M4 |
| Grants | per-object and per-column privileges, grant option, default privileges | M4 |
| Event triggers | event, tag filter, function, enabled state | M6 |
| Range types, collations, statistics objects, casts, aggregates, foreign data wrappers, publications | later | M8 |
