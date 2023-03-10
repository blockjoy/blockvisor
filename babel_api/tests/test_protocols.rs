use anyhow::bail;
use babel_api::config::Babel;
use std::fs;
use std::path::Path;

pub fn load(path: &Path) -> anyhow::Result<Babel> {
    tracing::info!("Loading babel configuration at {}", path.display());
    let toml_str = fs::read_to_string(path)?;

    let cfg: Babel = toml::from_str(&toml_str)?;
    if cfg.supervisor.entry_point.is_empty() {
        bail!("no entry point defined");
    }
    if cfg.supervisor.log_buffer_capacity_ln == 0 || cfg.supervisor.log_buffer_capacity_ln > 4096 {
        bail!("invalid log_buffer_capacity_ln - must be in [1..4096]");
    }
    babel_api::check_babel_config(&cfg)?;
    tracing::debug!("Loaded babel configuration: {:?}", &cfg);
    Ok(cfg)
}

#[test]
fn test_load() {
    use walkdir::WalkDir;

    for entry in WalkDir::new("protocols") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() && path.extension().unwrap_or_default() == "toml" {
            println!("loading: {path:?}");
            load(path).unwrap();
        }
    }
}
