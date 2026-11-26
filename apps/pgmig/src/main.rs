use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pgmig::api::{self, ApiError};
use pgmig::{Filter, Options, ShadowSpec, Source};

#[derive(Parser)]
#[command(name = "pgmig", version, about = "Postgres schema migrations from plain SQL")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with schema states (SQL files or live databases).
    #[command(subcommand)]
    Schema(SchemaCommand),
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
    },
    /// Print SQL that recreates a state's schema in a new database.
    Inspect {
        /// SQL file, directory of SQL files, or database URL.
        source: String,
        #[command(flatten)]
        shadow: ShadowArg,
        #[command(flatten)]
        scope: ScopeArgs,
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
    },
    /// Load SQL files into the shadow database and report errors; prints nothing on success.
    Validate {
        /// SQL file or directory of SQL files.
        source: String,
        #[command(flatten)]
        shadow: ShadowArg,
    },
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
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            // Library errors already include their cause in their message; `{e:#}` would repeat it.
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Builds each command's request from the flags, calls the library and prints the response.
fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let Command::Schema(command) = cli.command;
    match command {
        SchemaCommand::Apply { url, to, shadow, scope, dry_run, auto_approve } => {
            let req = api::PlanApplyRequest {
                target: url,
                desired: Source::parse(&to),
                shadow: ShadowSpec::parse(&shadow.shadow),
                options: scope.options()?,
            };
            schema_apply(req, dry_run, auto_approve)?;
        }
        SchemaCommand::Diff { from, to, shadow, scope } => {
            let req = api::DiffRequest {
                from: Source::parse(&from),
                to: Source::parse(&to),
                shadow: ShadowSpec::parse(&shadow.shadow),
                options: scope.options()?,
            };
            print!("{}", api::schema_diff(&req)?.plan.to_sql());
        }
        SchemaCommand::Validate { source, shadow } => {
            let req = api::ValidateRequest { source: Source::parse(&source), shadow: ShadowSpec::parse(&shadow.shadow) };
            let res = api::schema_validate(&req)?;
            for d in &res.diagnostics {
                eprintln!("error: {d}");
            }
            if !res.is_valid() {
                return Ok(ExitCode::FAILURE);
            }
        }
        SchemaCommand::Inspect { source, shadow, scope } => {
            let req = api::InspectRequest {
                source: Source::parse(&source),
                shadow: ShadowSpec::parse(&shadow.shadow),
                options: scope.options()?,
            };
            print!("{}", api::schema_inspect(&req)?.plan.to_sql());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Shows the plan, asks (unless told not to), then applies it.
fn schema_apply(req: api::PlanApplyRequest, dry_run: bool, auto_approve: bool) -> anyhow::Result<()> {
    let plan = api::schema_plan_apply(&req)?.plan;
    if plan.is_empty() {
        eprintln!("no changes");
        return Ok(());
    }
    print!("{}", plan.to_sql());
    std::io::stdout().flush()?;
    if dry_run {
        return Ok(());
    }
    let n = plan.statements.len();
    if !auto_approve {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("stdin is not a terminal, so pgmig cannot ask before applying; pass --auto-approve to apply without asking");
        }
        eprint!("Apply these {n} statements? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES") {
            anyhow::bail!("not applied");
        }
    }
    let report = api::apply(&api::ApplyRequest { target: req.target, plan })?;
    eprintln!("applied {} statements in {} transaction(s)", report.statements, report.batches);
    Ok(())
}
