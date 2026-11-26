# pgmig design

pgmig is a Postgres schema migration tool written in Rust: you write the schema you want in plain
SQL, and pgmig plans and runs the migrations to get there. It supports both declarative
(state-based) and versioned (migration directory) workflows, with lint and drift detection. The
core is a Rust library; the CLI and the Go, Python and TypeScript packages are thin layers over it.

Status: design only. Feature IDs below (`DIF-3`, `MIG-11`, …) refer to [docs/FEATURES.md](docs/FEATURES.md);
milestones refer to [docs/ROADMAP.md](docs/ROADMAP.md).

## Goals

- **Correct plans.** The defining property: for any two states A and B, applying `plan(A, B)` to A
  produces a database whose inspected model equals B's. Everything else is secondary.
- **Postgres semantics without reimplementing Postgres.** pgmig never interprets desired-state SQL
  itself. It executes it in a shadow database and reads the catalog back.
- **No Docker.** The default shadow database is pgmem, an in-memory Postgres embedded in the
  process. A real Postgres can be used instead when needed.
- **Safe by default on live systems.** Lock- and rewrite-aware planning and lint, advisory locking,
  resumable non-transactional applies, plans that only run against the state they were reviewed on.
- **Everything as a library.** Every command is a library call with typed input and output, so the
  language packages expose the same surface as the CLI.

## Non-goals

- A custom schema language. Input is plain SQL.
- Databases other than Postgres.
- A hosted service. Anything that would live in a service (saved plans, registries, drift history)
  is a file or a URL the user controls.

## Pipeline

```
 desired-state SQL ──► shadow DB (pgmem) ──► inspect ──► Model B ─┐
                                                                 ├─► diff ──► Changes ──► plan ──► Plan ──► lint
 target database ──────────────────────────► inspect ──► Model A ─┘                                 │
                                                                                                   ▼
                                                              apply (declarative)  or  write migration file (versioned)
```

1. **Load.** Desired-state SQL (file, directory with `-- pgmig:import`, migration directory, or an
   external command) is split into statements and executed in a fresh shadow database.
2. **Inspect.** One inspector reads the catalog of the shadow database and of the target, producing
   two `Model`s. Because both go through the same queries and the same server-side deparsers
   (`pg_get_expr`, `pg_get_indexdef`, `pg_get_constraintdef`, `pg_get_viewdef`,
   `pg_get_functiondef`, `pg_get_triggerdef`, `format_type`), equal schemas produce equal models.
3. **Diff.** `diff(A, B)` compares models object by object and returns a list of `Change`s.
4. **Plan.** The planner turns changes into ordered SQL statements, applies the diff policy (lock-safe
   rewrites, skips, concurrency), and groups statements into transactional and non-transactional units.
5. **Lint.** Rules run over the changes and the planned statements.
6. **Apply / write.** The declarative workflow executes the plan; the versioned workflow writes it as
   the next migration file and updates the sum file.

## Crates

Cargo workspace, `crates/pgmig-*` libraries plus the CLI in `apps/pgmig`, following the layout and
build conventions of rust-build and pgmem (one version per dependency, `default-features = false`,
one integration-test binary per crate, injected clock, doctests off).

| Crate | Responsibility |
|---|---|
| `pgmig-model` | The schema model: object kinds, identities, references; canonical serialization and the schema fingerprint (`INS-4`). No I/O. |
| `pgmig-sql` | Statement splitting (dollar quotes, `-- pgmig:` directives), SQL quoting and printing helpers, source positions for diagnostics. |
| `pgmig-conn` | A synchronous `Conn` (`batch`, `query` returning text values, SQLSTATE errors) over the `postgres` crate, used for real servers and for pgmem alike. TLS (rustls) comes in M1. |
| `pgmig-shadow` | The shadow database: create, wipe, load SQL files with errors mapped back to file and line, and fork (`SHD-1..4`). |
| `pgmig-inspect` | Catalog queries → `Model`, per Postgres major version, scoped by include/exclude filters. |
| `pgmig-diff` | `Model × Model → Vec<Change>`. |
| `pgmig-plan` | Dependency graph, statement generation, diff policy, lock knowledge, transaction grouping → `Plan`. |
| `pgmig-lint` | Rules, configuration, suppression directives, reporters. |
| `pgmig-migrate` | Migration directory, sum file, revisions table, apply engine, down migrations, checkpoints. |
| `pgmig-config` | `pgmig.toml` parsing, environments, variables. |
| `pgmig` | Facade: one function per command with serde input/output types. Everything the CLI and bindings call. |
| `pgmig-ffi` | C ABI over `pgmig` for the Go package (and anything else that speaks C). |
| `apps/pgmig` | The CLI (clap). Formatting and prompts only; no logic. |
| `bindings/go` | Go module (cgo) over `pgmig-ffi`. Python (PyO3) and TypeScript (napi-rs) come later in `bindings/`. |

