//! `foobar`: one binary, three tools on the same core.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use foobar_language_server::{cli, server};

/// Language tooling for the foobar config DSL.
#[derive(Parser)]
#[command(name = "foobar", version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the language server over stdin/stdout.
    Lsp {
        /// Accepted and ignored: stdio is the only transport, but VS Code's client passes it.
        #[arg(long, hide = true)]
        stdio: bool,
    },
    /// Lint .foobar files; exits with 1 if any error was found.
    Lint {
        /// Files or directories to report on [default: the root].
        paths: Vec<PathBuf>,
        /// Workspace scanned for context: lookups and reference counts see every file in it.
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Format files in place.
    Fmt {
        /// Only list files that would change; exits with 1 if any would.
        #[arg(long)]
        check: bool,
        /// Files or directories to format.
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    let result = match Args::parse().command {
        Command::Lsp { stdio: _ } => server::run().map(|()| ExitCode::SUCCESS),
        Command::Lint { paths, root } => cli::lint(&root, &paths),
        Command::Fmt { check, paths } => cli::fmt(&paths, check),
    };
    result.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        ExitCode::from(2)
    })
}
