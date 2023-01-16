use std::env;
use std::path::PathBuf;

pub const ENV_BABEL_ROOT_KEY: &str = "BABEL_ROOT";

lazy_static::lazy_static! {
    pub static ref ROOT_DIR: PathBuf = PathBuf::from(env::var(ENV_BABEL_ROOT_KEY).unwrap_or_else(|_| "/".to_string()));
    pub static ref BABEL_CONFIG_PATH: PathBuf = ROOT_DIR.join("etc").join("babel.conf");
    pub static ref BABELSUP_CONFIG_PATH: PathBuf = ROOT_DIR.join("etc").join("babelsup.conf");
    pub static ref BABEL_BIN_PATH: PathBuf = ROOT_DIR.join("usr").join("bin").join("babel");
    pub static ref PROC_CMDLINE_PATH: PathBuf = ROOT_DIR.join("proc").join("cmdline");
}
