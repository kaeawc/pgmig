use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pgmig::{ShadowSpec, Source};

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
    },
    /// Print SQL that recreates a state's schema in a new database.
    Inspect {
        /// SQL file, directory of SQL files, or database URL.
        source: String,
        #[command(flatten)]
        shadow: ShadowArg,
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

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Library errors already include their cause in their message; `{e:#}` would repeat it.
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let plan = match cli.command {
        Command::Schema(SchemaCommand::Diff { from, to, shadow }) => {
            pgmig::schema_diff(&Source::parse(&from), &Source::parse(&to), &ShadowSpec::parse(&shadow.shadow))?
        }
        Command::Schema(SchemaCommand::Validate { source, shadow }) => {
            return Ok(pgmig::schema_validate(&Source::parse(&source), &ShadowSpec::parse(&shadow.shadow))?);
        }
        Command::Schema(SchemaCommand::Inspect { source, shadow }) => {
            pgmig::schema_inspect(&Source::parse(&source), &ShadowSpec::parse(&shadow.shadow))?
        }
    };
    print!("{}", plan.to_sql());
    Ok(())
}
