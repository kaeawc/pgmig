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

`--concurrent-indexes` (same three commands; config: `[diff] concurrent_indexes = true`;
`--no-concurrent-indexes` overrides the config) creates and drops indexes with `CREATE [UNIQUE] INDEX
CONCURRENTLY` and `DROP INDEX CONCURRENTLY`, so writes to the table are not blocked. Postgres refuses
these statements inside a transaction, so a plan becomes an ordered list of *units*: runs of statements
that share a transaction, and each concurrent statement on its own outside one (the plan's
`non_transactional` JSON field lists their statement indexes; the text of a plan marks each with
`-- pgmig:notx`). Indexes on a table the plan creates or drops, indexes a constraint owns and indexes
on partitioned tables keep the plain statement. `schema apply` commits each unit before the next, so a
failure leaves the earlier units applied; a failed `CONCURRENTLY` build can leave an `INVALID` index,
and the error names it and says to remove it with `DROP INDEX CONCURRENTLY`. `migrate diff` writes a
plan with such statements as one file per unit with consecutive versions (`<name>.sql`,
`<name>_2.sql`, ...), a concurrent statement's file starting with `-- pgmig:txmode none`, all hashed
into the sum file together; its JSON response lists them in `files` (`file` and `version` are the
first). Such a plan cannot be combined with `--edit`. `-- pgmig:notx` belongs to the text of a plan
(`schema diff` output, `--edit`); a migration file holding it is refused, so save such a plan with
`migrate diff` rather than from `schema diff`.

`--if-exists` and `--cascade` (on `schema diff`, `schema apply`, `schema plan` and `migrate diff`;
config: `[diff] if_exists = true` and `[diff] cascade = true`; `--no-if-exists` and `--no-cascade`
override the config) change the statements a plan writes, and are off by default. With
`--if-exists`, `CREATE SCHEMA`, `CREATE TABLE` (also `PARTITION OF`), `CREATE SEQUENCE`, `CREATE
INDEX` (also `CONCURRENTLY`) and `ALTER TABLE ... ADD COLUMN` get `IF NOT EXISTS`, and `DROP
SCHEMA`/`TABLE`/`SEQUENCE`/`INDEX`/`TYPE`/`DOMAIN`, `ALTER TABLE ... DROP COLUMN`, `DROP CONSTRAINT`
and `ALTER TYPE ... DROP ATTRIBUTE` get `IF EXISTS`, so a plan that was partly applied can run again.
Postgres has no `IF NOT EXISTS` for `CREATE TYPE` (enums and composite types), `CREATE DOMAIN` or `ADD
CONSTRAINT`, so those stay as they are, and so does every statement whose effect does not depend on
whether the object exists. Note that `IF NOT EXISTS` skips an object that exists with a different
definition (including an `INVALID` index a failed `CONCURRENTLY` build left), without comparing
them. With `--cascade`, `DROP SCHEMA`/`TABLE`/`SEQUENCE`/`INDEX`/`TYPE`/`DOMAIN`, `DROP COLUMN` and
`DROP CONSTRAINT` get `CASCADE`. **`CASCADE` makes Postgres drop everything that depends on the
object, including views, functions and other objects pgmig does not manage or that `--include`,
`--exclude` and `--skip` leave out, and the plan does not list them.** Use it only when that is what
you want. `DROP INDEX CONCURRENTLY` has no `CASCADE` form and keeps the plain statement.

`--edit` (on `schema apply` and `migrate diff`) opens the planned SQL in `$VISUAL`, else `$EDITOR`,
else `vi` (split on whitespace, run without a shell) on a private temp file, removed afterwards.
What you save is what runs or is written, with no new diff: drop statements, add your own, or keep
one `-- pgmig:commit` line as the plan's commit point. `schema apply` then shows the edited plan and
asks as usual; with no statements left it applies nothing. `migrate diff` writes the saved text
as-is once it splits and its directives are well placed; with no statements left it writes
nothing. An editor that exits non-zero aborts both. `--edit` needs a terminal and is refused with
`--format json` (usage error, exit `2`). The edited text is not checked against `--skip`: a statement
the skip policy left out runs if you add it back.

