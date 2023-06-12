mod utils;
use babel_api::engine::{HttpResponse, ShResponse};
use babel_api::plugin::{ApplicationStatus, StakingStatus, SyncStatus};
use babel_api::{engine::JobStatus, plugin::Plugin, rhai_plugin};
use mockall::*;
use std::collections::HashMap;
use std::fs;

#[test]
fn test_testing() -> anyhow::Result<()> {
    let mut babel = utils::MockBabelEngine::new();
    babel.expect_save_data().returning(|_| Ok(()));

    babel.expect_start_job().returning(|_, _| Ok(()));
    babel.expect_stop_job().returning(|_| Ok(()));
    babel
        .expect_job_status()
        .returning(|_| Ok(JobStatus::Running));
    babel
        .expect_run_jrpc()
        .with(
            predicate::eq("http://localhost:4467/"),
            predicate::always(),
            predicate::always(),
        )
        .returning(|_, _, _| {
            Ok(HttpResponse {
                status_code: 200,
                body: r#"
                    {"result": {
                        "height": 77,
                        "block_age": 18,
                        "name": "node name",
                        "peer_addr": "peer/address" 
                    }}
                "#
                .to_string(),
            })
        });
    babel.expect_run_rest().returning(|_, _| {
        Ok(HttpResponse {
            status_code: 200,
            body: Default::default(),
        })
    });
    babel.expect_run_sh().returning(|_, _| {
        Ok(ShResponse {
            exit_code: 0,
            stdout: Default::default(),
            stderr: Default::default(),
        })
    });
    babel
        .expect_sanitize_sh_param()
        .returning(|input| Ok(input.to_string()));
    babel.expect_render_template().returning(|_, _, _| Ok(()));
    babel.expect_node_params().returning(|| {
        HashMap::from_iter([
            ("NETWORK".to_string(), "main".to_string()),
            ("TESTING_PARAM".to_string(), "testing_value".to_string()),
        ])
    });
    babel.expect_save_data().returning(|_| Ok(()));
    babel
        .expect_load_data()
        .returning(|| Ok(Default::default()));

    let script = fs::read_to_string("protocols/testing/babel.rhai")?;
    let plugin = rhai_plugin::RhaiPlugin::new(&script, babel)?;

    assert!(plugin.has_capability("init"));
    plugin.init(&HashMap::from_iter([(
        "key1".to_string(),
        "key1_value".to_string(),
    )]))?;
    assert_eq!(77, plugin.height()?);
    assert_eq!(18, plugin.block_age()?);
    assert_eq!("node name", plugin.name()?);
    assert_eq!("peer/address", plugin.address()?);
    assert!(!plugin.consensus()?);
    assert_eq!(
        ApplicationStatus::Broadcasting,
        plugin.application_status()?
    );
    assert_eq!(SyncStatus::Synced, plugin.sync_status()?);
    assert_eq!(StakingStatus::Staking, plugin.staking_status()?);
    plugin.generate_keys()?;
    assert_eq!(1, plugin.metadata()?.requirements.vcpu_count);
    Ok(())
}
