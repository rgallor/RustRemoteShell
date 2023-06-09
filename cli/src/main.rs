use clap::{Parser, Subcommand};
use rust_remote_shell::service::Device;
use std::net::SocketAddr;

use color_eyre::Result;

use tracing::Level;
use tracing_subscriber::FmtSubscriber;

use rust_remote_shell::device_server::{Server, TcpConnector};
use rust_remote_shell::sender_client::{Client, ClientConnect, TcpClientConnector};

/// CLI for a rust remote shell
#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

// these commands can be called from the CLI using lowercase Commands name
#[derive(Subcommand, Debug)]
enum Commands {
    /// Make the device listen on a specific IP and port
    Listener {
        addr: SocketAddr,
        #[clap(long, requires("server-cert-file"), requires("privkey-file"))]
        tls_enabled: bool,
        #[clap(long)]
        server_cert_file: Option<String>, // "certs/localhost.local.der" // TODO:: check if Path can be used rather then String
        #[clap(long)]
        privkey_file: Option<String>, // "certs/localhost.local.key.der" // TODO:: check if Path can be used rather then String
    },
    /// Create a client capable of sending command to a Listener
    Sender {
        listener_addr: url::Url,
        #[clap(long, requires("ca-cert-file"))]
        tls_enabled: bool,
        #[clap(long)]
        ca_cert_file: Option<String>, // "certs/CA.der" // TODO:: check if Path can be used rather then String
    },
    Host {
        addr: SocketAddr,
        #[clap(long, requires("server-cert-file"), requires("privkey-file"))]
        tls_enabled: bool,
        #[clap(long)]
        server_cert_file: Option<String>, // "certs/localhost.local.der" // TODO:: check if Path can be used rather then String
        #[clap(long)]
        privkey_file: Option<String>, // "certs/localhost.local.key.der" // TODO:: check if Path can be used rather then String
    },
    Device {
        listener_addr: url::Url,
        #[clap(long, requires("ca-cert-file"))]
        tls_enabled: bool,
        #[clap(long)]
        ca_cert_file: Option<String>, // "certs/CA.der" // TODO:: check if Path can be used rather then String
    },
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

    match cli.command {
        Commands::Listener {
            addr,
            tls_enabled,
            server_cert_file,
            privkey_file,
        } => {
            let device_server = Server::<TcpConnector>::new(addr);
            if tls_enabled {
                println!("TLS");

                // retrieve certificates from the file names given in input and pass them as argument to with_tls()
                let cert = tokio::fs::read(
                    server_cert_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server certificate");

                let privkey = tokio::fs::read(
                    privkey_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server private key");

                device_server
                    .with_tls(cert, privkey)
                    .await?
                    .listen()
                    .await?;
            } else {
                device_server.listen().await?;
            }
        }
        Commands::Sender {
            listener_addr,
            tls_enabled,
            ca_cert_file,
        } => {
            let mut sender_client =
                Client::<TcpClientConnector>::new(listener_addr.clone()).await?;
            if tls_enabled {
                // retrieve certificates from the file names given in input and pass them as argument to with_tls()
                let ca_cert = tokio::fs::read(
                    ca_cert_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server certificate");

                let mut sender_client = sender_client.with_tls(ca_cert).await?;
                sender_client.connector.connect().await?;
                sender_client.handle_connection().await?;
            } else {
                sender_client.connector.connect().await?;
                sender_client.handle_connection().await?;
            }
        }
        Commands::Host {
            addr,
            tls_enabled,
            server_cert_file,
            privkey_file,
        } => {
            let builder = rust_remote_shell::service::Host::bind(addr).await;

            if tls_enabled {
                println!("TLS");

                // retrieve certificates from the file names given in input and pass them as argument to with_tls()
                let cert = tokio::fs::read(
                    server_cert_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server certificate");

                let privkey = tokio::fs::read(
                    privkey_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server private key");

                builder
                    .with_tls(cert, privkey)
                    .await
                    .serve(tower::service_fn(rust_remote_shell::service::host_handle))
                    .await;
            } else {
                builder
                    .serve(tower::service_fn(rust_remote_shell::service::host_handle))
                    .await;
            }
        }
        Commands::Device {
            listener_addr,
            tls_enabled,
            ca_cert_file,
        } => {
            let mut device = Device::new(listener_addr);
            let mut ca_cert: Option<Vec<u8>> = None;

            if tls_enabled {
                ca_cert = Some(
                    tokio::fs::read(
                        ca_cert_file.expect("expected to be called with --tls-enabled option"),
                    )
                    .await
                    .expect("error while reading server certificate"),
                );
            }

            device
                .connect(
                    ca_cert,
                    tower::service_fn(rust_remote_shell::service::device_handle),
                )
                .await;
        }
    }

    Ok(())
}
