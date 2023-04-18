use anyhow::bail;
use babel_api::{
    config::Babel,
    engine::{Engine, JobConfig, JobStatus},
    metadata::BlockchainMetadata,
    plugin::Plugin,
    rhai_plugin,
};
use mockall::*;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

mock! {
    pub BabelEngine {}

    impl Engine for BabelEngine {
        fn start_job(&self, job_name: &str, job_config: JobConfig) -> anyhow::Result<()>;
        fn stop_job(&self, job_name: &str) -> anyhow::Result<()>;
        fn job_status(&self, job_name: &str) -> anyhow::Result<JobStatus>;
        fn run_jrpc(&self, host: &str, method: &str) -> anyhow::Result<String>;
        fn run_rest(&self, url: &str) -> anyhow::Result<String>;
        fn run_sh(&self, body: &str) -> anyhow::Result<String>;
        fn sanitize_sh_param(&self, param: &str) -> anyhow::Result<String>;
        fn render_template(
            &self,
            template: &Path,
            output: &Path,
            params: HashMap<String, String>,
        ) -> anyhow::Result<()>;
        fn node_params(&self) -> HashMap<String, String>;
        fn save_data(&self, value: &str) -> anyhow::Result<()>;
        fn load_data(&self) -> anyhow::Result<String>;
    }
}

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

pub fn rhai_smoke(path: &Path) -> anyhow::Result<BlockchainMetadata> {
    let script = fs::read_to_string(path)?;
    let mut babel = MockBabelEngine::new();
    babel.expect_save_data().returning(|_| Ok(()));

    babel.expect_start_job().returning(|_, _| Ok(()));
    babel.expect_stop_job().returning(|_| Ok(()));
    babel
        .expect_job_status()
        .returning(|_| Ok(JobStatus::Running));
    babel
        .expect_run_jrpc()
        .returning(|_, _| Ok(Default::default()));
    babel
        .expect_run_rest()
        .returning(|_| Ok(Default::default()));
    babel.expect_run_sh().returning(|_| Ok(Default::default()));
    babel
        .expect_sanitize_sh_param()
        .returning(|input| Ok(input.to_string()));
    babel.expect_render_template().returning(|_, _, _| Ok(()));
    babel.expect_node_params().returning(|| Default::default());
    babel.expect_save_data().returning(|_| Ok(()));
    babel
        .expect_load_data()
        .returning(|| Ok(Default::default()));
    let plugin = rhai_plugin::new(&script, babel)?;
    assert!(plugin.has_capability("init"));
    plugin
        .init(&HashMap::from_iter([(
            "key1".to_string(),
            "key1_value".to_string(),
        )]))
        .ok();
    plugin.height().ok();
    plugin.block_age().ok();
    plugin.name().ok();
    plugin.address().ok();
    plugin.consensus().ok();
    plugin.application_status().ok();
    plugin.sync_status().ok();
    plugin.staking_status().ok();
    plugin.generate_keys().ok();
    Ok(plugin.metadata()?)
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

#[test]
fn test_smoke_all_rhai() {
    use walkdir::WalkDir;

    for entry in WalkDir::new("protocols") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() && path.extension().unwrap_or_default() == "rhai" {
            println!("smoke test for: {path:?}");
            rhai_smoke(path).unwrap();
        }
    }
}