Pre-migration assertions: in a migration file, a `-- pgmig:assert` line directly above a statement
makes it an assertion, and `-- pgmig:assert any` makes it part of the file's one-of group. Each is
a single `SELECT` (or `WITH`) returning one row of one boolean column, and all of them come before
the file's first other statement (`migrate diff` never writes them; add them by hand, e.g. with
`migrate edit`). Before `migrate apply` runs a pending file, and again each time a run starts it
(a resume included), it evaluates them on the target in a read-only transaction: every plain
assertion must be true and, if the group exists, at least one `any` assertion; NULL counts as false.
Otherwise the run stops with error code `assertion_failed`, naming the assertion and its value;
nothing of that file runs and no revisions row is written, while earlier files of the run stay
applied (`--tx-mode all` rolls the whole run back). Assertions are not migration statements: they
are not counted in a revisions row, and the file's hash still covers them. Replay in the shadow
database (`migrate diff`, drift checks, `--verify`, fingerprints) and `--dry-run`, `status` and
`--baseline` do not evaluate them, as the shadow has no data.

`--format json` (on `inspect`, `diff`, `validate` and `apply`) prints the command's response as one
pretty-printed JSON document on stdout and nothing on stderr; a failure prints the error object
(`code`, `message`, `sqlstate`, `file`, `line`, `col`, `apply`) on stdout instead. Bad flags are
still reported as text on stderr. `apply` never asks
in JSON mode: pass `--auto-approve` or `--dry-run`.

### Saved plans: `schema plan` and `schema apply --plan`

`pgmig schema plan --url <db> --to <desired> --out plan.json` plans what `schema apply` would do
(same `--shadow`, `--include`, `--exclude`, `--schema`, `--skip`, `--concurrent-indexes`,
`--if-exists`, `--cascade` and `--verify`, and the same config settings) and saves it as a plan file; without `--out` the file
goes to stdout. The database is only read. A plan with no statements is saved too.

The file is pretty-printed JSON: `format` (`1`; another number is refused), `pgmig_version`, `from`
(the `h1:` fingerprint of the database's managed objects when planned, as `schema fingerprint`
prints it), `to` (the fingerprint of the desired state's managed objects), `skipped` (changes
`--skip` left out, when there are any), `options` (filters, schema mode, revisions schema, skip
policy, `concurrent_indexes`, `if_exists`, `cascade`: the scope both fingerprints are relative to), `plan` (the statements,
commit point and statements outside a transaction: what runs) and `sql` (the plan as a script, for
reviewing).

