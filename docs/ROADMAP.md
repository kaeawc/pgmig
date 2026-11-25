# pgmig roadmap

Milestones in order. Each one ends with something usable and is tested on pgmem and on real
Postgres. Feature IDs refer to [FEATURES.md](FEATURES.md); architecture is in [../DESIGN.md](../DESIGN.md).

## M0: Workspace

- Cargo workspace (`crates/`, `apps/pgmig`, `bindings/`), toolchain, rustfmt, clippy, deny, nextest
  and bacon configs, Makefile, `AGENTS.md`, Apache-2.0 `LICENSE`.
- CI: fmt, clippy, deny, tests on pgmem, tests on real Postgres (version matrix), SHA-pinned actions.
- pgmem as a git dependency pinned to a revision, behind `pgmig-conn`.
- Corpus test harness with the round-trip check and the pgmem known-failures ratchet.

**Done when:** an empty `pgmig` CLI builds, and one corpus case (create a table) passes on both
backends in CI.

## M1: Inspect, diff, declarative apply for core objects

- `pgmig-conn`, `pgmig-shadow`, `pgmig-sql`: shadow database on pgmem or a DSN, file loading with
  `-- pgmig:import`, errors with file and line. (SRC-1..4, SRC-8, SRC-9, SHD-1..3)
- `pgmig-model`, `pgmig-inspect`: schemas, tables, columns, constraints, indexes, sequences, enums,
  extensions, comments. (INS-1..3, INS-5, INS-9)
- `pgmig-diff`, `pgmig-plan`: dependency ordering, `USING` casts. (DIF-1, DIF-2, DIF-4, DIF-16)
- `schema inspect`, `schema diff`, `schema validate`, `schema apply [--dry-run]`. (DEC-1, DEC-2)
- JSON output and exit codes; Rust library surface. (OUT-1, LIB-1)

**Done when:** a typical app schema (tables, FKs, indexes, enums, identity columns) can be edited
and applied repeatedly, and every plan round-trips on both backends.

## M2: Versioned migrations and configuration

- Migration directory, sum file, `migrate new/diff/hash/validate/edit/rm/ls/show/rebase`. (SRC-5,
  MIG-1..6)
- Revisions table, advisory lock, transaction modes, partial-apply resume, execution order,
  baseline, status/set, hooks, JSON report. (MIG-7..14, MIG-18, MIG-22)
- Fingerprint. (INS-4)
- `pgmig.toml` with environments and variables. (CFG-1..4)
- Skip policy, `$EDITOR`, auto-approve with review policy, `schema clean`. (DIF-7, DIF-17, DEC-3, DEC-6)

**Done when:** a project can be run entirely through `pgmig migrate` in CI and production, and a
non-transactional migration that fails halfway resumes correctly after a fix.

## M3: All schema objects, safe plans, drift

- Domains, composite types, views, materialized views, functions, procedures, triggers, partitions.
- Drop and recreate of dependents; enum value removal; function overloads. (DIF-3, DIF-5, DIF-6)
- Lock-safe rewrites: concurrent indexes, `NOT VALID` + `VALIDATE`, lock-safe NOT NULL, `USING
  INDEX`, partitioned indexes, `WITH NO DATA`, `IF EXISTS`/`CASCADE`. (DIF-8..14)
- Shadow fork. (SHD-4)
- `drift check`. (DRF-1..3)

**Done when:** the corpus covers every object kind in FEATURES §13 marked M1 or M3, and lock-safe
plans are verified against real Postgres with the lock table test.

## M4: Lint, security objects

- Lint engine, configuration, suppression, reporters; all M4 rules in FEATURES §7. Schema lint. (DEC-7)
- Row-level security policies, roles, grants, default privileges; ownership modes; sensitive
  statements. (SRC-10, SRC-11)

**Done when:** `pgmig migrate lint --git-base main` runs in a pull request and reports annotations.

## M5: Go package and advanced workflows

- `pgmig-ffi` and `bindings/go` with prebuilt static libraries per platform. (LIB-2)
- Saved plans with fingerprint check. (DEC-4)
- Computed down migrations, checkpoints, pre-migration assertions. (MIG-15..17)
- Rename hints, external schema commands, merged sources. (DIF-15, SRC-6, SRC-7)
- First tagged release with prebuilt CLI binaries (needs the release tooling decision in DESIGN.md).

