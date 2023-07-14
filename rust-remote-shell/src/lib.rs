#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod astarte;
pub mod command;
pub mod device;
pub mod host;
pub mod protocol;
pub mod shell;
mod stream;
pub mod websocket;

// Avoid importing tls module if TLS is not enabled
#[cfg(feature = "tls")]
pub mod tls;
