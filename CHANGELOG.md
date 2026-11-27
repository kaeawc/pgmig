# Changelog

Notable changes to pgmig. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- `--verify` on `schema apply` and `migrate diff` (or `[diff] verify = true`; `--no-verify`
  overrides): the plan, or the edited SQL, first runs in the shadow database on a copy of the
  current schema, and the result must equal the desired schema (changes the diff policy skips
  aside). A failing statement or a remaining difference aborts before the target is changed or a
  migration written; JSON errors carry code `verify_failed` and a `verify` object. The copy is
  schema only (no rows), so failures that depend on data (`SET NOT NULL` over NULLs, a unique
  index over duplicates, `USING` casts) are not caught. `migrate diff` replays the migration
  directory for it; `schema apply` rebuilds it from what pgmig models (the whole target but the
  revisions schema; extensions, functions and views are not copied). Statements outside any
  schema (`CREATE ROLE`, `ALTER SYSTEM`, ...) in an edited plan or a migration really run, on the
  shadow's server.

- Domains and composite types: inspected (base type, collation, default, `NOT NULL`, named checks
  with `NOT VALID`, attributes, comments) and planned in dependency order with the tables, enums,
  domains and composites that use them: create, drop, `ALTER DOMAIN` defaults, `NOT NULL`, checks
  (`NOT VALID` / `VALIDATE`), `ALTER TYPE` add, drop and retype attributes. A domain's new base
  type is a drop and recreate while nothing uses it; changes Postgres cannot make in place are
  refused with the objects in the way. Filter kinds `domain:` and `composite:`; new skip kinds; the
  JSON model gains `domains` and `composites`.

- `schema clean` warns before cleaning a database that is not on this machine (the target names a
  host other than a Unix socket, `localhost` or a loopback address, and the server does not report
  a loopback address); the JSON response carries `local` and `server_address`.
- Migration files and `pgmig.sum` are written crash-safely: synced temporary files moved into
  place, so a file never appears partly written under its final name. Commands that write into
  the migration directory (`migrate diff`, `new`, `baseline`, `hash`, `edit`, `rm`, `rebase`) lock it (`.pgmig.lock`, safe to ignore in version
  control), so concurrent runs get distinct versions.

- Indexes: inspected with `pg_get_indexdef` (access methods, expressions, partial, covering,
  operator classes, collation, sort order, `NULLS NOT DISTINCT`, storage parameters) and planned as
  drop and create, with comments; indexes owned by constraints are left to the constraints. Skip
  kinds `create_index` and `drop_index`.
- `pgmig schema clean --url <db>` drops every managed object in a database (for local
  development), leaving it equal to a new one: `public` stays with its standard comment, the
  revisions schema is never touched, and extensions are not dropped (tables an extension created,
  such as `spatial_ref_sys`, are not yet told apart: the clean then fails and rolls back, see #18;
  indexes are not modelled, so dropping a column drops indexes on it). It shows the drops and the target, and asks
  for the database name to be typed; `--auto-approve` (required without a terminal), `--dry-run`,
  `--format json`. Filters and `--schema` apply; managed objects that a kept object needs (its
  schema, enum, sequence, referenced table) are kept and reported.
- `--edit` on `schema apply` and `migrate diff` opens the planned SQL in `$VISUAL`/`$EDITOR`; what
  you save is what runs or is written. An emptied plan applies or writes nothing, a failing editor
  aborts, and the temp file is always removed. Needs a terminal; refused with `--format json`.
- Diff policy: `--skip <kind>` (repeatable, on `schema diff`, `schema apply` and `migrate diff`) or
  `[diff] skip` in the config never plans that kind of change (`drop_table`, `drop_column`, ...).
  Changes that need a skipped one are skipped too, and every skipped change is reported: on stderr,
  and as `skipped` (`kind`, `object`, `statement`, `because`) in the JSON responses. `--exit-code`
  ignores skipped changes.
- `pgmig schema fingerprint <source>`: a stable `h1:` hash of the managed schema, the same for a
  live database, SQL files or a migration replay, on pgmem and Postgres 15–18.
