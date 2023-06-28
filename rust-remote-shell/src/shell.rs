//! Helper to execute shell commands.
//!
//! This module contains the [`CommandHandler`] struct responsible for the execution of a shell command.

use std::io::{self};
use std::string::FromUtf8Error;

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

    /// Non-existent command.
    #[error("Not existing command.")]
    WrongCommand {
        /// wrong command typed by the host.
        cmd: String,
        /// Io error.
        #[source]
        error: io::Error,
    },

    /// Error while formatting command output into UTF8.
    #[error("Error while formatting command output into UTF8.")]
    WrongOutConversion(#[from] FromUtf8Error),

    /// Error while creating child process to execute command.
    #[error("Error while creating child process to execute command.")]
    CreateChild(#[source] io::Error),

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
    #[instrument(skip(self))]
    pub fn execute(&self, cmd: String) -> Result<ChildStdout, ShellError> {
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
            .map_err(ShellError::CreateChild)?;

        let stdout = child.stdout.take().ok_or(ShellError::RetrieveStdout)?;

        Ok(stdout)
    }

    // TODO: handle errors
    // fn parse() -> Result<String, ShellError> {
    //     let cmd_out = match self.inner_execute(&cmd).await {
    //         Ok(cmd_out) => {
    //             debug!("Output computed");
    //             cmd_out
    //         }
    //         Err(ShellError::EmptyCommand) => {
    //             warn!("Empty command");
    //             return Err(ShellError::EmptyCommand);
    //         }
    //         Err(ShellError::WrongCommand { cmd, error }) => {
    //             warn!("Wrong command: {}", cmd);
    //             return Err(ShellError::WrongCommand { cmd, error });
    //         }
    //         _ => unreachable!("no other error can be thrown"),
    //     };

    //     String::from_utf8(cmd_out.stdout).map_err(|err| {
    //         warn!("Wrong output conversion");
    //         ShellError::WrongOutConversion(err)
    //     })
    // }
}
