use std::{collections::HashMap, fmt::Display, net::AddrParseError, num::TryFromIntError};

use astarte_device_sdk::{
    options::{AstarteOptions, AstarteOptionsError},
    types::AstarteType,
    AstarteAggregate, AstarteDeviceSdk, AstarteError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;
use url::Url;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Error while loading the Astarte Interfaces")]
    AstarteInterface(#[from] AstarteOptionsError),
    #[error("Error while creating an Astarte device")]
    AstarteCreateDevice(#[from] AstarteError),
    #[error("Error while handling Astarte events")]
    AstarteHandleEvent(#[source] AstarteError),
    #[error("Received Individual aggregation data type")]
    AstarteWrongAggregation,
    #[error("Received wrong astarte type")]
    AstarteWrongType,
    #[error("Error while reading a file")]
    ReadFile(#[from] tokio::io::Error),
    #[error("Error while serializing/deserializing with serde")]
    Serde,
    #[error("Error while parsing url")]
    Parse(#[from] url::ParseError),
    #[error("Wrong scheme, {0}")]
    ParseScheme(String),
    #[error("Error while parsing the ip address")]
    ParseAddr(#[from] AddrParseError),
    #[error("Error while parsing the port number")]
    ParsePort(#[from] TryFromIntError),
    #[error("Missing url information")]
    MissingUrlInfo(String),
}

#[derive(Serialize, Deserialize)]
pub struct DeviceConfig {
    realm: String,
    device_id: String,
    credentials_secret: String,
    pairing_url: String,
}

#[derive(Debug, Clone, Copy)]
enum Scheme {
    Ws,
    WsSecure,
}

impl Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scheme::Ws => write!(f, "ws"),
            Scheme::WsSecure => write!(f, "wss"),
        }
    }
}

impl TryFrom<&str> for Scheme {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "ws" => Ok(Self::Ws),
            "wss" => Ok(Self::WsSecure),
            _ => Err(Self::Error::ParseScheme(value.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DataAggObject {
    scheme: Scheme,
    host: url::Host,
    port: u16,
}

impl AstarteAggregate for DataAggObject {
    fn astarte_aggregate(
        self,
    ) -> Result<
        std::collections::HashMap<String, astarte_device_sdk::types::AstarteType>,
        AstarteError,
    > {
        let mut hm = HashMap::new();
        hm.insert("scheme".to_string(), self.scheme.to_string().try_into()?);
        hm.insert("host".to_string(), self.host.to_string().try_into()?);
        hm.insert("port".to_string(), AstarteType::Integer(self.port.into()));
        Ok(hm)
    }
}

impl TryFrom<DataAggObject> for Url {
    type Error = Error;

    fn try_from(value: DataAggObject) -> Result<Self, Self::Error> {
        let ip = match value.host {
            url::Host::Domain(domain) => domain,
            // the IP 127.0.0.1 cannot be used due to a low MSRV, therefore the IP is converted to a domain name
            url::Host::Ipv4(ipv4) if std::net::Ipv4Addr::LOCALHOST == ipv4 => {
                "localhost.local".to_string()
            }
            host => host.to_string(),
        };
        Url::parse(&format!("{}://{}:{}", value.scheme, ip, value.port)).map_err(Error::Parse)
    }
}

pub struct HandleAstarteConnection;

impl HandleAstarteConnection {
    pub async fn read_device_config(&self, device_cfg_path: &str) -> Result<DeviceConfig, Error> {
        let file = tokio::fs::read(device_cfg_path)
            .await
            .map_err(Error::ReadFile)?;
        let file = std::str::from_utf8(&file).unwrap();

        let cfg: DeviceConfig = serde_json::from_str(file).map_err(|_| Error::Serde)?;

        Ok(cfg)
    }

    pub async fn create_astarte_device(
        &self,
        cfg: &DeviceConfig,
    ) -> Result<AstarteDeviceSdk, Error> {
        let sdk_options = AstarteOptions::new(
            &cfg.realm,
            &cfg.device_id,
            &cfg.credentials_secret,
            &cfg.pairing_url,
        )
        .interface_directory("./rust-remote-shell/interfaces")
        .map_err(Error::AstarteInterface)?
        .ignore_ssl_errors();

        let device = AstarteDeviceSdk::new(&sdk_options)
            .await
            .map_err(Error::AstarteCreateDevice)?;

        Ok(device)
    }

    pub fn retrieve_url(&self, map: HashMap<String, AstarteType>) -> Result<Url, Error> {
        let scheme = map
            .get("scheme")
            .ok_or_else(|| Error::MissingUrlInfo("Missing scheme".to_string()))?;
        let host: &AstarteType = map
            .get("host")
            .ok_or_else(|| Error::MissingUrlInfo("Missing host (IP or domain name)".to_string()))?;
        let port = map
            .get("port")
            .ok_or_else(|| Error::MissingUrlInfo("Missing port value".to_string()))?;

        let data = match (scheme, host, port) {
            (
                AstarteType::String(scheme),
                AstarteType::String(host),
                AstarteType::Integer(port),
            ) => {
                let scheme = Scheme::try_from(scheme.as_ref())?;
                let host = url::Host::parse(host).map_err(Error::Parse)?;
                debug!("{:?}", host);
                let port: u16 = (*port).try_into().map_err(Error::ParsePort)?;

                DataAggObject { scheme, host, port }
            }
            _ => return Err(Error::AstarteWrongType),
        };

        data.try_into()
    }
}
