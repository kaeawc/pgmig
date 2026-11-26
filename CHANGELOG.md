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
- `-- pgmig:<name> args` directive comments are parsed (new crate `pgmig-sql`).

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
