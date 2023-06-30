//! Helper to execute shell commands.
//!
//! This module contains the [`CommandHandler`] struct responsible for the execution of a shell command.

use std::io;

use thiserror::Error;
use tokio::process::{self, ChildStdout};
use tracing::{debug, error, instrument, warn};

/// Shell errors.
#[derive(Error, Debug)]
pub enum ShellError {
    /// Empty command.
    #[error("Empty command.")]
    EmptyCommand,

    /// Malformed input.
    #[error("Malformed input.")]
    MalformedInput,

    /// Error while creating child process to execute command.
    #[error("Error while creating child process to execute command {cmd}")]
    CreateChild {
        /// wrong command typed by the host.
        cmd: String,
        /// Io error.
        #[source]
        err: io::Error,
    },

    /// Error while retrieving stdout from child process.
    #[error("Error while retrieving stdout from child process.")]
    RetrieveStdout,
}

/// Struct responsible for handling commands sent by a host.
#[derive(Default)]
pub struct CommandHandler;

impl CommandHandler {
    /// Execute a shell command.
    ///
    /// This method is called by a device after having received a command from a host.
    /// It checks if the received command command is correct and it eventually creates a [Child](tokio::process::Child) process
    /// responsible for executing the command. The command output is retrieved from the stdout of the child process and returned
    /// to the device.
    #[instrument(skip_all)]
    pub fn execute(cmd: String) -> Result<ChildStdout, ShellError> {
        debug!("Execute command {}", cmd);

        let cmd = shellwords::split(&cmd).map_err(|_| {
            warn!("Malformed input");
            ShellError::MalformedInput
        })?;

        let mut cmd_iter = cmd.iter();
        let cmd_to_exec = cmd_iter.next().ok_or(ShellError::EmptyCommand)?;

        let mut child = process::Command::new(cmd_to_exec)
            .args(cmd_iter)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| ShellError::CreateChild {
                cmd: cmd_to_exec.into(),
                err,
            })?;

        let stdout = child.stdout.take().ok_or(ShellError::RetrieveStdout)?;

        Ok(stdout)
    }
}
