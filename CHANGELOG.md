# Changelog

Notable changes to pgmig. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- Comments on schemas, tables and columns are inspected and planned (`COMMENT ON`), after the
  object they describe.

### Changed

- Plans are ordered by a dependency graph: each statement runs after what it references, and drops
  run before what they reference. A table whose column uses another table's row type is now
  created after that table and dropped before it.
- Breaking library changes: `pgmig_plan::plan` returns `Result<Plan, Error>` (an error names a
  dependency cycle); `Change::DropTable` carries the dropped `Table`; `DropColumn` and
  `AlterColumnType` carry the referenced types; `Column` has a new `type_ref` field.

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
