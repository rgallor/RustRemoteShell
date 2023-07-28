use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::prelude::*;

use rust_remote_shell::device::Device;
use rust_remote_shell::host::{Host, HostError};

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
        #[clap(long)]
        host_cert_file: Option<PathBuf>,
        #[clap(long)]
        privkey_file: Option<PathBuf>,
    },
    /// Device capable of receiving commands from an host and sending output to it
    Device {
        device_cfg_path: String,
        #[clap(long)]
        ca_cert_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env())
        .try_init()?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Host {
            addr,
            host_cert_file,
            privkey_file,
        } => {
            let host = Host::bind(addr).await?;

            #[cfg(feature = "tls")]
            listen(host, host_cert_file, privkey_file).await?;

            #[cfg(not(feature = "tls"))]
            listen(host).await?;
        }
        Commands::Device {
            device_cfg_path,
            ca_cert_file,
        } => {
            // To make comminicate a device with Astarte use the following command
            // astartectl appengine --appengine-url http://localhost:4002/ --realm-management-url http://localhost:4000/ --realm-key test_private.pem --realm-name test devices send-data 2TBn-jNESuuHamE2Zo1anA org.astarte-platform.rust-remote-shell.ConnectToHost /rshell '{"scheme" : "ws", "host" : "127.0.0.1", "port" : 8080}'
            let mut device = Device::new(device_cfg_path.as_str()).await?;

            // TODO: at the moment we are using the same CA for different hosts, which is wrong.
            // SOLUTION: use astarte send_data to send the CA and retrieve it as an AstarteType of String's array.
            #[cfg(feature = "tls")]
            device.with_tls(ca_cert_file);

            device.start().await?;
        }
    }

    Ok(())
}

#[cfg(feature = "tls")]
async fn listen(
    host: Host,
    host_cert_file: Option<PathBuf>,
    privkey_file: Option<PathBuf>,
) -> Result<(), HostError> {
    // retrieve certificates from the file names given in input and pass them as arguments to with_tls()
    let host_cert_file = host_cert_file.expect("host certificate must be inserted");
    let privkey_file = privkey_file.expect("host certificate must be inserted");

    host.with_tls(host_cert_file, privkey_file)
        .await?
        .listen()
        .await?;

    Ok(())
}

#[cfg(not(feature = "tls"))]
async fn listen(host: Host) -> Result<(), HostError> {
    host.listen().await?;
    Ok(())
}
