use clap::{Parser, Subcommand};

use std::net::SocketAddr;
use std::path::PathBuf;

use color_eyre::Result;

use tracing::Level;
use tracing_subscriber::FmtSubscriber;

use rust_remote_shell::device::Device;
use rust_remote_shell::host::Host;

/// CLI for a rust remote shell
#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

/// Rust remote shell commands
#[derive(Subcommand, Debug)]
enum Commands {
    /// Host waiting for a device connection
    Host {
        addr: SocketAddr,
        #[clap(long, requires("host-cert-file"), requires("privkey-file"))]
        tls_enabled: bool,
        #[clap(long)]
        host_cert_file: Option<PathBuf>,
        #[clap(long)]
        privkey_file: Option<PathBuf>,
    },
    /// Device capable of receiving commands from an host and sending output to it
    Device {
        device_cfg_path: String,
        #[clap(long)]
        tls_enabled: bool,
        #[clap(long)]
        ca_cert_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // tracing subscriber for logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let cli = Cli::parse();

    match cli.command {
        Commands::Host {
            addr,
            tls_enabled,
            host_cert_file,
            privkey_file,
        } => {
            let builder = Host::bind(addr).await?;

            if tls_enabled {
                // retrieve certificates from the file names given in input and pass them as argument to with_tls()
                let host_cert_file = host_cert_file.expect("host certificate must be inserted");
                let privkey_file = privkey_file.expect("host certificate must be inserted");

                builder
                    .with_tls(host_cert_file, privkey_file)
                    .await?
                    .serve()
                    .await?;
            } else {
                builder.serve().await?;
            }
        }
        Commands::Device {
            device_cfg_path,
            tls_enabled,
            ca_cert_file,
        } => {
            // To make comminicate a device with Astarte use the following command
            // astartectl appengine --appengine-url http://localhost:4002/ --realm-management-url http://localhost:4000/ --realm-key test_private.pem --realm-name test devices send-data 2TBn-jNESuuHamE2Zo1anA org.astarte-platform.rust-remote-shell.ConnectToHost /rshell '{"scheme" : "ws", "host" : "127.0.0.1", "port" : 8080}'
            let mut device = Device::new(device_cfg_path.as_str()).await?;

            if tls_enabled {
                // if an error occurred while reading the certficate, only webpki certs will be included into the root_certs
                device.connect_tls(ca_cert_file).await?;
            } else {
                device.connect().await?;
            }
        }
    }

    Ok(())
}
