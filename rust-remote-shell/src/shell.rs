use std::ffi::OsStr;
use std::io::{self};
use std::string::FromUtf8Error;

use thiserror::Error;
use tokio::process;
use tracing::{debug, error, instrument, warn};

#[derive(Error, Debug)]
pub enum ShellError {
    #[error("Empty command")]
    EmptyCommand,
    #[error("Malformed input")]
    MalformedInput,
    #[error("Command {cmd} does not exists")]
    WrongCommand {
        cmd: String,
        #[source]
        error: io::Error,
    },
    #[error("Error while formatting command output into UTF8")]
    WrongOutConversion(#[from] FromUtf8Error), // TODO: find a way to test it
}

#[derive(Default)]
pub struct CommandHandler;

impl CommandHandler {
    #[instrument(skip(self))]
    pub async fn execute(&self, cmd: String) -> Result<String, ShellError> {
        debug!("Execute command {}", cmd);
        let cmd = shellwords::split(&cmd).map_err(|_| {
            warn!("Malformed input");
            ShellError::MalformedInput
        })?;
        let cmd_out = match self.inner_execute(&cmd).await {
            Ok(cmd_out) => {
                debug!("Output computed");
                cmd_out
            }
            Err(ShellError::EmptyCommand) => {
                warn!("Empty command");
                return Err(ShellError::EmptyCommand);
            }
            Err(ShellError::WrongCommand { cmd, error }) => {
                warn!("Wrong command: {}", cmd);
                return Err(ShellError::WrongCommand { cmd, error });
            }
            _ => unreachable!("no other error can be thrown"),
        };
        String::from_utf8(cmd_out.stdout).map_err(|err| {
            warn!("Wrong output conversion");
            ShellError::WrongOutConversion(err)
        })
    }

    async fn inner_execute<S>(&self, cmd: &[S]) -> Result<std::process::Output, ShellError>
    where
        S: AsRef<OsStr>,
    {
        let mut cmd_iter = cmd.iter();
        let cmd_to_exec = cmd_iter.next().ok_or(ShellError::EmptyCommand)?;

        process::Command::new(cmd_to_exec)
            .args(cmd_iter)
            .output()
            .await
            .map_err(|e| ShellError::WrongCommand {
                cmd: cmd_to_exec.as_ref().to_string_lossy().to_string(),
                error: e,
            })
    }
}