## M6: Testing framework and the rest of lint

- Schema and migration tests. (TST-1..3)
- Remaining lint rules (restrictive changes, integer key overflow, column alignment, dynamic SQL,
  custom statement rules). Pre-apply policy checks. (DEC-5)
- `schema fmt`, other directory layouts and import, event triggers, GitHub Action. (INS-6, MIG-19, OUT-2)

## M7: More languages and fleet operations

- Python (PyO3) and TypeScript (napi-rs) packages. (LIB-3, LIB-4)
- Reference data sync, schema statistics, ER diagrams. (SRC-12, INS-7, INS-8)
- Remote migration directories, multi-database fan-out, drift webhook. (MIG-20, MIG-21, DRF-4)

## M8: Long tail

- Plan profiling on real Postgres, language server. (TST-4, OUT-3)
- Range types, collations, statistics objects, casts, aggregates, foreign data wrappers, publications.

## pgmem dependencies

Gaps in pgmem found while surveying it for pgmig. pgmig features that need them either wait for the
fix or document using a real Postgres shadow in the meantime.

| pgmem issue | Needed by |
|---|---|
| [kaeawc/pgmem#145](https://github.com/kaeawc/pgmem/issues/145) Stable in-process embedding API | M0 |
| [kaeawc/pgmem#164](https://github.com/kaeawc/pgmem/issues/164) `pg_get_keywords()` (keyword quoting test runs on Postgres only until then) | M0 |
| [kaeawc/pgmem#162](https://github.com/kaeawc/pgmem/issues/162) LICENSE file and crate publishing | first crates.io release |
| [kaeawc/pgmem#147](https://github.com/kaeawc/pgmem/issues/147) `pg_constraint.connamespace` is always `public` | M1 |
| [kaeawc/pgmem#148](https://github.com/kaeawc/pgmem/issues/148) `pg_get_indexdef` drops `INCLUDE` | M1 |
| [kaeawc/pgmem#152](https://github.com/kaeawc/pgmem/issues/152) Store `reloptions` | M1 |
| [kaeawc/pgmem#153](https://github.com/kaeawc/pgmem/issues/153) `ALTER TABLE` column and table attributes | M1 |
| [kaeawc/pgmem#160](https://github.com/kaeawc/pgmem/issues/160) Full-text search types and functions | M1 (common in app schemas) |
| [kaeawc/pgmem#146](https://github.com/kaeawc/pgmem/issues/146) Cheap database fork | M3 |
| [kaeawc/pgmem#155](https://github.com/kaeawc/pgmem/issues/155) `pg_depend` completeness and `pg_rewrite` | M3 |
| [kaeawc/pgmem#149](https://github.com/kaeawc/pgmem/issues/149) Function attributes and `ALTER FUNCTION` | M3 |
| [kaeawc/pgmem#150](https://github.com/kaeawc/pgmem/issues/150) `pg_get_triggerdef` schema-qualifies the function | M3 |
| [kaeawc/pgmem#154](https://github.com/kaeawc/pgmem/issues/154) Partitioned index DDL (`ON ONLY`, `ATTACH PARTITION`) | M3 |
| [kaeawc/pgmem#158](https://github.com/kaeawc/pgmem/issues/158) `ALTER EXTENSION` | M3 |
| [kaeawc/pgmem#151](https://github.com/kaeawc/pgmem/issues/151) Row-level security and policies | M4 |
| [kaeawc/pgmem#156](https://github.com/kaeawc/pgmem/issues/156) Column-level privileges | M4 |
| [kaeawc/pgmem#157](https://github.com/kaeawc/pgmem/issues/157) `regrole`, `pg_default_acl`, `pg_policies` | M4 |
| [kaeawc/pgmem#159](https://github.com/kaeawc/pgmem/issues/159) Remaining schema object types | M6, M8 |
| [kaeawc/pgmem#161](https://github.com/kaeawc/pgmem/issues/161) DDL lock modes in `pg_locks` | optional (M8) |
| [kaeawc/pgmem#141](https://github.com/kaeawc/pgmem/issues/141) Table inheritance, [kaeawc/pgmem#116](https://github.com/kaeawc/pgmem/issues/116) range types | M8 |
