# pgmig

A Postgres schema migration tool in Rust: desired state in plain SQL, planned and applied
migrations, lint and drift detection, with a library surface for Go, Python and TypeScript
packages. Design: [DESIGN.md](DESIGN.md). Features: [docs/FEATURES.md](docs/FEATURES.md).
Milestones: [docs/ROADMAP.md](docs/ROADMAP.md).

## Working rules

- New dependencies go in root `[workspace.dependencies]` with `default-features = false` and only
  the features used; crates reference them with `{ workspace = true }`.
- One version of every crate (`deny.toml`, enforced by the CI `deny` job). Versions shared with
  pgmem (`postgres`, `tokio`) must match pgmem's. No OpenSSL and no system libraries: Rust crates only, except
  build-time C compiled by `cc` (`ring`, for TLS).
- pgmem comes from crates.io. Bump it in root `Cargo.toml` when pgmig needs a newer release.
- Every behaviour is checked against real Postgres: tests run on pgmem by default and on real
  Postgres when `PGMIG_TEST_DSN` is set (CI job `postgres`, versions 15 and later). A test that
  passes on pgmem but fails on Postgres is a bug in the test or in pgmig.
- When pgmem disagrees with Postgres, file an issue on kaeawc/pgmem and reference it
  (`kaeawc/pgmem#N`) wherever pgmig works around it or skips a check. Don't write around a pgmem gap
  silently.
- Never parse desired-state SQL into the model in pgmig: execute it in the shadow database and
  inspect the catalog. Model fields hold what the server prints (`format_type`, `pg_get_expr`, ...).
- The diff corpus (`corpus/<area>/<case>/{from.sql,to.sql,plan.sql}`) is the planner's spec. Every
  case must round-trip: apply the plan to `from` and nothing is left to change. `make bless`
  rewrites `plan.sql`; review the diff. Cases failing only on pgmem go in
  `corpus/pgmem_known_failures.txt` with their pgmem issue; the corpus test fails when that list is
  out of date in either direction.
- The core is synchronous. No async in library crates.
- Libraries use `thiserror` enums; the CLI uses `anyhow`. Keep Postgres SQLSTATEs and messages
  verbatim.
- `unsafe` is denied workspace-wide; only the C ABI crate for language packages may allow it.
- Integration tests live in one binary per crate (`crates/<x>/tests/it/`, one module per area).
  Unit tests stay in-module. Doctests are disabled.
- Never name other migration products in code, docs, issues or pull requests; describe features
  on their own terms.

## Build & validate

```bash
cargo c                             # check --all-targets
cargo t                             # nextest run (or: cargo test)
cargo lint                          # clippy --all-targets -D warnings
cargo fmt --all
cargo deny check
make bless                          # rewrite corpus plan.sql files
PGMIG_TEST_DSN="host=127.0.0.1 port=5432 user=postgres" cargo t   # against real Postgres
```

After any change: `cargo c && cargo lint && cargo t`.

## Releasing

Add user-visible changes to `CHANGELOG.md` under `## [Unreleased]`. `scripts/release.sh X.Y.Z`
opens a release PR: it bumps the workspace version (and the `=X.Y.Z` pins on the internal path
dependencies in root `Cargo.toml`) and moves the Unreleased entries under the new version. Merging
it runs `.github/workflows/release.yml`: `pgmig` binaries for Linux (gnu + musl) / macOS / Windows,
`scripts/publish.sh` to crates.io via trusted publishing (environment `crates-io`; skips crates
already published, so reruns are safe), then the tag and GitHub release with the changelog section
as notes. Every crate is published and they share one version. Run the workflow by hand for a dry
run.

## Map

- `crates/pgmig-model` — schema model (`Model`, `Table`, `Column`, `QualName`), `quote_ident`.
- `crates/pgmig-conn` — synchronous `Conn` over the `postgres` crate (simple protocol, text values,
  SQLSTATE errors). Used for targets and for pgmem.
- `crates/pgmig-shadow` — `Shadow`: embedded pgmem or an empty real Postgres, wiped after use.
- `crates/pgmig-config` — `pgmig.toml`: environments, `${VAR}` interpolation, `config validate`.
- `crates/pgmig-sql` — `split`: SQL text → statements with positions and `-- pgmig:` directives.
- `crates/pgmig-inspect` — catalog queries → `Model`.
- `crates/pgmig-diff` — `diff(a, b) -> Vec<Change>`.
- `crates/pgmig-plan` — `plan(changes) -> Plan` (ordered SQL).
- `crates/pgmig` — library surface (`schema_diff`, `schema_inspect`, `Source`, `ShadowSpec`) and the
  integration tests, including the corpus runner (`tests/it/corpus.rs`).
- `apps/pgmig` — the `pgmig` CLI (package `pgmig-cli`).
- `corpus/` — diff corpus and `pgmem_known_failures.txt`.
