use std::{collections::HashMap, net::SocketAddrV4, str::FromStr};

use astarte_device_sdk::{
    options::{AstarteOptions, AstarteOptionsError},
    types::AstarteType,
    AstarteAggregate, AstarteDeviceSdk, AstarteError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
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
    #[error("Parse url error, {0}")]
    ParseUrl(&'static str),
}

#[derive(Serialize, Deserialize)]
pub struct DeviceConfig {
    realm: String,
    device_id: String,
    credentials_secret: String,
    pairing_url: String,
}

#[derive(Debug)]
pub struct DataAggObject {
    ip: String,
    port: i32,
}

impl AstarteAggregate for DataAggObject {
    fn astarte_aggregate(
        self,
    ) -> Result<
        std::collections::HashMap<String, astarte_device_sdk::types::AstarteType>,
        AstarteError,
    > {
        let mut hm = HashMap::new();
        hm.insert("ip".to_string(), self.ip.try_into()?);
        hm.insert("port".to_string(), self.port.try_into()?);
        Ok(hm)
    }
}

impl TryFrom<DataAggObject> for Url {
    type Error = Error;

    fn try_from(value: DataAggObject) -> Result<Self, Self::Error> {
        Url::parse(&format!("ws://{}:{}", value.ip, value.port))
            .map_err(|_| Error::ParseUrl("incorrect format"))
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
        let ip = map.get("ip").ok_or(Error::ParseUrl("Missing IP address"))?;
        let port = map
            .get("port")
            .ok_or(Error::ParseUrl("Missing port value"))?;

        let data = match (ip, port) {
            (AstarteType::String(ip), AstarteType::Integer(port)) => {
                // build a socket to check if the IP and the port number are correct
                let _socket = SocketAddrV4::from_str(&format!("{ip}:{port}"))
                    .map_err(|_| Error::ParseUrl("Received a wrong IP address or port"))?;

                DataAggObject {
                    ip: ip.to_string(),
                    port: *port,
                }
            }
            _ => return Err(Error::AstarteWrongType),
        };

        data.try_into()
    }
}
