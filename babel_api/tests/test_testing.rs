mod utils;
use babel_api::{
    engine::{HttpResponse, JobStatus, JobType, ShResponse},
    plugin::{ApplicationStatus, Plugin, StakingStatus, SyncStatus},
    rhai_plugin,
};
use std::{collections::HashMap, fs};

#[test]
fn test_testing() -> anyhow::Result<()> {
    let mut babel = utils::MockBabelEngine::new();
    babel.expect_save_data().returning(|_| Ok(()));

    babel
        .expect_start_job()
        .withf(|name, config| {
            if let JobType::Upload {
                manifest: Some(_), ..
            } = &config.job_type
            {
                name == "upload"
            } else {
                false
            }
        })
        .returning(|_, _| Ok(()));
    babel.expect_job_status().returning(|_| {
        Ok(JobStatus::Finished {
            exit_code: Some(0),
            message: "".to_string(),
        })
    });
    babel
        .expect_start_job()
        .withf(|name, config| {
            if let JobType::Download {
                manifest: Some(_), ..
            } = &config.job_type
            {
                name == "download"
            } else {
                false
            }
        })
        .returning(|_, _| Ok(()));
    babel.expect_start_job().returning(|_, _| Ok(()));
    babel.expect_stop_job().returning(|_| Ok(()));
    babel
        .expect_job_status()
        .returning(|_| Ok(JobStatus::Running));
    babel
        .expect_run_jrpc()
        .withf(|req, _| req.host == "http://localhost:4467/")
        .returning(|_, _| {
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
    plugin.call_custom_method(
        "upload",
        r#"{
            "manifest_slot": {
                "key": "manifest_key",
                "url": "some://valid.url",
            },
            "slots": [
                {
                    "key": "part_key_1",
                    "url": "some://valid.url",
                },
            ]
        }"#,
    )?;
    assert_eq!(
        r#"#{"finished": #{"exit_code": 0, "message": ""}}"#,
        plugin.call_custom_method("upload_status", "",)?
    );
    plugin.call_custom_method(
        "custom_download",
        r#"{
            "total_size": 1024,
            "chunks": [
                {
                    "key": "part_key_1",
                    "url": "some://valid.url",
                    "checksum": {
                        "sha1": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 ]
                    },
                    "size": 1024,
                    "destinations": [
                        {
                            "path": "f1.data",
                            "pos": 0,
                            "size": 1024
                        }
                    ]
                },
            ]
        }"#,
    )?;

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
