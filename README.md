# pgmig

Postgres schema migrations from plain SQL. Write the schema you want; pgmig works out the
migration, checks it, and runs it. No Docker: desired-state SQL runs in
[pgmem](https://github.com/kaeawc/pgmem), an in-memory Postgres embedded in the process, and
pgmig reads the result back from the catalog, so the plan follows Postgres' own semantics.

Status: early. Only tables, columns and schemas are planned so far (milestone M0 in
[docs/ROADMAP.md](docs/ROADMAP.md)). The design is in [DESIGN.md](DESIGN.md) and the full feature list in
[docs/FEATURES.md](docs/FEATURES.md).

## Try it

```bash
cargo run -p pgmig-cli -- schema diff --from postgres://localhost/app --to schema.sql
cargo run -p pgmig-cli -- schema diff --from old.sql --to new.sql
cargo run -p pgmig-cli -- schema inspect postgres://localhost/app
```

`--shadow <url>` runs SQL files in an empty real Postgres database instead of pgmem (for
extensions pgmem doesn't have).

## Layout

| Path | What |
| --- | --- |
| `crates/pgmig-model` | Schema model and identifier quoting. |
| `crates/pgmig-conn` | Synchronous Postgres connection with SQLSTATE errors. |
| `crates/pgmig-shadow` | Shadow database: embedded pgmem or an empty Postgres. |
| `crates/pgmig-inspect` | Catalog → model. |
| `crates/pgmig-diff` | Model × model → changes. |
| `crates/pgmig-plan` | Changes → ordered SQL. |
| `crates/pgmig` | Library surface: one function per command. |
| `apps/pgmig` | The `pgmig` CLI. |
| `corpus/` | Diff corpus: `from.sql`, `to.sql`, expected `plan.sql` per case. |

## Development

See [AGENTS.md](AGENTS.md). Tests run on pgmem by default; set `PGMIG_TEST_DSN` to run them on real
Postgres.

## License

Apache-2.0.
