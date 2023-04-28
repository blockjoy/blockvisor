mod utils;
use babel_api::{engine::JobStatus, metadata::BlockchainMetadata, plugin::Plugin, rhai_plugin};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub fn rhai_smoke(path: &Path) -> anyhow::Result<BlockchainMetadata> {
    let script = fs::read_to_string(path)?;
    let mut babel = utils::MockBabelEngine::new();
    babel.expect_save_data().returning(|_| Ok(()));

    babel.expect_start_job().returning(|_, _| Ok(()));
    babel.expect_stop_job().returning(|_| Ok(()));
    babel
        .expect_job_status()
        .returning(|_| Ok(JobStatus::Running));
    babel
        .expect_run_jrpc()
        .returning(|_, _, _| Ok(Default::default()));
    babel
        .expect_run_rest()
        .returning(|_, _| Ok(Default::default()));
    babel
        .expect_run_sh()
        .returning(|_, _| Ok(Default::default()));
    babel
        .expect_sanitize_sh_param()
        .returning(|input| Ok(input.to_string()));
    babel.expect_render_template().returning(|_, _, _| Ok(()));
    babel.expect_node_params().returning(|| Default::default());
    babel.expect_save_data().returning(|_| Ok(()));
    babel
        .expect_load_data()
        .returning(|| Ok(Default::default()));
    let plugin = rhai_plugin::RhaiPlugin::new(&script, babel)?;
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
