#![warn(missing_docs)]
#![doc = include_str!("../README.md")] // TODO: Add crate information in the README

pub mod astarte;
pub mod device;
pub mod host;
pub mod io_handler;
pub mod protocol;
pub mod shell;
pub mod websocket;

// Avoid importing tls module if TLS is not enabled
#[cfg(feature = "tls")]
pub mod tls;
