use std::ffi::OsStr;
use std::io;
use std::process;
use std::string::FromUtf8Error;

use thiserror::Error;

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
    #[error("The execution of the command caused an error while formatting the output into UTF8")]
    WrongOutConversion(#[from] FromUtf8Error),
}

/*

fn main() {
    todo!();

    // Read the input args and take the command
    let args = env::args();
    let cmd = cmd_from_input(args).map_err(|e| eprintln!("Error: {:?}", e));

    // Call function to execute the input command and save its output
    match execute_cmd(&cmd) {
        // print output command
        Ok(cmd_out) => println!("Command output: {}", cmd_out),
        Err(e) => eprintln!("Error: {:?}", e),
    }
}

*/

fn execute_cmd<S>(cmd: &[S]) -> Result<process::Output, ShellError>
where
    S: AsRef<OsStr>,
{
    let mut cmd_iter = cmd.iter();
    let cmd_to_exec = cmd_iter.next().ok_or(ShellError::EmptyCommand)?;

    process::Command::new(cmd_to_exec)
        .args(cmd_iter)
        .output()
        .map_err(|e| ShellError::WrongCommand {
            cmd: cmd_to_exec.as_ref().to_string_lossy().to_string(),
            error: e,
        })
}

pub fn cmd_from_input<S>(cmd: &[S]) -> Result<String, ShellError>
where
    S: AsRef<OsStr>,
{
    // before calling this function the binary should ensure that the input in in the correct sintactic format

    // try executing the command.
    // If the error states that the command does not exists, throw WrongCommand(cmd.split(' ').first().unwrap())
    let cmd_out = execute_cmd(cmd)?;

    Ok(std::string::String::from_utf8(cmd_out.stdout)
        // if the conversion from UTF8 to String goes wrong, return an error
        .map_err(|e| ShellError::WrongOutConversion(e))?)
}
