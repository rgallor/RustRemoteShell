use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

use rust_remote_shell::{device_server::DeviceServer, sender_client::SenderClient};

/// CLI for a rust remote shell
#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

// these commands can be called from the CLI using lowercase Commands name
#[derive(Subcommand, Debug)]
enum Commands {
    /// Make the device  listen on a specific IP and port
    Listener { addr: SocketAddr },
    /// Create a client capable of sending command to a Listener
    Sender { listener_addr: url::Url },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // define a subscriber for logging purposes
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let cli = Cli::parse();

    match &cli.command {
        Commands::Listener { addr } => {
            let device_server = DeviceServer::new(*addr);
            device_server.listen().await?;
        }
        Commands::Sender { listener_addr } => {
            let mut sender_client = SenderClient::new(listener_addr.clone());
            sender_client.connect().await?;
        }
    }

    Ok(())
}