### Why synchronous

Migration work is a handful of sequential round trips; async buys nothing and complicates every
binding (no runtime to carry across the C ABI, no `Send` puzzles in PyO3/napi). The core is
synchronous. The `postgres` crate (which pgmem also uses, at the same version) gives a synchronous
client for both real servers and pgmem.

## Shadow database and pgmem

pgmem is a pure-Rust, in-memory, Postgres-compatible database. pgmig starts it in-process through
its public `pgmem::Server` API (a server on a loopback port, with its own small runtime thread) and
connects with the same `postgres` client it uses for real servers. One client code path for both
backends means the inspector, planner and tests never know which one they're on. When pgmem offers
a stable embedding API without the wire layer (kaeawc/pgmem#145), `pgmig-conn` can switch to it
without changing its callers.

Rules:

- **Fidelity is tested, not assumed.** Every corpus case and integration test runs on pgmem and, in
  CI, on real Postgres. When they disagree, it's a pgmem issue to file, not a workaround to write in
  pgmig. Workarounds that are unavoidable live in one module (`pgmig-conn::pgmem::quirks`) and each
  cites the pgmem issue it waits on.
- **Fall back to a real server.** If desired-state SQL needs something pgmem lacks (an extension like
  PostGIS, an unsupported statement), the user points `shadow` at a real Postgres DSN. Errors from
  pgmem for unsupported features say so and suggest this.
- **Stable surface.** pgmig uses only pgmem's public `Server` API, never its executor internals.
  The dependency comes from crates.io.

pgmem gaps that block pgmig features are tracked in [docs/ROADMAP.md](docs/ROADMAP.md#pgmem-dependencies).

## Schema model

The model is a set of objects keyed by identity:

```rust
pub enum ObjectId {
    Schema(Name),
    Relation { schema: Name, name: Name },            // table, view, matview, sequence, partition
    Column { rel: RelId, name: Name },
    Constraint { on: ConstraintOwner, name: Name },   // table or domain
    Index { schema: Name, name: Name },
    Type { schema: Name, name: Name },                // enum, domain, composite, range
    Routine { schema: Name, name: Name, args: Vec<TypeName> },  // identity args, so overloads are distinct
    Trigger { rel: RelId, name: Name },
    Policy { rel: RelId, name: Name },
    Extension(Name),
    Role(Name),
    // ...
}
```

- Object bodies hold exactly what the catalog says, in the deparsed form the server prints. Two
  objects are equal when their bodies are equal; there is no client-side "semantic equality" logic
  that could disagree with Postgres.
- Every object lists the objects it references (column types, foreign key targets, view and function
  dependencies, owned-by sequences). The planner's dependency graph is built from these. Where the
  catalog's `pg_depend` is reliable (real Postgres) it is the source; the inspector cross-checks it
  against references it can derive itself.
- **Fingerprint** (`INS-4`): SHA-256 of the model's canonical serialization (objects sorted by
  identity, fields in fixed order, filters applied, server-version-specific noise removed). Saved
  plans, checkpoints and drift checks compare fingerprints.

## Inspector

- One query module per object kind, using only `pg_catalog` (not `information_schema`, which hides
  objects the user doesn't own and loses detail).
- Queries are written for Postgres 15 and later and branch on `server_version_num` where catalogs
  differ.
- Objects belonging to extensions (`pg_depend.deptype = 'e'`) and to system schemas are excluded.
- Include/exclude filters are applied in the queries where possible and on the model otherwise, so
  every command sees the same filtered model.

## Diff

`diff(a, b)` walks both models by identity: in B only → create; in A only → drop; in both with
different bodies → a kind-specific alter. Each object kind has a diff function that knows which
differences can be altered in place and which force a drop and recreate (for example a view whose
column list changes incompatibly, a function whose return type changes, an enum losing a value).

Renames are never inferred. A drop and an add of similar objects stay a drop and an add unless the
user confirms a rename interactively or marks it with `-- pgmig:rename` (`DIF-15`).

## Planner

1. **Expand.** Each change becomes one or more steps. A forced recreate of an object also schedules a
   drop and recreate of everything that depends on it (`DIF-3`).
2. **Order.** Steps are sorted topologically over the dependency graph: creates after what they
   reference, drops before what they reference is dropped. Cycles (for example mutually referencing
   foreign keys) are broken by creating the tables first and adding the constraints afterwards.
3. **Rewrite for safety.** The diff policy turns risky steps into safe sequences (`DIF-8..12`):
   - `CREATE INDEX` → `CREATE INDEX CONCURRENTLY` in its own non-transactional unit;
   - `ADD CONSTRAINT ... CHECK/FOREIGN KEY` → `NOT VALID` then `VALIDATE CONSTRAINT`;
   - `SET NOT NULL` → validated `CHECK (col IS NOT NULL)`, `SET NOT NULL`, drop the check;
   - `ADD PRIMARY KEY / UNIQUE` → concurrent unique index, then `ADD CONSTRAINT ... USING INDEX`;
   - index on a partitioned table → `ON ONLY` on the parent, concurrent index per partition,
     `ALTER INDEX ... ATTACH PARTITION`.
4. **Group.** Statements that can't run in a transaction (`CONCURRENTLY`, `ALTER TYPE ... ADD VALUE`
   followed by a use of the new value, `VACUUM`) are put in their own units, so the apply engine and
   the versioned workflow can run or write them separately.
5. **Verify.** In tests and with `--verify`, the plan is applied to a fork of A's shadow state and the
   result is inspected and compared with B. A mismatch is a planner bug and fails loudly.

### Lock knowledge

pgmem doesn't take or report locks, so lock and rewrite analysis is static: a table in `pgmig-plan`
maps each kind of step to the lock it takes and whether it rewrites or scans the table, per Postgres
major version (for example: non-volatile `ADD COLUMN ... DEFAULT` is metadata-only; `varchar(n)` →
`text` is binary-coercible; `SET NOT NULL` skips the scan when a validated check exists). The table
is tested against real Postgres in CI by running each step in a transaction and reading `pg_locks`
and the relation's `relfilenode` before and after.

## Lint

Rules see two inputs: the semantic changes (from `pgmig-diff`) and the statements (from
`pgmig-sql`). For a migration file, pgmig replays the directory up to the previous file in the
shadow database, then runs the file one statement at a time, inspecting before and after each
statement to know exactly what it changed. With fork support in the shadow database this costs one
replay per lint run, not one per statement.

Rules have IDs like `lock/index-not-concurrent`, a default severity, and a config entry
(`error`/`warn`/`off`, `force`). Suppression: `-- pgmig:nolint [rule ...]` on a file or statement.
Reporters: human text, JSON, GitHub annotations.

## Versioned workflow

### Directory

```
migrations/
  20261125093000_create_users.sql
  20261126101500_add_orders.sql
  pgmig.sum
```

Version = the leading timestamp; description = the rest of the name. `migrate diff <name>` and
`migrate new <name>` write the next file: its version is the UTC time (`YYYYMMDDHHMMSS`), or the
newest file's plus one when the clock does not give a greater one, and the name is sanitized to
`[a-z0-9_]`. Both refuse a directory that does not match its sum file, and rehash it after writing.
A directory holding `pgmig.sum` is also a state wherever one is accepted (`--from`, `--to`,
`SOURCE`): it is checked against the sum file and replayed in version order.

### Sum file

```
h1:<base64 sha256 of the whole directory>
20261125093000_create_users.sql h1:<base64>
20261126101500_add_orders.sql h1:<base64>
```

Each file's hash is SHA-256 over the previous file's hash, its name, a zero byte and its bytes exactly
as on disk (no newline normalization), so changing, renaming or inserting a file changes every line
after it. The directory hash covers the file lines as written, so a hand-edited sum file is caught.
Versions are digits, ordered as numbers; a duplicate version or a `.sql` file not named
`<version>_<description>.sql` is an error. Two branches that each add a file both rewrite the last line, which
guarantees a version-control conflict instead of a silent merge; `migrate rebase` resolves it.

### Revisions table

`pgmig_revisions` in a configurable schema (`[migrations] revisions_schema`, default `pgmig`), one
row per migration file:

| column | type | |
|---|---|---|
| `version` | `text` primary key | |
| `description` | `text not null` | |
| `kind` | `smallint not null` | 0 normal, 1 baseline, 2 checkpoint |
| `applied` | `integer not null` | statements applied |
| `total` | `integer not null` | statements in the file |
| `executed_at` | `timestamptz not null` | when the file started (the server's `now()`) |
| `execution_ms` | `bigint not null` | |
| `error` | `text` | last error, if any |
| `error_stmt` | `text` | statement that failed |
| `hash` | `text not null` | the file's own hash (SHA-256 of its name, a zero byte and its bytes; not chained, so a non-linear insert leaves applied rows valid) |
| `partial_hashes` | `text[] not null` | hash of each applied statement, for resuming |
| `pgmig_version` | `text not null` | |
| `row_hash` | `text not null` | SHA-256 over the row, to catch hand edits |

The table is created on first use, with its schema, and is versioned by its comment:
`pgmig revisions table, format 1`. pgmig refuses a table with a newer format than it knows, and a
table of that name without the comment; an older format is where a later pgmig upgrades the table
in place.

`row_hash` covers every other column except `executed_at` (whose text depends on the session's time
zone and date style), each length-prefixed. Reading the table recomputes it and reports the first
row that does not match as edited by hand, naming its version, so an `UPDATE` that marks a file
applied is caught. It guards against accidents, not against someone who recomputes the hash;
`migrate set` (`MIG-14`) is the supported way to change rows. Values are sent as quoted literals
over the simple protocol, in the caller's transaction.

pgmig never manages the revisions schema: `schema inspect`, `schema diff`, `schema apply` and
`migrate diff` leave it and everything in it out of both sides, as if `--exclude
schema:<revisions_schema>` were always given (an exact name, not a pattern, and no `--include` brings
it back). Otherwise `schema apply` against a migrated database would drop the table.

### Apply engine

1. Take `pg_advisory_lock` (configurable key, timeout, opt-out).
2. Check the sum file, then compare the revisions table with the directory (respecting the execution
   order policy).
3. For each pending file: run session hooks, then its statements in the file's transaction mode,
   updating the revisions row as it goes. In non-transactional mode each statement's hash is
   recorded as it succeeds, so a retry resumes after the last one and refuses if those statements
   changed (`MIG-11`); each statement is marked in flight before it runs, and a retry refuses to
   guess whether a statement left in flight took effect. A `-- pgmig:commit` directive (written by
   `migrate diff` after new enum values) commits the file's transaction there and begins another;
   `--tx-mode all` refuses such a file. Hooks run around each transaction, at session level for a
   file without one. The lock is a session lock: connect directly or through a session-mode
   pooler.
4. Emit a structured report (`MIG-22`).

### Existing databases, status and set

`migrate baseline <name> --url <db>` adopts a database that predates pgmig: it writes `schema
inspect` of it (the managed objects, schema-qualified, never the revisions schema) as the first
file of an empty directory. It refuses a directory that already has files: a fresh database replays
every file in order, so a file recreating the whole schema after them would fail.

`migrate apply` refuses a database with no history (no revisions table, or an empty one) that
already holds objects: a relation (table, view, materialized view, sequence, foreign table,
composite type), enum, domain, range type, function or procedure in a user schema, not owned by an
extension and not in the revisions schema. A schema without such objects, empty or not, does not
count. Running every file onto it would fail half way, or succeed on top of objects nobody meant to
keep. `--baseline <version>` (an exact version or file name, never a prefix, checked in every case)
records every file up to and including it as applied, kind `baseline`, without running it, in one
transaction, then applies the rest; it does so only on a database without history that holds
objects. On a database without history and without objects it is ignored and every file runs, the
baseline file included (the report's `baseline_skipped`); on one that has that version applied it is
ignored; on one with other history it is refused. So a deploy can always pass it: new databases get
the whole schema, the adopted one is baselined once. `--allow-dirty` applies every file anyway.

`migrate status` only reads (no lock): the current version (newest complete row), each file's
status, the pending files, the last error, and each problem `apply` would refuse or stop at (the
directory not matching its sum file, an applied file edited or missing, a row edited by hand, an
out-of-order file, a failure, a statement left in flight), each with how to recover. `--exit-code`
exits 3 when the database is not up to date.

`migrate set <version>` makes the history exactly the files up to and including that version,
running nothing, in one transaction under the apply lock: files without a row get one (kind normal,
every statement applied, the file's hash), rows that are not complete are recorded as complete,
rows edited by hand are rewritten, and rows after the version or without a file are deleted.
Recording a row that is not complete as complete (a failure, a file stopped part way, a statement
left in flight: some statements may not have run), deleting a row that records a complete
migration, and recording an applied file that was edited since, need `--force`; `0` clears the
history. It is the recovery path for a statement left in
flight: check whether it took effect, then finish the file by hand and `set --force` its version, or
undo what it applied and `set` the version before it so `apply` runs the file again.

### Down migrations

`migrate down` doesn't use hand-written down files. It inspects the database as it is now, replays
the directory up to the target version in the shadow database, and plans from the current state to
that one. Because it starts from the real state, it works after a partial failure. Each destructive
step gets a pre-check (a table must be empty before it's dropped, a column must be all NULL) that
runs before anything executes (`MIG-15`).

## Declarative workflow

`schema apply` = inspect target, load desired state, diff, plan, lint, confirm, execute (in units,
with the same advisory lock and hooks as the versioned engine). `schema plan` saves the plan with its
from- and to-fingerprints; `schema apply --plan <file>` re-inspects the target and refuses unless the
fingerprint matches (`DEC-4`).

## Drift

`drift check` computes the expected state (the migration directory replayed to the version recorded
in the revisions table, or the desired-state files) and the live state, and diffs them. The output is
the plan that would bring the live database back to the expected state, plus a non-zero exit code.

## Configuration

`pgmig.toml`, TOML so it needs no new language:

```toml
shadow = "pgmem"                       # or a DSN
to = "schema/"                         # the desired state
exclude = ["public.tmp_*"]

[migrations]
dir = "migrations"

[env.local]
url = "postgres://localhost/app"

[env.prod]
url = "host=db user=app password=${PGPASSWORD}"
schema = "app"
```

Keys are named after the flags they stand for: `url`, `shadow`, `from`, `to` (also the source of
`schema inspect` and `schema validate`), `include`, `exclude`, `schema` (the `--schema` scope),
`format`, `migrations.dir` (`--dir`), and `migrations.revisions_schema` (the schema of the
revisions table; no flag). Top-level keys apply to every environment;
`[env.<name>]`, selected with `--env`, overrides them key by key (arrays replace), and a flag wins
over both. `--dry-run`, `--auto-approve` and `--exit-code` stay on the command line. Unknown keys
are errors, so a typo never silently falls back to a default.

`${VAR}` interpolates `--var VAR=value`, or else the environment variable; `$${` is a literal `${`.
Only the top level and the selected environment are interpolated, so another environment's
variables need not be set. Errors name keys and variables, never values (URLs hold passwords).
The file is `--config <path>`, or `pgmig.toml` in the current directory; parent directories are not
searched. `pgmig config validate` checks it.

Later sections, rejected as unknown keys until they exist:

```toml
[env.local.migrations]
tx_mode = "file"

[diff]
skip = ["drop_table"]
concurrent_index = true

[lint.rules]
"lock/*" = "error"
"naming/table" = { level = "warn", pattern = "^[a-z_]+$" }
```

## Errors

Libraries return `thiserror` enums; the CLI uses `anyhow`. Errors from Postgres or pgmem keep their
SQLSTATE and message verbatim and carry the source file and line when the statement came from a file.

## Language packages

All packages call the `pgmig` facade, whose inputs and outputs are serde types.

- **Go (M5):** `pgmig-ffi` exposes a C ABI: opaque handles for long-lived things (a configured
  project), and functions that take and return JSON for commands. Memory is freed by an exported
  `pgmig_free`. `unsafe` is confined to this crate. The Go module (`bindings/go`) wraps it with
  idiomatic types and links prebuilt static libraries for each supported platform, published with
  each release, so users don't need a Rust toolchain.
- **Python and TypeScript (M7):** PyO3 and napi-rs packages over the same facade, built as wheels and
  npm prebuilt binaries.

## Testing

- **Diff corpus.** `corpus/<area>/<case>/{from.sql,to.sql,plan.sql}`. The test loads `from` and `to`,
  diffs, compares with `plan.sql` (blessable with an env var), then applies the plan to `from` and
  checks the result equals `to` (the round-trip invariant) and that `diff(to, to)` is empty.
- **Two backends.** Every corpus case and integration test runs on pgmem by default and on real
  Postgres when `PGMIG_TEST_DSN` is set. CI runs every supported Postgres major version (15 and later).
- **Known-failures ratchet.** Cases that fail only on pgmem are listed in `corpus/pgmem_known_failures.txt`,
  each with the pgmem issue it waits on. CI fails if the list is out of date in either direction.
- **Apply engine.** Fault injection (fail statement *k*, kill the connection) to test resume and the
  revisions table.
- **Property tests.** Random schemas from a generator, random edits, round-trip invariant.

## Open questions

1. **Release tooling.** Releases use the same hand-written workflow as pgmem
   (`.github/workflows/release.yml`: native runners, crates.io trusted publishing, binaries on the
   GitHub release). The Go package's prebuilt static libraries and later the wheels and npm
   binaries still need a place in it, as do SBOMs, attestations and a Homebrew tap. Decide before M5.
2. **Lint configuration from code.** Whether `pgmig.toml` is enough or rule configuration also needs
   to be settable from the language packages at call time.
3. **Oldest supported Postgres.** Proposed: 15, since 14 reaches end of community support in November 2026.
