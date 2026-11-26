use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use pgmig::api::{self, ApiError};
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
argument parser as text on stderr (exit 2).";

#[derive(Parser)]
#[command(name = "pgmig", version, about = "Postgres schema migrations from plain SQL", after_help = AFTER_HELP)]
struct Cli {
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
        /// the URL of an empty Postgres database.
        #[arg(long)]
        shadow: Option<String>,
        #[command(flatten)]
        format: FormatArg,
    },
}

#[derive(clap::Args)]
struct DirArg {
    /// The migration directory.
    #[arg(long)]
    dir: std::path::PathBuf,
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Print the SQL that turns one state into another.
    Diff {
        /// SQL file, directory of SQL files, or database URL.
        #[arg(long)]
        from: String,
        /// SQL file, directory of SQL files, or database URL.
        #[arg(long)]
        to: String,
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
        /// SQL file, directory of SQL files, or database URL.
        source: String,
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
        url: String,
        /// SQL file, directory of SQL files, or database URL.
        #[arg(long)]
        to: String,
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
        /// SQL file or directory of SQL files.
        source: String,
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
    /// Output format: `text`, or `json` (one JSON document on stdout; errors too).
    #[arg(long, value_enum, default_value = "text")]
    format: Format,
}

#[derive(clap::Args)]
struct ShadowArg {
    /// Where SQL files are executed: `pgmem` (embedded) or the URL of an empty Postgres database.
    #[arg(long, default_value = "pgmem")]
    shadow: String,
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
    fn options(&self) -> anyhow::Result<Options> {
        Ok(Options { filter: Filter::parse(&self.include, &self.exclude).map_err(ApiError::from)?, schema: self.schema.clone() })
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = cli.format();
    match run(cli) {
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
    fn format(&self) -> Format {
        match &self.command {
            Command::Schema(
                SchemaCommand::Diff { format, .. }
                | SchemaCommand::Inspect { format, .. }
                | SchemaCommand::Apply { format, .. }
                | SchemaCommand::Validate { format, .. },
            )
            | Command::Migrate(MigrateCommand::Hash { format, .. } | MigrateCommand::Validate { format, .. }) => format.format,
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Status 3: `--exit-code` and a plan with statements.
const CHANGES: u8 = 3;

/// Builds each command's request from the flags, calls the library and prints the response.
fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let command = match cli.command {
        Command::Schema(command) => command,
        Command::Migrate(command) => return migrate(command),
    };
    match command {
        SchemaCommand::Apply { url, to, shadow, scope, dry_run, auto_approve, format, exit_code } => {
            let req = api::PlanApplyRequest {
                target: url,
                desired: Source::parse(&to),
                shadow: ShadowSpec::parse(&shadow.shadow),
                options: scope.options()?,
            };
            return schema_apply(req, dry_run, auto_approve, format.format, exit_code);
        }
        SchemaCommand::Diff { from, to, shadow, scope, format, exit_code } => {
            let req = api::DiffRequest {
                from: Source::parse(&from),
                to: Source::parse(&to),
                shadow: ShadowSpec::parse(&shadow.shadow),
                options: scope.options()?,
            };
            let res = api::schema_diff(&req)?;
            match format.format {
                Format::Text => print!("{}", res.plan.to_sql()),
                Format::Json => print_json(&res)?,
            }
            if exit_code && !res.plan.is_empty() {
                return Ok(ExitCode::from(CHANGES));
            }
        }
        SchemaCommand::Validate { source, shadow, format } => {
            let req = api::ValidateRequest { source: Source::parse(&source), shadow: ShadowSpec::parse(&shadow.shadow) };
            let res = api::schema_validate(&req)?;
            match format.format {
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
        SchemaCommand::Inspect { source, shadow, scope, format } => {
            let req = api::InspectRequest {
                source: Source::parse(&source),
                shadow: ShadowSpec::parse(&shadow.shadow),
                options: scope.options()?,
            };
            let res = api::schema_inspect(&req)?;
            match format.format {
                Format::Text => print!("{}", res.plan.to_sql()),
                Format::Json => print_json(&res)?,
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn migrate(command: MigrateCommand) -> anyhow::Result<ExitCode> {
    match command {
        MigrateCommand::Hash { dir, format } => {
            let res = api::migrate_hash(&api::MigrateHashRequest { dir: dir.dir })?;
            match format.format {
                Format::Text if res.written => {
                    let n = res.files.len();
                    eprintln!("wrote {} ({n} file{})", res.sum_file.display(), if n == 1 { "" } else { "s" });
                }
                Format::Text => eprintln!("{} is up to date", res.sum_file.display()),
                Format::Json => print_json(&res)?,
            }
        }
        MigrateCommand::Validate { dir, shadow, format } => {
            let req = api::MigrateValidateRequest { dir: dir.dir, shadow: shadow.as_deref().map(ShadowSpec::parse) };
            let res = api::migrate_validate(&req)?;
            match format.format {
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

/// What `schema apply --format json` prints: the plan, and the report when it was applied.
#[derive(Serialize)]
struct ApplyOutput {
    #[serde(flatten)]
    plan: api::PlanApplyResponse,
    applied: bool,
    report: Option<pgmig::ApplyReport>,
}

/// Shows the plan, asks (unless told not to), then applies it. In JSON mode it never asks and
/// prints one document at the end.
fn schema_apply(
    req: api::PlanApplyRequest,
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
            print_json(&ApplyOutput { plan: res, applied: false, report: None })?;
        } else {
            eprintln!("no changes");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !json {
        print!("{}", res.plan.to_sql());
        std::io::stdout().flush()?;
    }
    if dry_run {
        if json {
            print_json(&ApplyOutput { plan: res, applied: false, report: None })?;
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
        eprint!("Apply these {n} statements? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES") {
            anyhow::bail!("not applied");
        }
    }
    let report = api::apply(&api::ApplyRequest { target: req.target, plan: res.plan.clone() })?;
    if json {
        print_json(&ApplyOutput { plan: res, applied: true, report: Some(report) })?;
    } else {
        eprintln!("applied {} statements in {} transaction(s)", report.statements, report.batches);
    }
    Ok(ExitCode::SUCCESS)
}
