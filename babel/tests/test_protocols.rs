use babel_api::config::Babel;
use eyre::bail;
use std::path::Path;
use tokio::fs;

pub async fn load(path: &Path) -> eyre::Result<Babel> {
    tracing::info!("Loading babel configuration at {}", path.display());
    let toml_str = fs::read_to_string(path).await?;

    let cfg: Babel = toml::from_str(&toml_str)?;
    if cfg.supervisor.entry_point.is_empty() {
        bail!("no entry point defined");
    }
    if cfg.supervisor.log_buffer_capacity_ln == 0 || cfg.supervisor.log_buffer_capacity_ln > 4096 {
        bail!("invalid log_buffer_capacity_ln - must be in [1..4096]");
    }
    tracing::debug!("Loaded babel configuration: {:?}", &cfg);
    Ok(cfg)
}

#[tokio::test]
async fn test_load() {
    use walkdir::WalkDir;

    for entry in WalkDir::new("protocols") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() && path.extension().unwrap_or_default() == "toml" {
            println!("loading: {path:?}");
            load(path).await.unwrap();
        }
    }
}
