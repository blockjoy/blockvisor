use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use tokio::process::Command;
use tracing::info;

/// Runs the specified command and returns error on failure.
/// **IMPORTANT**: Whenever you use new CLI tool in BV,
/// remember to add it to requirements check in `installer::check_cli_dependencies()`.   
pub async fn run_cmd<I, S>(cmd: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(cmd);
    cmd.args(args);
    info!("Running command: `{:?}`", cmd);
    match cmd
        .status()
        .await
        .with_context(|| format!("Failed to run command `{cmd:?}`"))?
        .code()
    {
        Some(code) if code != 0 => bail!("Command `{cmd:?}` failed with exit code {code}"),
        Some(_) => Ok(()),
        None => bail!("Command `{cmd:?}` failed with no exit code"),
    }
}
