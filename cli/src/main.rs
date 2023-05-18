use std::ops::Deref;

use clap::{Parser, Subcommand};

use color_eyre::Result;

/// CLI for a rust remote shell
#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

// these commands canbe called from the CLI using lowercase Commands name
#[derive(Subcommand, Debug)]
enum Commands {
    /// Execute a command
    Command { cmd: String },
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    match &cli.command {
        Commands::Command { cmd } => {
            println!("Input command \"{}\"", cmd); // substitute with logging inside the function

            // parse the cmd into a slice
            let cmd = shellwords::split(cmd.trim())
                .or(Err(rust_remote_shell::ShellError::MalformedInput))?;

            let cmd_out = rust_remote_shell::cmd_from_input(cmd.deref())?;
            println!("Command output: {}", cmd_out);
        }
    }

    Ok(())
}
