//! Helper to execute shell commands.
//!
//! This module contains the [`CommandHandler`] struct responsible for the execution of a shell command.

use std::io;

use thiserror::Error;
use tokio::process::{self, Child, ChildStdout};
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

    /// Error while chilling child process.
    #[error("Error while killing child process.")]
    KillChild(#[from] io::Error),

    /// Error occurring when trying to CTRL C in case no command is executed.
    #[error("CTRL C without executing any command.")]
    NoCommand,
}

/// Struct responsible for handling commands sent by a host.
#[derive(Default)]
pub struct CommandHandler {
    child: Option<Child>,
}

impl CommandHandler {
    /// Execute a shell command.
    ///
    /// This method is called by a device after having received a command from a host.
    /// It checks if the received command command is correct and it eventually creates a [Child](tokio::process::Child) process
    /// responsible for executing the command. The command output is retrieved from the stdout of the child process and returned
    /// to the device.
    #[instrument(skip_all)]
    pub fn execute(&mut self, cmd: String) -> Result<ChildStdout, ShellError> {
        debug!("Execute command {}", cmd);

        let cmd = shellwords::split(&cmd).map_err(|_| {
            warn!("Malformed input");
            ShellError::MalformedInput
        })?;

        let mut cmd_iter = cmd.iter();
        let cmd_to_exec = cmd_iter.next().ok_or(ShellError::EmptyCommand)?;

        self.child = Some(
            process::Command::new(cmd_to_exec)
                .args(cmd_iter)
                .stdout(std::process::Stdio::piped())
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|err| ShellError::CreateChild {
                    cmd: cmd_to_exec.into(),
                    err,
                })?,
        );

        let stdout = self
            .child
            .as_mut()
            .expect("child must have been initialized")
            .stdout
            .take()
            .ok_or(ShellError::RetrieveStdout)?;

        Ok(stdout)
    }

    /// Set the Child process to None.
    ///
    /// This method is usefull in case a CTRL C command is received, making it possible
    /// to avoid interrupting an already executed command.
    pub fn empty(&mut self) {
        self.child = None;
    }

    /// Kill the process responsible for handling a command in case of a CTRL C
    pub async fn ctrl_c(&mut self) -> Result<(), ShellError> {
        match self.child {
            Some(_) => {
                let mut child = self.child.take().unwrap();
                Ok(child.kill().await?)
            }
            None => Err(ShellError::NoCommand),
        }
    }
}
