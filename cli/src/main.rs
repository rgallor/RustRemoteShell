use std::ops::Deref;

use clap::{Parser, Subcommand};

use color_eyre::Result;
use shellwords;

// use the library containing the rust remote shell functions
use rust_remote_shell;

/// Shell CLI
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Device
    #[arg(short, long, required = true,  value_parser = clap::builder::NonEmptyStringValueParser::new())]
    device: String, // not an Option<String> because the device id/name is required

    #[command(subcommand)]
    command: Commands,
}

// these commands canbe called from the CLI using lowercase Commands name
#[derive(Subcommand, Debug)]
enum Commands {
    /// Execute a command
    Command { cmd: String },
    // Authenticate device
    // Authenticate { token: String },
    // Logout device
    // Logout,
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
        } // Commands::Authenticate { token } => {
          //     println!("Authenticate device {}", cli.device)
          // }
          // Commands::Logout => { ACTIONS TO LOG OUT }
    }

    Ok(())
}
