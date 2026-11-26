# Changelog

Notable changes to pgmig. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

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
- Enum types: create, drop, add values at a position, and rename a value (planned only when one
  label changes in place; the statement carries a comment saying stored values change). Removing
  or reordering values is reported as unsupported. `ADD VALUE` is planned first, and the plan marks
  a commit point (`-- commit:` line) before statements that may use a new value.
- Comments on schemas, tables and columns are inspected and planned (`COMMENT ON`), after the
  object they describe.

### Changed

- Plans are ordered by a dependency graph: each statement runs after what it references, and drops
  run before what they reference. A table whose column uses another table's row type is now
  created after that table and dropped before it.
- Breaking library changes: `pgmig_plan::plan` returns `Result<Plan, Error>` (an error names a
  dependency cycle); `Change::DropTable` carries the dropped `Table`; `DropColumn` and
  `AlterColumnType` carry the referenced types; `Column` has a new `type_ref` field; `Plan` has a new `commit_after` field and `plan` can fail
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
