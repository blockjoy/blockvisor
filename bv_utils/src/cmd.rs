use eyre::{bail, Context, Result};
use std::{ffi::OsStr, io::BufRead};
use tokio::process::Command;
use tracing::info;

/// Runs the specified command and returns error on failure.
/// **IMPORTANT**: Whenever you use new CLI tool in BV,
/// remember to add it to requirements check in `installer::check_cli_dependencies()`.   
pub async fn run_cmd<I, S>(cmd: &str, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(cmd);
    cmd.args(args);
    info!("Running command: `{:?}`", cmd);
    let output = cmd
        .output()
        .await
        .with_context(|| format!("Failed to run command `{cmd:?}`"))?;
    match output.status.code() {
        Some(code) if code != 0 => bail!("Command `{cmd:?}` failed with exit code {code}"),
        Some(_) => {
            let stdout = String::from_utf8(output.stdout)?;
            Ok(stdout)
        }
        None => bail!("Command `{cmd:?}` failed with no exit code"),
    }
}

/// Requests confirmation from the user, i.e. the user must type `y` to continue.
///
/// ### Params
/// msg:    the message that is displayed to the user to request access. On display this function
///         will append ` [y/N]` to the message.
/// dash_y: if this flag is true, requesting user input is skippend and `true` is immediately
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
