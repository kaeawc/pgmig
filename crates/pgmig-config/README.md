# pgmig-config

pgmig's project configuration: `pgmig.toml` parsing, environments and `${VAR}` interpolation.

An internal crate of [pgmig](https://crates.io/crates/pgmig); depend on `pgmig` instead. Its API
follows pgmig's version and may change in any release.

## `pgmig.toml`

Every key is named after the command-line flag it stands for. Top-level keys apply everywhere;
an `[env.<name>]` section, selected with `--env <name>`, overrides them key by key.

```toml
shadow = "pgmem"                  # --shadow: `pgmem` or the URL of an empty Postgres database
to = "schema/"                    # --to (and the SOURCE of `schema inspect` / `schema validate`)
exclude = ["public.tmp_*"]        # --exclude (array; replaces, never extends, the level below)
format = "text"                   # --format: "text" or "json"

[migrations]
dir = "migrations"                # --dir

[env.local]
url = "postgres://localhost/app"  # --url: the target database

[env.prod]
url = "host=db.internal user=app password=${PGPASSWORD}"
schema = "app"                    # --schema: manage only this schema's objects
```

Keys: `url`, `shadow`, `from`, `to`, `include`, `exclude`, `schema`, `format`, `migrations.dir`.
`include` and `exclude` are arrays of strings; the others are strings. Any other key is an
error naming its path (`env.prod.urll`). Diff policy and lint rules will get their own sections.

- **Precedence:** command-line flag > `[env.<name>]` > top level > built-in default.
- **Interpolation:** `${NAME}` in a string value is replaced by `--var NAME=value`, or else by the
  environment variable `NAME`; an undefined name is an error naming the key. `$${` is a literal
  `${`. Only the values in use (top level and the selected environment) are interpolated; the
  placeholders of every section are checked for syntax.
- **Discovery:** `--config <path>`, or else `pgmig.toml` in the current directory (parent
  directories are not searched). Relative paths in values are relative to the current directory,
  as on the command line.
- **Secrets:** errors name keys and variables, never values.
