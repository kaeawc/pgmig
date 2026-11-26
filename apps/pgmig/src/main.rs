use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use pgmig::api::{self, ApiError};
use pgmig::config::{Config, Settings, Vars};
use pgmig::{Filter, Options, ShadowSpec, Source};
use serde::Serialize;

const AFTER_HELP: &str = "\
Exit codes:
  0  success
  1  error (also: `schema validate` or `migrate validate` found errors)
  2  usage error (unknown flag, missing argument)
  3  `--exit-code` was given and the plan is not empty

With `--format json`, a command prints one JSON document on stdout, and nothing on stderr. A
failure prints the error object ({\"code\", \"message\", \"sqlstate\", \"file\", \"line\", \"col\",
\"apply\"}) on stdout and exits 1, or 2 when its code is \"usage\". Bad flags are reported by the
argument parser as text on stderr (exit 2).

Configuration: every flag except --dry-run, --auto-approve and --exit-code can be set in
pgmig.toml (or the file given to --config), under the flag's name: `url`, `shadow`, `from`, `to`
(also the SOURCE of `schema inspect` and `schema validate`), `include`, `exclude`, `schema`,
`format`, and `dir` as `[migrations] dir`. `[env.NAME]` sections, selected with --env NAME,
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
struct DirArg {
    /// The migration directory (config: `[migrations] dir`).
    #[arg(long)]
    dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Print the SQL that turns one state into another.
    Diff {
        /// SQL file, directory of SQL files, or database URL.
        #[arg(long)]
        from: Option<String>,
        /// SQL file, directory of SQL files, or database URL.
        #[arg(long)]
        to: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        format: FormatArg,
        /// Exit with status 3 when there are changes.
        #[arg(long)]
        exit_code: bool,
    },
    /// Print SQL that recreates a state's schema in a new database.
    Inspect {
        /// SQL file, directory of SQL files, or database URL (config: `to`).
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
        /// SQL file, directory of SQL files, or database URL.
        #[arg(long)]
        to: Option<String>,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Apply without asking. Required when stdin is not a terminal.
        #[arg(long)]
        auto_approve: bool,
        #[command(flatten)]
        format: FormatArg,
        /// With --dry-run, exit with status 3 when there are changes.
        #[arg(long, requires = "dry_run")]
        exit_code: bool,
    },
    /// Load SQL files into the shadow database and report errors; prints nothing on success.
    Validate {
        /// SQL file or directory of SQL files (config: `to`).
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
    /// kind one of schema, table, enum, sequence; `*` and `?` match unquoted names.
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
        })
    }
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
                | SchemaCommand::Apply { format, .. }
                | SchemaCommand::Validate { format, .. },
            )
            | Command::Migrate(MigrateCommand::Hash { format, .. } | MigrateCommand::Validate { format, .. })
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
        SchemaCommand::Apply { url, to, shadow, scope, dry_run, auto_approve, format: _, exit_code } => {
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
                options: scope.options(s)?,
            };
            return schema_apply(req, &shown, dry_run, auto_approve, format, exit_code);
        }
        SchemaCommand::Diff { from, to, shadow, scope, format: _, exit_code } => {
            let req = api::DiffRequest {
                from: Source::parse(&need(from, s.from.clone(), "--from", "from")?),
                to: Source::parse(&need(to, s.to.clone(), "--to", "to")?),
                shadow: shadow.spec(s),
                options: scope.options(s)?,
            };
            let res = api::schema_diff(&req)?;
            match format {
                Format::Text => print!("{}", res.plan.to_sql()),
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

/// Shows the plan, asks (unless told not to), then applies it. In JSON mode it never asks and
/// prints one document at the end.
fn schema_apply(
    req: api::PlanApplyRequest,
    target: &Target,
    dry_run: bool,
    auto_approve: bool,
    format: Format,
    exit_code: bool,
) -> anyhow::Result<ExitCode> {
    let json = format == Format::Json;
    let res = api::schema_plan_apply(&req)?;
    let changes = !res.plan.is_empty();
    let dry_code = if dry_run && exit_code && changes { ExitCode::from(CHANGES) } else { ExitCode::SUCCESS };
    if !changes {
        if json {
            print_json(&ApplyOutput { target, plan: res, applied: false, report: None })?;
        } else {
            eprintln!("no changes");
        }
        return Ok(ExitCode::SUCCESS);
    }
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
        eprintln!("applied {} statements in {} transaction(s)", report.statements, report.batches);
    }
    Ok(ExitCode::SUCCESS)
}
