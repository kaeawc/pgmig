use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use pgmig::api::{self, ApiError};
use pgmig::config::{Config, Settings, Vars};
use pgmig::{ChangeKind, Filter, Options, ShadowSpec, Skipped, Source};
use serde::Serialize;

const AFTER_HELP: &str = "\
Exit codes:
  0  success
  1  error (also: `schema validate` or `migrate validate` found errors)
  2  usage error (unknown flag, missing argument)
  3  `--exit-code` was given and the plan is not empty (`migrate status`: the database is not up
     to date: files pending, or problems)

A state (--from, --to, SOURCE) is a database when it is a postgres:// URL or a key=value
connection string, a migration directory when it is a directory holding pgmig.sum (checked
against it, then replayed in version order), and otherwise a SQL file or directory of SQL files.

With `--format json`, a command prints one JSON document on stdout, and nothing on stderr. A
failure prints the error object ({\"code\", \"message\", \"sqlstate\", \"file\", \"line\", \"col\",
\"apply\"}) on stdout and exits 1, or 2 when its code is \"usage\". Bad flags are reported by the
argument parser as text on stderr (exit 2).

Configuration: every flag except --dry-run, --auto-approve, --edit, --exit-code, --force,
--baseline, --allow-dirty and --no-lock can be set in
pgmig.toml (or the file given to --config), under the flag's name: `url`, `shadow`, `from`, `to`
(also the SOURCE of `schema inspect`, `schema fingerprint` and `schema validate`, and the desired state of `migrate
diff`), `include`, `exclude`, `schema`,
`format`, and the `migrate apply` flags under `[migrations]`: `dir`, `tx_mode`, `exec_order`,
`lock_name`, `lock_timeout`, `before_sql`, `after_sql`. `[migrations] revisions_schema` (default
`pgmig`) names the schema of the revisions table; pgmig never manages that schema. `[diff] skip`
is --skip, `[diff] verify = true` is --verify, `[diff] concurrent_indexes = true` is --concurrent-indexes. `[env.NAME]` sections, selected with --env NAME,
override the top level. A flag wins over both. `${KEY}` in a value is replaced by --var KEY=VALUE,
or else by the environment variable KEY; `$${` is a literal `${`.";

#[derive(Parser)]
#[command(name = "pgmig", version, about = "Postgres schema migrations from plain SQL", after_help = AFTER_HELP)]
struct Cli {
    /// Read settings from this file instead of ./pgmig.toml (only the current directory is searched).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Use the config file's [env.NAME] section over its top-level settings.
    #[arg(long, global = true, value_name = "NAME")]
    env: Option<String>,
    /// A value for `${KEY}` in the config file (repeatable); wins over the environment variable KEY.
    #[arg(long = "var", global = true, value_name = "KEY=VALUE")]
    vars: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with schema states (SQL files or live databases).
    #[command(subcommand)]
    Schema(SchemaCommand),
    /// Work with a migration directory (`<version>_<description>.sql` files and pgmig.sum).
    #[command(subcommand)]
    Migrate(MigrateCommand),
    /// Work with the configuration file (pgmig.toml).
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Check the file: syntax, unknown keys, `${KEY}` placeholders, and with --env that the
    /// environment exists and resolves. Prints nothing on success.
    Validate {
        #[command(flatten)]
        format: FormatArg,
    },
}

#[derive(Subcommand)]
enum MigrateCommand {
    /// Write or update the directory's sum file (pgmig.sum).
    Hash {
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Write the next migration: replay the directory in the shadow database, diff it against the
    /// desired state, and write the plan to `<version>_<NAME>.sql` (version: the UTC time
    /// YYYYMMDDHHMMSS, after every existing file), then update pgmig.sum. With no changes nothing
    /// is written. The directory must match its sum file.
    Diff {
        /// The migration's description: lowercased, anything but a-z, 0-9 and _ becomes _.
        name: String,
        #[command(flatten)]
        dir: DirArg,
        /// The desired state: SQL file, directory of SQL files, migration directory or database
        /// URL (config: `to`).
        #[arg(long)]
        to: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        skip: PolicyArg,
        /// Open the plan in $VISUAL, else $EDITOR, else vi (split on whitespace; no shell) before
        /// writing it: what you save is what is written (a file left without statements writes nothing), with no new diff. Needs a terminal; not with
        /// --format json.
        #[arg(long)]
        edit: bool,
        #[command(flatten)]
        verify: VerifyArg,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Adopt an existing database: write its schema (`schema inspect` of it, schema-qualified;
    /// --include, --exclude and --schema apply, the revisions schema never) as the first migration
    /// `<version>_<NAME>.sql` of an empty or missing directory, then update pgmig.sum. Then mark it
    /// applied there with `migrate apply --baseline <version>`; fresh databases run it as usual.
    Baseline {
        /// The migration's description: lowercased, anything but a-z, 0-9 and _ becomes _.
        name: String,
        /// The database to read (config: `url`).
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Write an empty migration `<version>_<NAME>.sql` to fill in by hand, then update pgmig.sum.
    New {
        /// The migration's description: lowercased, anything but a-z, 0-9 and _ becomes _.
        name: String,
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Apply the pending migrations to a database, in version order. Refuses when the directory
    /// does not match pgmig.sum or the database's revisions table does not match the directory (an
    /// applied file edited or deleted, a row edited by hand, an older pending file under the
    /// linear order). Holds an advisory lock for the whole run, so concurrent runs wait.
    ///
    /// A database without migration history that already holds objects is refused unless
    /// --baseline or --allow-dirty is given.
    Apply {
        /// Apply at most N pending files.
        n: Option<usize>,
        /// The database to migrate (config: `url`).
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        dir: DirArg,
        /// `file` (default): each file in its own transaction; `all`: every pending file in one;
        /// `none`: no transaction, each statement recorded as it commits so a re-run resumes after
        /// the last one. `-- pgmig:txmode none` before a file's first statement runs it without a
        /// transaction.
        #[arg(long, value_enum)]
        tx_mode: Option<TxModeArg>,
        /// For pending files older than the newest applied one: `linear` (default) refuses,
        /// `linear-skip` skips them with a warning, `non-linear` applies them.
        #[arg(long, value_enum)]
        exec_order: Option<ExecOrderArg>,
        /// The advisory lock's name (default `pgmig:<revisions schema>`).
        #[arg(long, value_name = "NAME")]
        lock_name: Option<String>,
        /// How long to wait for the lock: `500ms`, `10s` (default), `2m`; `0` tries once.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        lock_timeout: Option<u64>,
        /// Do not take the advisory lock.
        #[arg(long, conflicts_with_all = ["lock_name", "lock_timeout"])]
        no_lock: bool,
        /// SQL run right after each BEGIN, e.g. "SET LOCAL lock_timeout = '5s'".
        #[arg(long, value_name = "SQL")]
        before_sql: Option<String>,
        /// SQL run right before each COMMIT.
        #[arg(long, value_name = "SQL")]
        after_sql: Option<String>,
        /// On a database without history that holds objects: record every file up to and including
        /// VERSION (exact version or file name) as applied, kind baseline, without running it, then
        /// apply the files after it. Ignored on an empty database (every file runs, VERSION's
        /// included) and when the database already has VERSION applied, so it is safe to always
        /// pass; refused when the database has other history.
        #[arg(long, value_name = "VERSION", conflicts_with = "allow_dirty")]
        baseline: Option<String>,
        /// Apply every file onto a database without history even though it already holds objects.
        #[arg(long)]
        allow_dirty: bool,
        /// List the pending files and change nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Show a database's migration history against the directory: the current version (newest
    /// complete), each file's status, the pending files, the last error, and each problem that
    /// makes `migrate apply` refuse or stop (a file failed or left in flight, an applied file
    /// edited or missing, a row edited by hand) with how to recover. Only reads.
    Status {
        /// The database to read (config: `url`).
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        format: FormatArg,
        /// Exit with status 3 when the database is not up to date (files pending, or problems).
        #[arg(long)]
        exit_code: bool,
    },
    /// Fix up the revisions table by hand, running nothing: make the history exactly the files up
    /// to and including VERSION. Files without a row are recorded as applied; rows not complete
    /// (failed, stopped part way, a statement left in flight) are recorded as complete; rows edited
    /// by hand are rewritten; rows after VERSION or without a file are deleted. Recording a row
    /// that is not complete as complete (its remaining statements may not have run), deleting a
    /// row of a complete migration, or recording an applied file that was edited since, needs
    /// --force.
    /// `0` clears the history. One transaction, under `migrate apply`'s advisory lock.
    ///
    /// After a statement left in flight: check whether it took effect, then finish the file by hand
    /// and set its version with --force, or undo what it applied and set the version before it
    /// (`migrate apply` then runs the file again).
    Set {
        /// The exact version or file name the history ends at; `0` for none.
        version: String,
        /// The database to change (config: `url`).
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        dir: DirArg,
        /// Record rows that are not complete as complete, delete rows of complete migrations and
        /// rehash edited applied files.
        #[arg(long)]
        force: bool,
        /// The advisory lock's name (default `pgmig:<revisions schema>`).
        #[arg(long, value_name = "NAME")]
        lock_name: Option<String>,
        /// How long to wait for the lock: `500ms`, `10s` (default), `2m`; `0` tries once.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        lock_timeout: Option<u64>,
        /// Do not take the advisory lock.
        #[arg(long, conflicts_with_all = ["lock_name", "lock_timeout"])]
        no_lock: bool,
        /// Show what would change and change nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        format: FormatArg,
    },
    /// List the migrations in version order: version and description, and with --url their status
    /// in that database (applied, pending, partial, changed), read from its revisions table without
    /// writing. Applied versions without a file are listed as missing.
    Ls {
        #[command(flatten)]
        dir: DirArg,
        /// Read this database's revisions table for each file's status (only the flag: `migrate ls`
        /// connects only when asked).
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Print a migration file.
    Show {
        /// The file name, the version, or the start of exactly one file's version.
        migration: String,
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Open a migration in $VISUAL, else $EDITOR, else vi (split on whitespace: `code -w` works;
    /// no shell), then update pgmig.sum. Needs a terminal; the directory must match pgmig.sum.
    Edit {
        /// The file name, the version, or the start of exactly one file's version.
        migration: String,
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Delete a migration and update pgmig.sum. With a database (--url, or `url` in the config
    /// file), refuses a file its revisions table records unless --force.
    Rm {
        /// The file name, the version, or the start of exactly one file's version.
        migration: String,
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        guard: GuardArgs,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Move migrations after every other file with new versions (descriptions kept), then update
    /// pgmig.sum. After a merge in which both branches added files: keep the other branch's
    /// pgmig.sum, then rebase your own files. With a database (--url, or `url` in the config file),
    /// refuses a file its revisions table records unless --force.
    Rebase {
        /// The files to move: file names, versions, or unique version prefixes.
        #[arg(required = true)]
        migrations: Vec<String>,
        #[command(flatten)]
        dir: DirArg,
        #[command(flatten)]
        guard: GuardArgs,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Check the files against the sum file, and with --shadow replay them in order; prints
    /// nothing on success.
    Validate {
        #[command(flatten)]
        dir: DirArg,
        /// Also replay every migration in order into this shadow database: `pgmem` (embedded) or
        /// the URL of an empty Postgres database. A `shadow` in the config file turns it on too.
        #[arg(long)]
        shadow: Option<String>,
        #[command(flatten)]
        format: FormatArg,
    },
}

#[derive(clap::Args)]
struct GuardArgs {
    /// Check this database's revisions table (read only) for the files first (config: `url`).
    #[arg(long)]
    url: Option<String>,
    /// Go ahead even when the database records the file.
    #[arg(long)]
    force: bool,
}

impl GuardArgs {
    fn guard(self, s: &Settings) -> pgmig::Guard {
        pgmig::Guard {
            target: self.url.or_else(|| s.url.clone()),
            force: self.force,
            revisions_schema: s.migrations_revisions_schema.clone().unwrap_or_else(|| pgmig::revisions::DEFAULT_SCHEMA.to_owned()),
        }
    }
}

/// `--edit` needs a terminal for the editor, and text output: refused (usage error, exit 2) before
/// any work otherwise.
fn check_edit(format: Format) -> anyhow::Result<()> {
    if format == Format::Json {
        return Err(ApiError::usage("--edit opens an editor, which --format json never does; drop one of them").into());
    }
    if !std::io::stdin().is_terminal() {
        return Err(ApiError::usage("stdin is not a terminal, so pgmig cannot run an editor for --edit").into());
    }
    Ok(())
}

/// The editor of `migrate edit` and `--edit`: $VISUAL, else $EDITOR, else `vi`, split on whitespace.
fn editor() -> Vec<String> {
    let var = |k| std::env::var(k).ok().filter(|v: &String| !v.trim().is_empty());
    let cmd = var("VISUAL").or_else(|| var("EDITOR")).unwrap_or_else(|| "vi".to_owned());
    cmd.split_whitespace().map(str::to_owned).collect()
}

#[derive(Clone, Copy, ValueEnum)]
enum TxModeArg {
    File,
    All,
    None,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExecOrderArg {
    Linear,
    LinearSkip,
    NonLinear,
}

fn parse_duration(s: &str) -> Result<u64, String> {
    pgmig::config::parse_duration_ms(s).ok_or_else(|| "expected a duration such as 500ms, 10s or 2m".to_owned())
}

#[derive(clap::Args)]
struct DirArg {
    /// The migration directory (config: `[migrations] dir`).
    #[arg(long)]
    dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Print the SQL that turns one state into another.
    Diff {
        /// SQL file, directory of SQL files, migration directory, or database URL.
        #[arg(long)]
        from: Option<String>,
        /// SQL file, directory of SQL files, migration directory, or database URL.
        #[arg(long)]
        to: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        skip: PolicyArg,
        #[command(flatten)]
        format: FormatArg,
        /// Exit with status 3 when there are changes (skipped ones do not count).
        #[arg(long)]
        exit_code: bool,
    },
    /// Print SQL that recreates a state's schema in a new database.
    Inspect {
        /// SQL file, directory of SQL files, migration directory, or database URL (config: `to`).
        source: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Print the fingerprint of a state's schema: `h1:<base64 SHA-256>` of its managed objects, the
    /// same for a database and for SQL or migrations describing the same schema.
    Fingerprint {
        /// SQL file, directory of SQL files, migration directory, or database URL (config: `to`).
        source: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Change a live database to match a desired state: show the plan, ask, execute.
    Apply {
        /// The database to change.
        #[arg(long)]
        url: Option<String>,
        /// SQL file, directory of SQL files, migration directory, or database URL.
        #[arg(long)]
        to: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        skip: PolicyArg,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Apply without asking. Required when stdin is not a terminal.
        #[arg(long)]
        auto_approve: bool,
        /// Open the plan in $VISUAL, else $EDITOR, else vi (split on whitespace; no shell) before
        /// showing and applying it: what you save is what runs (a `-- pgmig:commit` line marks one commit point), with no new diff. Needs a terminal; not with
        /// --format json.
        #[arg(long)]
        edit: bool,
        #[command(flatten)]
        verify: VerifyArg,
        #[command(flatten)]
        format: FormatArg,
        /// With --dry-run, exit with status 3 when there are changes.
        #[arg(long, requires = "dry_run")]
        exit_code: bool,
    },
    /// Drop every managed object in a database, leaving it as a new database holds it: for local
    /// development. Shows the plan and asks for the database name to be typed. `public` stays (with
    /// its standard comment), and so do the revisions schema, unmanaged objects and the managed
    /// objects they need (reported as kept); extensions are not touched.
    Clean {
        /// The database to clean.
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Clean without asking. Required when stdin is not a terminal.
        #[arg(long)]
        auto_approve: bool,
        #[command(flatten)]
        format: FormatArg,
    },
    /// Load SQL files into the shadow database and report errors; prints nothing on success.
    Validate {
        /// SQL file, directory of SQL files, or migration directory (config: `to`).
        source: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        format: FormatArg,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[derive(clap::Args)]
struct FormatArg {
    /// Output format: `text` (the default), or `json` (one JSON document on stdout; errors too).
    #[arg(long, value_enum)]
    format: Option<Format>,
}

#[derive(clap::Args)]
struct ShadowArg {
    /// Where SQL files are executed: `pgmem` (embedded, the default) or the URL of an empty Postgres
    /// database.
    #[arg(long)]
    shadow: Option<String>,
}

impl ShadowArg {
    fn spec(self, settings: &Settings) -> ShadowSpec {
        ShadowSpec::parse(self.shadow.as_deref().or(settings.shadow.as_deref()).unwrap_or("pgmem"))
    }
}

#[derive(clap::Args)]
struct ScopeArgs {
    /// Manage only objects matching this pattern (repeatable): `[kind:]schema[.object[.column]]`,
    /// kind one of schema, table, enum, domain, composite, sequence; `*` and `?` match unquoted names.
    #[arg(long, value_name = "PATTERN")]
    include: Vec<String>,
    /// Leave objects matching this pattern alone (repeatable; same syntax as --include).
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
    /// Manage only this schema's objects (not the schema itself). `inspect` and `diff` print their
    /// names unqualified; `apply` always prints and runs schema-qualified SQL.
    #[arg(long, value_name = "NAME")]
    schema: Option<String>,
}

impl ScopeArgs {
    /// The flags, or where none are given, the config file's values (a flag replaces the whole list).
    fn options(self, settings: &Settings) -> anyhow::Result<Options> {
        let list = |flags: Vec<String>, config: &Option<Vec<String>>| {
            if flags.is_empty() { config.clone().unwrap_or_default() } else { flags }
        };
        let include = list(self.include, &settings.include);
        let exclude = list(self.exclude, &settings.exclude);
        Ok(Options {
            filter: Filter::parse(&include, &exclude).map_err(ApiError::from)?,
            schema: self.schema.or_else(|| settings.schema.clone()),
            revisions_schema: revisions_schema(settings),
            skip: Vec::new(),
            concurrent_indexes: false,
        })
    }
}

/// The diff policy: what plans leave out (`--skip`) and how they build indexes.
#[derive(clap::Args)]
struct PolicyArg {
    /// Never plan this kind of change (repeatable): one of the `kind` names of the JSON changes,
    /// such as drop_table, drop_column, drop_schema, drop_constraint, drop_default. A change that
    /// needs a skipped one is skipped too. Skipped changes are reported on stderr (in JSON:
    /// `skipped`) and found again by every later diff (config: `[diff] skip`).
    #[arg(long, value_name = "KIND", value_parser = parse_kind)]
    skip: Vec<ChangeKind>,
    /// Create and drop indexes CONCURRENTLY, so writes to the table are not blocked. Postgres
    /// refuses these statements in a transaction, so each runs on its own outside one: `schema
    /// apply` commits the statements before it first, and `migrate diff` writes one file per such
    /// statement (`-- pgmig:txmode none`) and cannot be combined with --edit. A failed build leaves
    /// an INVALID index to drop (DROP INDEX CONCURRENTLY). A primary key or unique constraint is
    /// added by building its index that way and then attaching it (ADD CONSTRAINT ... USING
    /// INDEX). Indexes and keys on tables the plan creates or drops, and on partitioned tables,
    /// keep the plain statement (config: `[diff] concurrent_indexes`).
    #[arg(long, overrides_with = "no_concurrent_indexes")]
    concurrent_indexes: bool,
    /// Plain CREATE INDEX and DROP INDEX, whatever `[diff] concurrent_indexes` says.
    #[arg(long)]
    no_concurrent_indexes: bool,
}

fn parse_kind(s: &str) -> Result<ChangeKind, String> {
    s.parse().map_err(|e: pgmig::UnknownKind| e.to_string())
}

impl PolicyArg {
    /// `options` with the policy: the flags, or where none are given, `[diff] skip` (checked when
    /// the file was read) and `[diff] concurrent_indexes`.
    fn add_to(self, mut options: Options, settings: &Settings) -> anyhow::Result<Options> {
        options.skip = if self.skip.is_empty() {
            let names = settings.diff_skip.iter().flatten();
            names.map(|n| n.parse().map_err(|e: pgmig::UnknownKind| ApiError::usage(e.to_string()))).collect::<Result<_, _>>()?
        } else {
            self.skip
        };
        options.concurrent_indexes = match (self.concurrent_indexes, self.no_concurrent_indexes) {
            (true, _) => true,
            (_, true) => false,
            _ => settings.diff_concurrent_indexes.unwrap_or(false),
        };
        Ok(options)
    }
}

#[derive(clap::Args)]
struct VerifyArg {
    /// Check the plan first (with --edit, as edited): apply it in the shadow database to a copy of
    /// the current state and compare the result with the desired state (changes --skip leaves out
    /// may remain). A failure stops before the target is changed or a migration written (config:
    /// `[diff] verify`). The copy is schema only, with no rows: failures that depend on data (SET NOT
    /// NULL over NULLs, a unique index over duplicates, a USING cast) are not caught. `migrate diff`
    /// replays the directory; `schema apply` rebuilds the copy from what pgmig models (tables, types,
    /// sequences, constraints, indexes, comments), without extensions, functions or views.
    /// Statements outside any schema (CREATE ROLE, ALTER SYSTEM, ...) in an edited plan or a
    /// migration really run, on the shadow's server.
    #[arg(long, overrides_with = "no_verify")]
    verify: bool,
    /// Do not check the plan, whatever `[diff] verify` says.
    #[arg(long)]
    no_verify: bool,
}

impl VerifyArg {
    /// The flags, or where neither is given, `[diff] verify`.
    fn on(self, settings: &Settings) -> bool {
        match (self.verify, self.no_verify) {
            (true, _) => true,
            (_, true) => false,
            _ => settings.diff_verify.unwrap_or(false),
        }
    }
}

/// One notice per skipped change, on stderr.
fn print_skipped(skipped: &[Skipped]) {
    for s in skipped {
        eprintln!("skipped {s}");
    }
}

/// `[migrations] revisions_schema`, or the default.
fn revisions_schema(s: &Settings) -> String {
    s.migrations_revisions_schema.clone().unwrap_or_else(|| pgmig::revisions::DEFAULT_SCHEMA.to_owned())
}

/// The database a command connects to (--url, else `url` in the config file), and how to show it.
fn target(url: Option<String>, loaded: &Loaded) -> anyhow::Result<(String, Target)> {
    let source = match &url {
        Some(_) => "--url".to_owned(),
        None => loaded.url_source.clone().unwrap_or_else(|| "`url` in the config file".to_owned()),
    };
    let target = need(url, loaded.settings.url.clone(), "--url", "url")?;
    let shown = Target { display: pgmig::config::redact(&target), source };
    Ok((target, shown))
}

/// A value that must be given: the flag, else the config file's key; a usage error (exit 2) names both.
fn need<T>(flag: Option<T>, config: Option<T>, name: &str, key: &str) -> anyhow::Result<T> {
    match flag.or(config) {
        Some(v) => Ok(v),
        None => Err(ApiError::usage(format!("missing {name}: pass it, or set `{key}` in pgmig.toml (see --config, --env)")).into()),
    }
}

/// The settings in use, and where their `url` came from (`env.prod.url in pgmig.toml`).
#[derive(Default)]
struct Loaded {
    settings: Settings,
    url_source: Option<String>,
}

/// The settings from the config file (--config, or ./pgmig.toml when there is one) for --env.
fn load_settings(cli: &Cli) -> anyhow::Result<Loaded> {
    let vars = Vars::from_args(&cli.vars).map_err(ApiError::from)?;
    match Config::discover(cli.config.as_deref(), Path::new("")).map_err(ApiError::from)? {
        Some(config) => Ok(Loaded {
            settings: config.resolve(cli.env.as_deref(), &vars).map_err(ApiError::from)?,
            url_source: config.url_key(cli.env.as_deref()).map(|key| format!("`{key}` in {}", config.path.display())),
        }),
        None if cli.env.is_some() => Err(ApiError::usage(format!(
            "--env needs a config file: there is no {} in the current directory (pass --config)",
            pgmig::config::FILE_NAME
        ))
        .into()),
        None => Ok(Loaded::default()),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // `config validate` reports the file's problems itself, so it does not load it first.
    let loaded = if matches!(cli.command, Command::Config(_)) { Ok(Loaded::default()) } else { load_settings(&cli) };
    let config_format = loaded.as_ref().ok().and_then(|l| match l.settings.format.as_deref() {
        Some("json") => Some(Format::Json),
        Some("text") => Some(Format::Text),
        _ => None,
    });
    let format = cli.format_flag().or(config_format).unwrap_or(Format::Text);
    match loaded.and_then(|loaded| run(cli, &loaded, format)) {
        Ok(code) => code,
        Err(e) => {
            // A usage error found after parsing (no approval in a non-interactive shell) exits 2,
            // like the usage errors clap reports.
            let usage = e.downcast_ref::<ApiError>().is_some_and(|a| a.code == api::ErrorCode::Usage);
            if format == Format::Json {
                let err = match e.downcast::<ApiError>() {
                    Ok(err) => err,
                    Err(e) => ApiError::usage(e.to_string()),
                };
                match serde_json::to_string_pretty(&err) {
                    Ok(json) => println!("{json}"),
                    Err(e) => eprintln!("error: {e}"),
                }
            } else {
                // Library errors already include their cause in their message; `{e:#}` would repeat it.
                eprintln!("error: {e}");
            }
            if usage { ExitCode::from(2) } else { ExitCode::FAILURE }
        }
    }
}

impl Cli {
    fn format_flag(&self) -> Option<Format> {
        match &self.command {
            Command::Schema(
                SchemaCommand::Diff { format, .. }
                | SchemaCommand::Inspect { format, .. }
                | SchemaCommand::Fingerprint { format, .. }
                | SchemaCommand::Apply { format, .. }
                | SchemaCommand::Clean { format, .. }
                | SchemaCommand::Validate { format, .. },
            )
            | Command::Migrate(
                MigrateCommand::Hash { format, .. }
                | MigrateCommand::Diff { format, .. }
                | MigrateCommand::New { format, .. }
                | MigrateCommand::Baseline { format, .. }
                | MigrateCommand::Apply { format, .. }
                | MigrateCommand::Status { format, .. }
                | MigrateCommand::Set { format, .. }
                | MigrateCommand::Ls { format, .. }
                | MigrateCommand::Show { format, .. }
                | MigrateCommand::Edit { format, .. }
                | MigrateCommand::Rm { format, .. }
                | MigrateCommand::Rebase { format, .. }
                | MigrateCommand::Validate { format, .. },
            )
            | Command::Config(ConfigCommand::Validate { format }) => format.format,
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Status 3: `--exit-code` and a plan with statements.
const CHANGES: u8 = 3;

/// Builds each command's request from the flags and the config file's settings, calls the library
/// and prints the response in `format`.
fn run(cli: Cli, loaded: &Loaded, format: Format) -> anyhow::Result<ExitCode> {
    let settings = &loaded.settings;
    let command = match cli.command {
        Command::Schema(command) => command,
        Command::Migrate(MigrateCommand::Apply {
            n,
            url,
            dir,
            tx_mode,
            exec_order,
            lock_name,
            lock_timeout,
            no_lock,
            before_sql,
            after_sql,
            baseline,
            allow_dirty,
            dry_run,
            format: _,
        }) => {
            let s = settings;
            let (target, shown) = target(url, loaded)?;
            // Values in the config file were checked when it was read.
            let tx_mode = match tx_mode {
                Some(TxModeArg::File) => pgmig::TxMode::File,
                Some(TxModeArg::All) => pgmig::TxMode::All,
                Some(TxModeArg::None) => pgmig::TxMode::None,
                None => s.migrations_tx_mode.as_deref().and_then(pgmig::TxMode::parse).unwrap_or_default(),
            };
            let exec_order = match exec_order {
                Some(ExecOrderArg::Linear) => pgmig::ExecOrder::Linear,
                Some(ExecOrderArg::LinearSkip) => pgmig::ExecOrder::LinearSkip,
                Some(ExecOrderArg::NonLinear) => pgmig::ExecOrder::NonLinear,
                None => s.migrations_exec_order.as_deref().and_then(pgmig::ExecOrder::parse).unwrap_or_default(),
            };
            let lock_timeout_ms = lock_timeout
                .or_else(|| s.migrations_lock_timeout.as_deref().and_then(pgmig::config::parse_duration_ms))
                .unwrap_or(pgmig::migrate_apply::DEFAULT_LOCK_TIMEOUT_MS);
            let req = api::MigrateApplyRequest {
                target,
                dir: need(dir.dir, s.migrations_dir.clone().map(PathBuf::from), "--dir", "migrations.dir")?,
                options: pgmig::ApplyOptions {
                    tx_mode,
                    exec_order,
                    lock: !no_lock,
                    lock_name: lock_name.or_else(|| s.migrations_lock_name.clone()),
                    lock_timeout_ms,
                    before_sql: before_sql.or_else(|| s.migrations_before_sql.clone()),
                    after_sql: after_sql.or_else(|| s.migrations_after_sql.clone()),
                    limit: n,
                    dry_run,
                    revisions_schema: revisions_schema(s),
                    baseline,
                    allow_dirty,
                },
            };
            return migrate_apply(&req, &shown, format);
        }
        Command::Migrate(MigrateCommand::Status { url, dir, format: _, exit_code }) => {
            let (target, shown) = target(url, loaded)?;
            let req = api::MigrateStatusRequest {
                target,
                dir: need(dir.dir, settings.migrations_dir.clone().map(PathBuf::from), "--dir", "migrations.dir")?,
                revisions_schema: revisions_schema(settings),
            };
            return migrate_status(&req, &shown, format, exit_code);
        }
        Command::Migrate(MigrateCommand::Set { version, url, dir, force, lock_name, lock_timeout, no_lock, dry_run, format: _ }) => {
            let s = settings;
            let (target, shown) = target(url, loaded)?;
            let req = api::MigrateSetRequest {
                target,
                dir: need(dir.dir, s.migrations_dir.clone().map(PathBuf::from), "--dir", "migrations.dir")?,
                version,
                options: pgmig::SetOptions {
                    force,
                    dry_run,
                    lock: !no_lock,
                    lock_name: lock_name.or_else(|| s.migrations_lock_name.clone()),
                    lock_timeout_ms: lock_timeout
                        .or_else(|| s.migrations_lock_timeout.as_deref().and_then(pgmig::config::parse_duration_ms))
                        .unwrap_or(pgmig::migrate_apply::DEFAULT_LOCK_TIMEOUT_MS),
                    revisions_schema: revisions_schema(s),
                },
            };
            return migrate_set(&req, &shown, format);
        }
        Command::Migrate(command) => return migrate(command, settings, format),
        Command::Config(ConfigCommand::Validate { .. }) => {
            let req = api::ConfigValidateRequest {
                path: cli.config.unwrap_or_else(|| PathBuf::from(pgmig::config::FILE_NAME)),
                env: cli.env,
                vars: pgmig::config::parse_vars(&cli.vars).map_err(ApiError::from)?,
            };
            let res = api::config_validate(&req)?;
            match format {
                Format::Text => {
                    for d in &res.diagnostics {
                        eprintln!("error: {d}");
                    }
                }
                Format::Json => print_json(&res)?,
            }
            return Ok(if res.is_valid() { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
    };
    let s = settings;
    match command {
        SchemaCommand::Apply { url, to, shadow, scope, skip, dry_run, auto_approve, edit, verify, format: _, exit_code } => {
            if edit {
                check_edit(format)?;
            }
            let source = match &url {
                Some(_) => "--url".to_owned(),
                None => loaded.url_source.clone().unwrap_or_else(|| "`url` in the config file".to_owned()),
            };
            let target = need(url, s.url.clone(), "--url", "url")?;
            let shown = Target { display: pgmig::config::redact(&target), source };
            let req = api::PlanApplyRequest {
                target,
                desired: Source::parse(&need(to, s.to.clone(), "--to", "to")?),
                shadow: shadow.spec(s),
                options: skip.add_to(scope.options(s)?, s)?,
                verify: false,
            };
            let flags = ApplyFlags { dry_run, auto_approve, edit, verify: verify.on(s), exit_code };
            return schema_apply(req, &shown, flags, format);
        }
        SchemaCommand::Clean { url, scope, dry_run, auto_approve, format: _ } => {
            let (target, shown) = target(url, loaded)?;
            let req = api::CleanPlanRequest { target, options: scope.options(s)? };
            return schema_clean(req, &shown, dry_run, auto_approve, format);
        }
        SchemaCommand::Diff { from, to, shadow, scope, skip, format: _, exit_code } => {
            let req = api::DiffRequest {
                from: Source::parse(&need(from, s.from.clone(), "--from", "from")?),
                to: Source::parse(&need(to, s.to.clone(), "--to", "to")?),
                shadow: shadow.spec(s),
                options: skip.add_to(scope.options(s)?, s)?,
            };
            let res = api::schema_diff(&req)?;
            match format {
                Format::Text => {
                    print!("{}", res.plan.to_sql());
                    std::io::stdout().flush()?;
                    print_skipped(&res.skipped);
                }
                Format::Json => print_json(&res)?,
            }
            if exit_code && !res.plan.is_empty() {
                return Ok(ExitCode::from(CHANGES));
            }
        }
        SchemaCommand::Validate { source, shadow, format: _ } => {
            let req = api::ValidateRequest { source: Source::parse(&need(source, s.to.clone(), "SOURCE", "to")?), shadow: shadow.spec(s) };
            let res = api::schema_validate(&req)?;
            match format {
                Format::Text => {
                    for d in &res.diagnostics {
                        eprintln!("error: {d}");
                    }
                }
                Format::Json => print_json(&res)?,
            }
            if !res.is_valid() {
                return Ok(ExitCode::FAILURE);
            }
        }
        SchemaCommand::Inspect { source, shadow, scope, format: _ } => {
            let req = api::InspectRequest {
                source: Source::parse(&need(source, s.to.clone(), "SOURCE", "to")?),
                shadow: shadow.spec(s),
                options: scope.options(s)?,
            };
            let res = api::schema_inspect(&req)?;
            match format {
                Format::Text => print!("{}", res.plan.to_sql()),
                Format::Json => print_json(&res)?,
            }
        }
        SchemaCommand::Fingerprint { source, shadow, scope, format: _ } => {
            let req = api::FingerprintRequest {
                source: Source::parse(&need(source, s.to.clone(), "SOURCE", "to")?),
                shadow: shadow.spec(s),
                options: scope.options(s)?,
            };
            let res = api::schema_fingerprint(&req)?;
            match format {
                Format::Text => println!("{}", res.fingerprint),
                Format::Json => print_json(&res)?,
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn migrate(command: MigrateCommand, s: &Settings, format: Format) -> anyhow::Result<ExitCode> {
    let dir = |d: DirArg| need(d.dir, s.migrations_dir.clone().map(PathBuf::from), "--dir", "migrations.dir");
    match command {
        MigrateCommand::Hash { dir: d, format: _ } => {
            let res = api::migrate_hash(&api::MigrateHashRequest { dir: dir(d)? })?;
            match format {
                Format::Text if res.written => {
                    let n = res.files.len();
                    eprintln!("wrote {} ({n} file{})", res.sum_file.display(), if n == 1 { "" } else { "s" });
                }
                Format::Text => eprintln!("{} is up to date", res.sum_file.display()),
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::Diff { name, dir: d, to, shadow, scope, skip, edit, verify, format: _ } => {
            if edit {
                check_edit(format)?;
            }
            let req = api::MigrateDiffRequest {
                dir: dir(d)?,
                name,
                to: Source::parse(&need(to, s.to.clone(), "--to", "to")?),
                shadow: shadow.spec(s),
                options: skip.add_to(scope.options(s)?, s)?,
                editor: edit.then(editor),
                verify: verify.on(s),
            };
            migrate_diff(&req, format)?;
        }
        MigrateCommand::Baseline { name, url, dir: d, scope, format: _ } => {
            let req = api::MigrateBaselineRequest {
                dir: dir(d)?,
                name,
                target: need(url, s.url.clone(), "--url", "url")?,
                options: scope.options(s)?,
            };
            let res = api::migrate_baseline(&req)?;
            match format {
                Format::Text => eprintln!(
                    "wrote {}; on that database, record it as applied with `pgmig migrate apply --baseline {}`",
                    res.file.display(),
                    res.version
                ),
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::New { name, dir: d, format: _ } => {
            let res = api::migrate_new(&api::MigrateNewRequest { dir: dir(d)?, name })?;
            match format {
                Format::Text => eprintln!("wrote {}", res.file.display()),
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::Ls { dir: d, url, format: _ } => {
            let req = api::MigrateLsRequest { dir: dir(d)?, target: url, revisions_schema: revisions_schema(s) };
            let res = api::migrate_ls(&req)?;
            match format {
                Format::Text => {
                    for f in &res.files {
                        match f.status {
                            Some(st) => println!("{:<8} {}  {}", st.as_str(), f.version, f.description),
                            None => println!("{}  {}", f.version, f.description),
                        }
                    }
                    for m in &res.missing {
                        println!("{:<8} {}  {}", "missing", m.version, m.description);
                    }
                    if !res.missing.is_empty() {
                        let n = res.missing.len();
                        eprintln!("warning: {n} applied version{} without a file", if n == 1 { "" } else { "s" });
                    }
                }
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::Show { migration, dir: d, format: _ } => {
            let res = api::migrate_show(&api::MigrateShowRequest { dir: dir(d)?, migration })?;
            match format {
                Format::Text => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(res.sql.as_bytes())?;
                    out.flush()?;
                }
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::Edit { migration, dir: d, format: _ } => {
            let dir = dir(d)?;
            if !std::io::stdin().is_terminal() {
                return Err(ApiError::usage(
                    "stdin is not a terminal, so pgmig cannot run an editor; edit the file, then run `pgmig migrate hash`",
                )
                .into());
            }
            let res = api::migrate_edit(&api::MigrateEditRequest { dir, migration, editor: editor() })?;
            match format {
                Format::Text if res.changed => eprintln!("updated {} for {}", res.sum_file.display(), res.file.display()),
                Format::Text => eprintln!("{} unchanged", res.file.display()),
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::Rm { migration, dir: d, guard, format: _ } => {
            let res = api::migrate_rm(&api::MigrateRmRequest { dir: dir(d)?, migration, guard: guard.guard(s) })?;
            match format {
                Format::Text => {
                    eprintln!("removed {}", res.file.display());
                    if res.recorded {
                        eprintln!("warning: the database records version {}; `migrate apply` refuses until that row is fixed", res.version);
                    }
                }
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::Rebase { migrations, dir: d, guard, format: _ } => {
            let res = api::migrate_rebase(&api::MigrateRebaseRequest { dir: dir(d)?, migrations, guard: guard.guard(s) })?;
            match format {
                Format::Text if res.moved.is_empty() => eprintln!("nothing to move: already after every other file"),
                Format::Text => {
                    for m in &res.moved {
                        eprintln!("moved {} -> {}", m.from.display(), m.to.display());
                    }
                }
                Format::Json => print_json(&res)?,
            }
        }
        // Handled by `run`, which knows where `url` came from.
        MigrateCommand::Apply { .. } | MigrateCommand::Status { .. } | MigrateCommand::Set { .. } => {
            unreachable!("migrate apply, status and set are run by `run`")
        }
        MigrateCommand::Validate { dir: d, shadow, format: _ } => {
            // The config file's `shadow` turns the replay on too.
            let shadow = shadow.or_else(|| s.shadow.clone());
            let req = api::MigrateValidateRequest { dir: dir(d)?, shadow: shadow.as_deref().map(ShadowSpec::parse) };
            let res = api::migrate_validate(&req)?;
            match format {
                Format::Text => {
                    for d in &res.diagnostics {
                        eprintln!("error: {d}");
                    }
                }
                Format::Json => print_json(&res)?,
            }
            if !res.is_valid() {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `migrate diff`: what was skipped, verified and written on stderr, or the response as JSON.
fn migrate_diff(req: &api::MigrateDiffRequest, format: Format) -> anyhow::Result<()> {
    let res = api::migrate_diff(req)?;
    if format == Format::Json {
        return print_json(&res);
    }
    print_skipped(&res.skipped);
    if req.verify && res.file.is_some() {
        eprintln!("verified: in the shadow database the migration makes the desired state");
    }
    match &res.file {
        Some(file) if res.edited => eprintln!("wrote {} (as edited)", file.display()),
        Some(_) if res.files.len() > 1 => {
            eprintln!(
                "wrote {} files, one per unit of the plan (a statement outside a transaction has a file of its own):",
                res.files.len()
            );
            for f in &res.files {
                eprintln!("  {}", f.path.display());
            }
        }
        Some(file) => {
            let n = res.plan.statements.len();
            eprintln!("wrote {} ({n} statement{})", file.display(), if n == 1 { "" } else { "s" });
        }
        None if res.edited => eprintln!("the edited migration has no statements: nothing written"),
        None => eprintln!("no changes"),
    }
    Ok(())
}

/// What `migrate apply --format json` prints: the target and the report.
#[derive(Serialize)]
struct MigrateApplyOutput<'a> {
    target: &'a Target,
    #[serde(flatten)]
    report: api::MigrateApplyResponse,
}

/// `migrate apply`: the files applied (or with --dry-run pending) on stdout, one per line; skipped
/// files and the summary on stderr.
fn migrate_apply(req: &api::MigrateApplyRequest, target: &Target, format: Format) -> anyhow::Result<ExitCode> {
    use pgmig::FileStatus;
    if format == Format::Text {
        eprintln!("Target: {} (from {})", target.display, target.source);
    }
    let report = api::migrate_apply(req)?;
    if format == Format::Json {
        print_json(&MigrateApplyOutput { target, report })?;
        return Ok(ExitCode::SUCCESS);
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if report.baseline_skipped {
        eprintln!("--baseline skipped: the database is empty (no history, no objects), so every file runs");
    }
    for f in &report.files {
        match f.status {
            FileStatus::Skipped => {
                eprintln!("warning: skipped {}: older than the newest applied migration (--exec-order linear-skip)", f.file);
            }
            FileStatus::Applied => {
                let resumed = if f.resumed_after > 0 { format!(", resumed after {}", f.resumed_after) } else { String::new() };
                println!("{} ({} statement{}{resumed}, {} ms)", f.file, f.statements, plural(f.statements), f.execution_ms);
            }
            FileStatus::Baselined if report.dry_run => println!("{} (baseline: would be recorded as applied, not run)", f.file),
            FileStatus::Baselined => println!("{} (baseline: recorded as applied, not run)", f.file),
            FileStatus::Pending if report.dry_run => println!("{}", f.file),
            FileStatus::Pending | FileStatus::AlreadyApplied | FileStatus::Failed | FileStatus::RolledBack | FileStatus::Unknown => {}
        }
    }
    let pending = report.files.iter().filter(|f| f.status == FileStatus::Pending).count();
    if report.dry_run {
        match pending {
            0 => eprintln!("nothing to apply"),
            n => eprintln!("{n} migration{} pending (dry run: nothing applied)", plural(n)),
        }
    } else if report.applied == 0 {
        eprintln!("nothing to apply");
    } else {
        let left = if pending > 0 { format!("; {pending} left pending") } else { String::new() };
        eprintln!(
            "applied {} migration{} ({} statement{}){left}",
            report.applied,
            plural(report.applied),
            report.statements,
            plural(report.statements)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// What `migrate status --format json` and `migrate set --format json` print: the target and the
/// result.
#[derive(Serialize)]
struct TargetOutput<'a, T: Serialize> {
    target: &'a Target,
    #[serde(flatten)]
    result: T,
}

/// `migrate status`: the files with their status on stdout, then the problems and their hints.
fn migrate_status(req: &api::MigrateStatusRequest, target: &Target, format: Format, exit_code: bool) -> anyhow::Result<ExitCode> {
    let res = api::migrate_status(req)?;
    let code = if exit_code && !res.is_up_to_date() { ExitCode::from(CHANGES) } else { ExitCode::SUCCESS };
    if format == Format::Json {
        print_json(&TargetOutput { target, result: res })?;
        return Ok(code);
    }
    println!("Target: {} (from {})", target.display, target.source);
    match (&res.revisions_table, &res.current) {
        (None, _) => println!("no revisions table: nothing applied"),
        (Some(_), None) => println!("current version: none"),
        (Some(_), Some(v)) => println!("current version: {v}"),
    }
    for f in &res.files {
        let kind = match f.kind.as_deref() {
            Some(k) if k != "normal" => format!(" ({k})"),
            _ => String::new(),
        };
        println!("{:<8} {}  {}{kind}", f.status.as_str(), f.version, f.description);
    }
    for m in &res.missing {
        println!("{:<8} {}  {}", "missing", m.version, m.description);
    }
    let n = res.pending.len();
    match n {
        0 => println!("nothing pending"),
        n => println!("{n} migration{} pending", if n == 1 { "" } else { "s" }),
    }
    if let Some(e) = &res.last_error {
        println!("last error: {e}");
    }
    for i in &res.issues {
        println!(
            "problem: {}
  {}",
            i.message, i.hint
        );
    }
    Ok(code)
}

/// `migrate set`: each row changed, on stdout.
fn migrate_set(req: &api::MigrateSetRequest, target: &Target, format: Format) -> anyhow::Result<ExitCode> {
    use pgmig::SetAction;
    if format == Format::Text {
        eprintln!("Target: {} (from {})", target.display, target.source);
    }
    let res = api::migrate_set(req)?;
    if format == Format::Json {
        print_json(&TargetOutput { target, result: res })?;
        return Ok(ExitCode::SUCCESS);
    }
    for c in &res.changes {
        let what = match c.action {
            SetAction::Insert => "recorded as applied",
            SetAction::Resolve => "recorded as complete",
            SetAction::Rehash => "recorded with the file as it is now",
            SetAction::Repair => "rewritten (was edited by hand)",
            SetAction::Delete => "deleted",
        };
        println!("{}: {what}", c.file.as_deref().unwrap_or(&c.version));
    }
    let end = res.version.as_deref().unwrap_or("none");
    match (res.changes.is_empty(), res.dry_run) {
        (true, _) => eprintln!("history already ends at {end}: nothing to change"),
        (false, true) => eprintln!("dry run: nothing changed"),
        (false, false) => eprintln!("history now ends at {end}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// The database `schema apply` changes, as shown: password redacted, and where it was set.
#[derive(Serialize)]
struct Target {
    /// The connection string with its password replaced by `***`.
    display: String,
    /// `--url`, or the config key and file (`` `env.prod.url` in pgmig.toml ``).
    source: String,
}

/// What `schema apply --format json` prints: the target, the plan, and the report when it was applied.
#[derive(Serialize)]
struct ApplyOutput<'a> {
    target: &'a Target,
    #[serde(flatten)]
    plan: api::PlanApplyResponse,
    applied: bool,
    report: Option<pgmig::ApplyReport>,
}

/// The flags of `schema apply` that decide what [`schema_apply`] does with the plan.
struct ApplyFlags {
    dry_run: bool,
    auto_approve: bool,
    edit: bool,
    verify: bool,
    exit_code: bool,
}

/// Shows the plan (with --edit, as edited), asks (unless told not to), then applies it. In JSON
/// mode it never asks and prints one document at the end.
fn schema_apply(req: api::PlanApplyRequest, target: &Target, flags: ApplyFlags, format: Format) -> anyhow::Result<ExitCode> {
    let ApplyFlags { dry_run, auto_approve, edit, verify, exit_code } = flags;
    let json = format == Format::Json;
    // An edited plan is checked once edited, below; otherwise as it is planned.
    let req = api::PlanApplyRequest { verify: verify && !edit, ..req };
    let mut res = api::schema_plan_apply(&req)?;
    if !json {
        print_skipped(&res.skipped);
    }
    if res.plan.is_empty() {
        if json {
            print_json(&ApplyOutput { target, plan: res, applied: false, report: None })?;
        } else {
            eprintln!("no changes");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if edit {
        // What the editor saves is the plan from here on: `changes` still says what the diff found.
        let edited = api::edit_plan(&api::EditPlanRequest { plan: res.plan, editor: editor() })?;
        if edited.plan.is_empty() {
            eprintln!("the edited plan has no statements: nothing to apply");
            return Ok(ExitCode::SUCCESS);
        }
        res.sql = edited.plan.to_sql();
        res.plan = edited.plan;
        if verify {
            api::schema_verify(&api::VerifyRequest {
                target: req.target.clone(),
                desired: req.desired.clone(),
                plan: res.plan.clone(),
                shadow: req.shadow.clone(),
                options: req.options.clone(),
            })?;
        }
    }
    if verify && !json {
        eprintln!("verified: in the shadow database the plan makes the desired state");
    }
    let dry_code = if dry_run && exit_code { ExitCode::from(CHANGES) } else { ExitCode::SUCCESS };
    if !json {
        print!("{}", res.plan.to_sql());
        std::io::stdout().flush()?;
        // On stderr, like the prompt: stdout stays the plan.
        eprintln!("Target: {} (from {})", target.display, target.source);
    }
    if dry_run {
        if json {
            print_json(&ApplyOutput { target, plan: res, applied: false, report: None })?;
        }
        return Ok(dry_code);
    }
    let n = res.plan.statements.len();
    if !auto_approve {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("stdin is not a terminal, so pgmig cannot ask before applying; pass --auto-approve to apply without asking");
        }
        if json {
            anyhow::bail!("--format json never asks before applying; pass --auto-approve to apply without asking, or --dry-run");
        }
        eprint!("Apply these {n} statements to this database? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES") {
            anyhow::bail!("not applied");
        }
    }
    let report = api::apply(&api::ApplyRequest { target: req.target, plan: res.plan.clone() })?;
    if json {
        print_json(&ApplyOutput { target, plan: res, applied: true, report: Some(report) })?;
    } else {
        match report.non_transactional {
            0 => eprintln!("applied {} statements in {} transaction(s)", report.statements, report.batches),
            k => eprintln!(
                "applied {} statements: {} in {} transaction(s), {k} outside a transaction",
                report.statements,
                report.statements - k,
                report.batches
            ),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// What `schema clean --format json` prints: the target, the plan, and the report when it was applied.
#[derive(Serialize)]
struct CleanOutput<'a> {
    target: &'a Target,
    #[serde(flatten)]
    plan: api::CleanPlanResponse,
    applied: bool,
    report: Option<pgmig::ApplyReport>,
}

/// Shows the drops and what is kept, asks for the database name (unless told not to), then
/// applies them. In JSON mode it never asks and prints one document at the end.
fn schema_clean(
    req: api::CleanPlanRequest,
    target: &Target,
    dry_run: bool,
    auto_approve: bool,
    format: Format,
) -> anyhow::Result<ExitCode> {
    let json = format == Format::Json;
    let res = api::schema_plan_clean(&req)?;
    if !json {
        for k in &res.kept {
            eprintln!("kept {}: {}", k.object, k.because);
        }
    }
    if res.plan.is_empty() {
        if json {
            print_json(&CleanOutput { target, plan: res, applied: false, report: None })?;
        } else {
            eprintln!("nothing to clean");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !json {
        print!("{}", res.plan.to_sql());
        std::io::stdout().flush()?;
        eprintln!("Target: {} (from {}), database {}", target.display, target.source, res.database);
    }
    if !res.local && !json {
        eprintln!(
            "warning: database {} is not on this machine (server address {}); check this is not a shared or production database",
            res.database,
            res.server_address.as_deref().unwrap_or("unknown"),
        );
    }
    if dry_run {
        if json {
            print_json(&CleanOutput { target, plan: res, applied: false, report: None })?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !auto_approve {
        if json {
            return Err(ApiError::usage(
                "--format json never asks before cleaning; pass --auto-approve to clean without asking, or --dry-run",
            )
            .into());
        }
        if !std::io::stdin().is_terminal() {
            return Err(ApiError::usage(
                "stdin is not a terminal, so pgmig cannot ask before cleaning; pass --auto-approve to clean without asking",
            )
            .into());
        }
        eprint!("This drops the objects above from database {}. Type its name to confirm: ", res.database);
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !pgmig::clean_confirmed(&answer, &res.database) {
            anyhow::bail!("not cleaned: the answer is not the database name {}", res.database);
        }
    }
    let report = api::apply(&api::ApplyRequest { target: req.target.clone(), plan: res.plan.clone() })?;
    if json {
        print_json(&CleanOutput { target, plan: res, applied: true, report: Some(report) })?;
    } else {
        eprintln!("cleaned: {} statements in {} transaction(s)", report.statements, report.batches);
    }
    Ok(ExitCode::SUCCESS)
}
