use eyre::Result;
use std::ffi::OsString;
use std::fmt::Display;
use std::{ffi::OsStr, io::BufRead};
use thiserror::Error;
use tokio::process::Command;
use tracing::debug;

#[derive(Error, Debug)]
pub enum CmdError {
    #[error("Failed to run command `{cmd:?}: {err:#}")]
    SpawnFailed { cmd: String, err: eyre::Report },
    #[error("Command `{0}` failed with no exit code")]
    NoExitCode(String),
    #[error("Command `{cmd}` failed with exit code {code}: {stderr}")]
    Failed {
        cmd: String,
        code: i32,
        stderr: String,
    },
}

/// Runs the specified command and returns error on failure.
/// **IMPORTANT**: Whenever you use new CLI tool in BV,
/// remember to add it to requirements check in `installer::check_cli_dependencies()`.
pub async fn run_cmd<I, S>(cmd: &str, args: I) -> Result<String, CmdError>
where
    I: IntoIterator<Item = S> + Clone,
    S: AsRef<OsStr>,
{
    debug!(
        "Running command: `{cmd}{}`",
        args.clone()
            .into_iter()
            .fold(OsString::new(), |mut v, item| {
                v.push(" \"");
                v.push(item);
                v.push("\"");
                v
            })
            .to_string_lossy()
    );
    let mut command = Command::new(cmd);
    command.args(args);
    let output = command
        .output()
        .await
        .map_err(|err| CmdError::SpawnFailed {
            cmd: cmd.to_string(),
            err: err.into(),
        })?;
    match output.status.code() {
        Some(code) if code != 0 => Err(CmdError::Failed {
            cmd: cmd.to_string(),
            code,
            stderr: String::from_utf8(output.stderr).unwrap_or_default(),
        }),
        Some(_) => {
            let stdout =
                String::from_utf8(output.stdout).unwrap_or("stdout is invalid UTF-8".to_string());
            Ok(stdout)
        }
        None => Err(CmdError::NoExitCode(cmd.to_string())),
    }
}

/// Requests confirmation from the user, i.e. the user must type `y` to continue.
///
/// ### Params
/// msg:    The message that is displayed to the user to request access. On display this function
///         will append ` [y/N]` to the message.
/// dash_y: If this flag is true, requesting user input is skipped and `true` is immediately
///         returned.
pub fn ask_confirm(msg: &str, dash_y: bool) -> Result<bool> {
    if dash_y {
        return Ok(true);
    }
    println!("{msg} [y/N]:");
    let mut input = String::new();
    std::io::stdin().lock().read_line(&mut input)?;
    Ok(input.trim().to_lowercase() == "y")
}

/// Requests value from the user, i.e. the user must type in value or press `Return` to accept default value.
///
/// ### Params
/// msg:    The message that is displayed to the user to request access. On display this function
///         will append ` [default_value]` to the message.
/// default_value: The default value.
/// dash_y: If this flag is true, requesting user input is skipped and `None` is immediately
///         returned.
pub fn ask_value(msg: &str, default_value: &impl Display, dash_y: bool) -> Result<Option<String>> {
    if dash_y {
        return Ok(None);
    }
    println!("{msg} [{default_value}]:");
    let mut input = String::new();
    std::io::stdin().lock().read_line(&mut input)?;
    let value = input.trim().to_string();
    Ok(if value.is_empty() { None } else { Some(value) })
}
