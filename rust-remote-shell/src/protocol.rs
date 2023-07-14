//! Protocol describing the Host and Device actions after a connection has been established.

use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message;

use crate::{device::DeviceError, host::HostError};

/// Enum used to describe the messages a host can send to a device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum HostMsg {
    /// Close the WebSocket connection.
    Exit,
    /// Execute a command.
    Command {
        /// Command to execute.
        cmd: String,
    },
    /// CTRL C signal sent
    Ctrlc,
}

impl TryFrom<HostMsg> for Message {
    type Error = HostError;

    fn try_from(value: HostMsg) -> Result<Self, Self::Error> {
        let msg = bson::to_vec(&value)?;
        Ok(Message::Binary(msg))
    }
}

impl TryFrom<Message> for HostMsg {
    type Error = DeviceError;

    fn try_from(value: Message) -> Result<Self, Self::Error> {
        match value {
            Message::Close(_) => Ok(HostMsg::Exit),
            Message::Binary(msg) => {
                let host_msg: HostMsg = bson::from_slice(&msg)?;
                Ok(host_msg)
            }
            msg => Err(DeviceError::WrongWsMessage(msg)),
        }
    }
}

/// Enum used to describe the messages a device can send to a host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum DeviceMsg {
    /// The output of a command is ready.
    Output(Vec<u8>),
    /// EOF received.
    Eof,
    /// Error.
    Err(String),
}

impl TryFrom<DeviceMsg> for Message {
    type Error = DeviceError;

    fn try_from(value: DeviceMsg) -> Result<Self, Self::Error> {
        Ok(Message::Binary(bson::to_vec(&value)?))
    }
}