- `migrate apply --format json` reports pending files, per-file status and per-statement timings
  (index, line, hash, milliseconds; no SQL text), and a failure carries the partial report.
- Adopting existing databases: `pgmig migrate baseline <name>` writes the live schema as the first
  migration; `migrate apply --baseline <version>` records files up to it as applied on a populated
  database with no history (and runs everything on an empty one), and refuses other populated
  databases unless `--allow-dirty`. `pgmig migrate status` shows the current version, pending files
  and problems with recovery hints; `pgmig migrate set <version>` repairs the history (`--force` for
  failed, partial or later complete rows).
- `pgmig migrate ls`, `show`, `edit`, `rm` and `rebase`: list files (with applied status given
  `--url`), print, edit in `$VISUAL`/`$EDITOR`, remove, or move out-of-order files to the end with new
  versions after a merge, each keeping `pgmig.sum` valid. `rm` and `rebase` refuse applied files
  unless `--force`.
- `pgmig migrate apply --url <target> --dir <dir>`: applies pending migrations under an advisory
  lock (`--lock-name`, `--lock-timeout`, `--no-lock`), in `--tx-mode file` (default), `all` or
  `none` (or `-- pgmig:txmode none` per file), with `--exec-order linear`, `linear-skip` or
  `non-linear`, session hooks (`--before-sql`, `--after-sql`), resume of partly applied files, and
  refusal when the directory or the recorded history does not match. `--dry-run` lists pending files.
- Revisions table library (`pgmig_revisions` in the `pgmig` schema, configurable with
  `[migrations] revisions_schema`): versioned format, per-row hashes that reveal hand edits.
- `pgmig migrate diff <name>` writes the next migration from the desired state (nothing when there
  are no changes) and `pgmig migrate new <name>` writes an empty one; both rehash `pgmig.sum`. A
  directory holding `pgmig.sum` is read as a migration directory wherever a state is accepted.
- `pgmig.toml`: every command's options can come from a config file with top-level defaults and
  `[env.<name>]` sections (`--env`, `--config`), `${VAR}` interpolation and `--var NAME=VALUE`;
  flags win over the selected environment, which wins over the top level. `pgmig config validate`
  checks the file. `schema apply` now shows the target (password redacted) and where it came from.
- Migration directories: `<version>_<description>.sql` files and a `pgmig.sum` whose per-file
  hashes chain, so an edit, insertion or deletion is detected and parallel additions conflict in
  version control. `pgmig migrate hash` writes the sum file; `pgmig migrate validate` checks it and,
  with `--shadow`, replays every migration. Migrations cannot use `-- pgmig:import`.
- SQL files are loaded one statement at a time, and load errors report `file:line:col` with the
  SQLSTATE and server message. Dollar-quoted bodies, quoted strings and identifiers, comments and
  `BEGIN ATOMIC` bodies may contain `;`.
- `pgmig schema validate <source>`: load SQL files into the shadow database and report the first
  error as `file:line:col`; prints nothing on success.
- `-- pgmig:import <path>` above a file's first statement loads that file, or every `.sql` file in
  that directory, first. Paths are relative to the importing file, each file loads once, and an
  import cycle is an error naming the files. Files without imports keep lexical order.
- `-- pgmig:<name> args` directive comments are parsed (new crate `pgmig-sql`).
- `pgmig schema apply --url <target> --to <desired>`: plan, show, confirm and execute. Each batch
  runs in a transaction; a failure rolls it back and names the statement and how many earlier
  batches stayed applied. `--dry-run` prints the plan; without a terminal it needs
  `--auto-approve`.
- `--format json` on `schema inspect`, `diff`, `validate` and `apply`, and documented exit codes:
  0 success, 1 error, 2 usage, 3 when `--exit-code` is given and the plan is not empty.
- Library API with serde types (`pgmig::api`): one request and response per command, `Model`,
  `Change` and `Plan` serializable, structured errors (including apply outcomes), and unknown request
  fields refused. The CLI is a thin formatter over it.
- TLS for target and shadow connections (rustls, no OpenSSL): `sslmode` disable, prefer (the new
  default; falls back to plaintext when the handshake fails), require, verify-ca and verify-full,
  with `sslrootcert` or the bundled Mozilla roots. Building now needs a C compiler (for `ring`).