`pgmig schema apply --url <db> --plan plan.json` runs it only if the database still matches. It
reads the file, fingerprints the database's managed objects under the file's own options and, when
that is not `from`, stops with a `plan_stale` error naming both fingerprints and applies nothing;
a change outside the planned scope (another schema, an excluded table) does not make a plan stale,
and is not checked either: a plan that needs an object outside the scope (a foreign key to an
excluded table) can still fail part way when that object changed. `options` itself is not covered
by these checks, and a plan made by another version of pgmig is refused (its fingerprints or SQL may
differ) with a note saying to plan again with this version.
It also refuses a file whose `sql` is not the script of its `plan` (`saved_plan`), so the SQL that
was reviewed is the SQL that runs. Otherwise it shows the SQL and asks as `schema apply` does
(`--auto-approve`, `--dry-run`, `--exit-code`, `--format json`, no prompt without a terminal), runs
the plan's units (a statement outside a transaction on its own, as planned), then fingerprints the
database again: when it is not `to` the changes stay applied and the command fails with
`plan_diverged` ("applied, but the result does not match the planned state") so that CI notices.
That last check is skipped when `skipped` is not empty, since the result is then expected to differ.
`--plan` cannot be combined with `--to`, `--shadow`, `--include`, `--exclude`, `--schema`, `--skip`,
`--concurrent-indexes`, `--if-exists`, `--cascade`, `--verify` or `--edit` (usage error, exit `2`): the file carries its scope
and was verified when planned. With `--format json` the response has `target`, `from`, `to`, `plan`,
`sql`, `applied`, `report` and `fingerprint` (the database's after applying).

pgmig takes no lock: between the check (made again right before the first statement) and the end
of the apply, another session can still change the schema. Use it to keep a reviewed plan from
running against a database that has moved on, not as mutual exclusion. Library: `pgmig::SavedPlan`,
`api::schema_plan`, `api::apply_saved`.

### `schema clean`

`pgmig schema clean --url <db>` (or `url` in `pgmig.toml`) drops every managed object in a
database, for local development: afterwards it equals a new database. It prints the drops and the
target (password redacted, and where it came from) and asks for the database name to be typed;
`--auto-approve` skips the question (needed without a terminal: otherwise usage error, exit `2`),
`--dry-run` only prints. `public` stays, with its standard comment. `--include`, `--exclude` and
`--schema` limit what is dropped (`--schema app` empties `app` and keeps it); an unmanaged object
stays with the managed objects it needs (its schema, an enum a column uses, a sequence a default
uses, a table a foreign key references), each reported as `kept` (on stderr; in JSON, `kept`
with `object` and `because`). The revisions schema (`pgmig`) is never dropped, so drop it yourself
for a full reset. Extensions, and objects pgmig does not model, are not dropped; one that depends
on a dropped object makes the plan fail and roll back. With `--format json` the response has
`target`, `database`, `plan`, `changes`, `kept`, `sql`, `applied` and `report`.

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

### `drift check`

`pgmig drift check --url <db> --dir migrations/` compares a live database with the migration
directory at the version its revisions table records: every file recorded as applied (baselines
included) is replayed in the shadow database, in the order it ran (execution time, not version, so a file run last by
`--exec-order non-linear` is replayed last), and of a file that stopped part way
only the statements recorded as applied. Files not applied yet are listed as pending and are not
drift. The history is checked as `migrate status` checks it (an applied file edited or missing, a
failed or interrupted file, a statement left in flight, a row edited by hand, a file out of order,
a directory that does not match `pgmig.sum`); each problem is printed with how to recover, and counts
as drift; the plan then targets the directory as it is now, so a warning says to resolve the
problems first (the same for a missing revisions table, which means an empty expected database;
JSON `history.warnings`). `--to <state>` compares with any state instead: SQL files, a migration directory replayed
to its newest file, or another database (staging against production, say). With neither flag,
`[migrations] dir` is used, else `to`. `--include`, `--exclude`, `--schema` and `--skip` work as for
`schema diff`.

The SQL that turns the database back into the expected state goes to stdout (schema-qualified: it
is meant to be run), a summary to stderr. A change the planner cannot plan is listed (`cannot plan:`;
JSON `unsupported`) and the plan is then empty. `--format json` prints `target`, `drifted`,
`changes`, `skipped`, `plan`, `sql`, `unsupported` and `history` (`dir`, `revisions_table`,
`current`, `replayed`, `partial`, `pending` (not the partial ones), `warnings`, `issues` with `kind`, `version`, `message` and `hint`;
null with `--to`). It exits `3` on drift and `0` without; it only reads the database.

Exit codes: `0` success; `1` error (including `validate` finding errors); `2` usage (bad flags, or `apply` with no way to approve);
`3` with `--exit-code` (`diff`, or `apply --dry-run`) when the plan is not empty (skipped changes
do not count: a diff with only skipped changes exits `0`), and from `drift check` (always) when the
database drifted.

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
