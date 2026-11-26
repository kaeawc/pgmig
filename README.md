# pgmig

Postgres schema migrations from plain SQL. Write the schema you want; pgmig works out the
migration, checks it, and runs it. No Docker: desired-state SQL runs in
[pgmem](https://github.com/kaeawc/pgmem), an in-memory Postgres embedded in the process, and
pgmig reads the result back from the catalog, so the plan follows Postgres' own semantics.

Status: early. Only tables, columns and schemas are planned so far (milestone M0 in
[docs/ROADMAP.md](docs/ROADMAP.md)). The design is in [DESIGN.md](DESIGN.md) and the full feature list in
[docs/FEATURES.md](docs/FEATURES.md).

## Install

```bash
cargo binstall pgmig-cli       # the `pgmig` binary, prebuilt (or: cargo install pgmig-cli)
cargo add pgmig                # the library
```

Prebuilt binaries for Linux (glibc and static musl), macOS and Windows are attached to each
[GitHub release](https://github.com/kaeawc/pgmig/releases).

## Try it

```bash
cargo run -p pgmig-cli -- schema diff --from postgres://localhost/app --to schema.sql
cargo run -p pgmig-cli -- schema diff --from old.sql --to new.sql
cargo run -p pgmig-cli -- schema inspect postgres://localhost/app
```

`--shadow <url>` runs SQL files in an empty real Postgres database instead of pgmem (for
extensions pgmem doesn't have).

`--include` / `--exclude` (repeatable) limit what `diff`, `inspect` and `apply` manage, by
`[kind:]schema[.object[.column]]` globs (`*`, `?`; kind `schema`, `table`, `enum` or `sequence`):
`--exclude 'public.tmp_*'` leaves those objects alone on both sides. A managed object may reference
an unmanaged one (a foreign key to an excluded table); the plan assumes it exists. `--schema <name>`
manages one schema's objects (not the schema itself); `inspect` and `diff` print their names
unqualified (types, defaults and constraint definitions keep their schema), while `apply` prints
and runs schema-qualified SQL so the target's `search_path` cannot redirect a statement to another
schema. Without it, every schema is managed and names are qualified.
Databases are inspected with `search_path` set to `pg_catalog` alone, so a role's or session's
`search_path` does not change how a schema prints.

`--format json` (on `inspect`, `diff`, `validate` and `apply`) prints the command's response as one
pretty-printed JSON document on stdout and nothing on stderr; a failure prints the error object
(`code`, `message`, `sqlstate`, `file`, `line`, `col`, `apply`) on stdout instead. Bad flags are
still reported as text on stderr. `apply` never asks
in JSON mode: pass `--auto-approve` or `--dry-run`.

Exit codes: `0` success; `1` error (including `validate` finding errors); `2` usage (bad flags, or `apply` with no way to approve);
`3` with `--exit-code` (`diff`, or `apply --dry-run`) when the plan is not empty.

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