- `--include` / `--exclude` globs over `[kind:]schema.object[.column]` on `schema diff`, `inspect`
  and `apply`, applied to both sides. `--schema <name>` manages one schema and prints unqualified
  names; `apply` always executes qualified names.
- A column type change with no assignment cast (`text` to `integer`) is planned `USING` an explicit
  cast, with a blocking default dropped before and set again after. Castability is asked of the
  server.
- Table constraints: primary key, unique (`NULLS NOT DISTINCT`, `INCLUDE`), check (`NO INHERIT`),
  foreign key (match type, actions, deferrability) and exclusion, plus their comments. Changes are
  drop and add, except renames and foreign-key deferrability. Foreign keys are added after the
  tables they join, so tables can reference each other, and are dropped and re-added around type
  changes of their columns.
- Sequences, identity columns and serial columns: sequence options and `OWNED BY` (the current
  value is never compared), identity `ALWAYS`/`BY DEFAULT` and options, and serial columns as a
  default plus an owned sequence, so switching between serial and identity plans cleanly.
- Enum types: create, drop, add values at a position, and rename a value (planned only when one
  label changes in place; the statement carries a comment saying stored values change). Removing
  or reordering values is reported as unsupported. `ADD VALUE` is planned first, and the plan marks
  a commit point (`-- commit:` line) before statements that may use a new value.
- Comments on schemas, tables and columns are inspected and planned (`COMMENT ON`), after the
  object they describe.

### Changed

- Replaying a migration directory in the shadow (`migrate diff`, `--verify`, fingerprints and
  other `migrate` sources) runs each file as `migrate apply` does: one transaction per stretch
  between `-- pgmig:commit` points, or statement by statement under `-- pgmig:txmode none`, with
  the same directive checks. A file whose transaction layout fails on a real database now fails
  replay too.

- Literals in plans (comments, enum labels) containing a backslash render as escape strings
  (`E'...'`), so they read back the same whatever `standard_conforming_strings` the target has (#116).

- Plans mark commit points with a `-- pgmig:commit` directive line after the explanatory comment.
- The revisions schema (`pgmig` by default) is never managed by `schema` commands: it is left out
  of inspect, diff and apply on both sides. A project with its own schema named `pgmig` must set
  `[migrations] revisions_schema` to another name.
- Inspection runs with `search_path = pg_catalog`, so names inside server-printed definitions
  (defaults, types, constraint definitions) are always schema-qualified, and the same schema reads
  the same whatever the connection's search path.
- Plans are ordered by a dependency graph: each statement runs after what it references, and drops
  run before what they reference. A table whose column uses another table's row type is now
  created after that table and dropped before it.
- Breaking library changes: `pgmig_plan::plan` returns `Result<Plan, Error>` (an error names a
  dependency cycle); `Change::DropTable` carries the dropped `Table`; `DropColumn` and
  `AlterColumnType` carry the referenced types; `Column` has a new `type_ref` field; `DropTable` and `DropColumn` carry the sequences they drop; `Table` has new `constraints` and `indexes` fields; `AlterColumnType` has a new `using` field; `schema_diff`, `schema_inspect` and `schema_plan_apply` take `&Options`; `Plan` has a new `commit_after` field and `plan` can fail
  with `Error::Unsupported`.

## [0.1.0] - 2026-11-26

### Added

- `pgmig schema diff` and `pgmig schema inspect` for schemas, tables and columns, with desired-state
  SQL executed in embedded pgmem or an empty real Postgres.
- Crates `pgmig` (library), `pgmig-cli` (the `pgmig` binary) and the internal crates `pgmig-model`,
  `pgmig-conn`, `pgmig-shadow`, `pgmig-inspect`, `pgmig-diff`, `pgmig-plan`.
- Prebuilt binaries for Linux (glibc and static musl), macOS and Windows on each GitHub release;
  `cargo binstall pgmig-cli` installs them.

[Unreleased]: https://github.com/kaeawc/pgmig/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kaeawc/pgmig/releases/tag/v0.1.0
