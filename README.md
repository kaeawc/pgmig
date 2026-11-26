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
cargo run -p pgmig-cli -- schema fingerprint schema.sql   # h1:..., the same for the database it describes
```

Flags can live in `pgmig.toml` under their own names, with `[env.<name>]` sections selected by
`--env` (see [DESIGN.md](DESIGN.md#configuration)); `pgmig config validate` checks the file:

```toml
shadow = "pgmem"
to = "schema/"

[env.local]
url = "postgres://localhost/app"
```

`--shadow <url>` runs SQL files in an empty real Postgres database instead of pgmem (for
extensions pgmem doesn't have).

`--include` / `--exclude` (repeatable) limit what `diff`, `inspect`, `fingerprint` and `apply` manage, by
`[kind:]schema[.object[.column]]` globs (`*`, `?`; kind `schema`, `table`, `enum` or `sequence`):
`--exclude 'public.tmp_*'` leaves those objects alone on both sides. A managed object may reference
an unmanaged one (a foreign key to an excluded table); the plan assumes it exists. `--schema <name>`
manages one schema's objects (not the schema itself); `inspect` and `diff` print their names
unqualified (types, defaults and constraint definitions keep their schema), while `apply` prints
and runs schema-qualified SQL so the target's `search_path` cannot redirect a statement to another
schema. Without it, every schema is managed and names are qualified.
Databases are inspected with `search_path` set to `pg_catalog` alone, so a role's or session's
`search_path` does not change how a schema prints.

`--skip <kind>` (repeatable; on `schema diff`, `schema apply` and `migrate diff`; config: `[diff]
skip = ["drop_table", ...]`, used when no flag is given) never plans that kind of change. Kinds are
the `kind` names of the JSON changes (`drop_table`, `drop_column`, `drop_schema`, `drop_sequence`,
`drop_constraint`, `alter_column_type`, `add_enum_value`, ...). A change that cannot run without a
skipped one is skipped too: a schema whose table is kept, a column or identity whose sequence is
kept (`drop_sequence`), a foreign key over a column keeping its type, a default naming an enum
value not added. Skipped changes are printed on stderr (`skipped drop_schema app (because
drop_table app.t is skipped)`), and since the difference stays, every later diff finds them again.

| `skipped[]` field | Meaning |
| --- | --- |
| `kind` | The change's kind. |
| `object` | What it is about: `app`, `public.t`, `public.t.c`. |
| `statement` | The SQL it would have run (null for an unsupported enum change). |
| `because` | Null when its kind is skipped; otherwise `{kind, object}` of the skipped change it needs. |

The JSON responses of `schema diff`, `schema apply` and `migrate diff` carry `skipped` (empty
without a policy); `changes` and the plan leave those changes out.

`--format json` (on `inspect`, `diff`, `validate` and `apply`) prints the command's response as one
pretty-printed JSON document on stdout and nothing on stderr; a failure prints the error object
(`code`, `message`, `sqlstate`, `file`, `line`, `col`, `apply`) on stdout instead. Bad flags are
still reported as text on stderr. `apply` never asks
in JSON mode: pass `--auto-approve` or `--dry-run`.

### `migrate apply` report

`pgmig migrate apply --format json` prints the report below on success (`target`, then the fields).
A failure prints the usual error object (`code` `migration`, `message`, `sqlstate`, `file`, `line`,
`col`, `migration`) and exits `1`; its `migration` object (`statement_index`, `statement`,
`applied_files`, `outcome`) also holds the same report as `report`, as far as the run got.

| Field | Meaning |
| --- | --- |
| `dry_run` | Nothing was run. |
| `pending` | File names pending when the run started (to run, or beyond `n`), in run order. |
| `files[]` | Every file of the directory, in version order: `file`, `version`, `status`, `tx_mode` (`file`, `all` or `none`: how the file is to run or ran, set for every file this run was to run, i.e. the pending files up to the limit `n`, in a dry run and after a failure too; null for files beyond `n` and for `already_applied`, `skipped` and `baselined` files), `statements` (to run), `resumed_after`, `execution_ms`, `statement_timings[]`. |
| `files[].status` | `already_applied`, `applied` (by this run), `pending`, `skipped`, `baselined`; after a failure also `failed` (a statement of this file, or its own bookkeeping, failed), `rolled_back` (ran in `--tx-mode all`, undone because a later file, `after_sql` or `COMMIT` failed) and `unknown` (ran in `--tx-mode all` and `COMMIT`'s outcome is not known: it may be applied). Files not reached stay `pending`. |
| `files[].statement_timings[]` | The statements this run ran, in order: `index` (0-based), `line` (1-based), `hash` (as in the revisions table), `execution_ms`. After a failure: those before the failing statement. The SQL text is left out, as statements may hold secrets; `migration.statement` has the failing one. |
| `applied`, `statements` | Files applied and statements run by this run. |
| `execution_ms` | The run, hooks and bookkeeping included. Times are whole milliseconds. |
| `baseline_skipped` | `--baseline` was ignored because the database is empty. |

Exit codes: `0` success; `1` error (including `validate` finding errors); `2` usage (bad flags, or `apply` with no way to approve);
`3` with `--exit-code` (`diff`, or `apply --dry-run`) when the plan is not empty (skipped changes
do not count: a diff with only skipped changes exits `0`).

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
