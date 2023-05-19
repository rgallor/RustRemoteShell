use std::{net::SocketAddr, ops::Deref};

use clap::{Parser, Subcommand};

use color_eyre::Result;
use rust_remote_shell::DeviceServer;

/// CLI for a rust remote shell
#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

// these commands can be called from the CLI using lowercase Commands name
#[derive(Subcommand, Debug)]
enum Commands {
    /// Execute a command
    Command { cmd: String },
    /// Make the device  listen on a specific IP and port
    Listener { addr: SocketAddr },
    /// Create a client capable of sending command to a Listener
    Sender { addr: SocketAddr },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    match &cli.command {
        Commands::Command { cmd } => {
            println!("Input command \"{}\"", cmd); // substitute with logging inside the function

            // parse the cmd into a slice
            let cmd = shellwords::split(cmd.trim())
                .map_err(|_| rust_remote_shell::ShellError::MalformedInput)?;

            let cmd_out = rust_remote_shell::cmd_from_input(cmd.deref())?;
            println!("Command output: {}", cmd_out);
        }
        Commands::Listener { addr } => {
            let device_server = DeviceServer::new(*addr);
            device_server.listen().await?;
        }
        Commands::Sender { addr: _ } => {
            todo!();
            //let sender_client = SenderClient::new(addr);
        }
    }

    Ok(())
}
