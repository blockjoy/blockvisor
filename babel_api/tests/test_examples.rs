use babel_api::{
    self,
    engine::{HttpResponse, JobConfig, JobInfo, JobStatus, RestartConfig, ShResponse},
    plugin::{ApplicationStatus, Plugin},
    rhai_plugin,
};
use bv_tests_utils::babel_engine_mock::MockBabelEngine;
use eyre::Context;
use mockall::*;
use std::{collections::HashMap, fs, path::Path};

pub fn rhai_smoke(path: &Path) -> eyre::Result<()> {
    let script = fs::read_to_string(path)?;
    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(64, 32);
    engine
        .compile(script)
        .with_context(|| "Rhai syntax error")?;
    Ok(())
}

fn dummy_babel_engine() -> MockBabelEngine {
    let mut babel = MockBabelEngine::new();
    babel.expect_save_data().returning(|_| Ok(()));

    babel.expect_create_job().returning(|_, _| Ok(()));
    babel.expect_start_job().returning(|_| Ok(()));
    babel.expect_job_info().returning(|_| {
        Ok(JobInfo {
            status: JobStatus::Finished {
                exit_code: Some(0),
                message: "".to_string(),
            },
            progress: None,
            restart_count: 0,
            logs: vec![],
            upgrade_blocking: false,
        })
    });
    babel.expect_create_job().returning(|_, _| Ok(()));
    babel.expect_start_job().returning(|_| Ok(()));
    babel.expect_create_job().returning(|_, _| Ok(()));
    babel.expect_start_job().returning(|_| Ok(()));
    babel.expect_stop_job().returning(|_| Ok(()));
    babel.expect_job_info().returning(|_| {
        Ok(JobInfo {
            status: JobStatus::Running,
            progress: None,
            restart_count: 0,
            logs: vec![],
            upgrade_blocking: false,
        })
    });
    babel.expect_get_jobs().returning(|| Ok(HashMap::default()));
    babel.expect_run_jrpc().returning(|_, _| {
        Ok(HttpResponse {
            status_code: 200,
            body: Default::default(),
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
    babel.expect_log().returning(|_, _| ());
    babel
}

#[test]
fn test_rhai_syntax() {
    use walkdir::WalkDir;

    for entry in WalkDir::new("examples") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() && path.extension().unwrap_or_default() == "rhai" {
            rhai_smoke(path)
                .with_context(|| path.display().to_string())
                .unwrap();
        }
    }
}

#[test]
fn test_custom_download_upload() {
    let dummy_babel = dummy_babel_engine();
    let script = fs::read_to_string("examples/custom_download_upload.rhai").unwrap();
    let plugin = rhai_plugin::RhaiPlugin::new(&script, dummy_babel).unwrap();
    plugin.init().unwrap();
    plugin.upload().unwrap();
}

#[test]
fn test_init_minimal() {
    let dummy_babel = dummy_babel_engine();
    let script = fs::read_to_string("examples/init_minimal.rhai").unwrap();
    let plugin = rhai_plugin::RhaiPlugin::new(&script, dummy_babel).unwrap();
    plugin.init().unwrap();
}

#[test]
fn test_jobs() {
    let dummy_babel = dummy_babel_engine();
    let script = fs::read_to_string("examples/jobs.rhai").unwrap();
    let plugin = rhai_plugin::RhaiPlugin::new(&script, dummy_babel).unwrap();
    plugin.init().unwrap();
}

#[test]
fn test_polygon_functions() {
    let mut dummy_babel = MockBabelEngine::new();
    dummy_babel.expect_run_sh().returning(|_, _| {
        Ok(ShResponse {
            exit_code: 0,
            stdout: "addr".to_string(),
            stderr: Default::default(),
        })
    });
    dummy_babel
        .expect_get_jobs()
        .returning(|| Ok(HashMap::default()));
    dummy_babel.expect_run_jrpc().once().returning(|_, _| {
        Ok(HttpResponse {
            status_code: 200,
            body: Default::default(),
        })
    });
    dummy_babel.expect_run_jrpc().once().returning(|_, _| {
        Ok(HttpResponse {
            status_code: 200,
            body: r#"{"result":"0xa1"}"#.to_string(),
        })
    });
    dummy_babel.expect_run_jrpc().once().returning(|_, _| {
        Ok(HttpResponse {
            status_code: 200,
            body: "{}".to_string(),
        })
    });

    let script = fs::read_to_string("examples/polygon_functions.rhai").unwrap();
    let plugin = rhai_plugin::RhaiPlugin::new(&script, dummy_babel).unwrap();
    assert_eq!("addr", plugin.address().unwrap());
    assert_eq!(
        ApplicationStatus::Broadcasting,
        plugin.application_status().unwrap()
    );
    assert_eq!(161, plugin.height().unwrap());
    assert_eq!(0, plugin.block_age().unwrap());
}

#[test]
fn test_plugin_config() -> eyre::Result<()> {
    let mut babel = bv_tests_utils::babel_engine_mock::MockBabelEngine::new();

    babel.expect_node_params().returning(|| {
        HashMap::from_iter([
            ("NETWORK".to_string(), "main".to_string()),
            ("TESTING_PARAM".to_string(), "testing_value".to_string()),
        ])
    });
    babel
        .expect_run_sh()
        .with(
            predicate::eq("mkdir -p /opt/netdata/var/cache/netdata"),
            predicate::eq(None),
        )
        .times(2)
        .returning(|_, _| {
            Ok(ShResponse {
                exit_code: 0,
                stdout: Default::default(),
                stderr: Default::default(),
            })
        });
    babel
        .expect_run_sh()
        .with(predicate::eq("echo downloaded"), predicate::eq(None))
        .times(2)
        .returning(|_, _| {
            Ok(ShResponse {
                exit_code: 0,
                stdout: Default::default(),
                stderr: Default::default(),
            })
        });
    babel
        .expect_run_sh()
        .with(predicate::eq("echo uploading"), predicate::eq(None))
        .once()
        .returning(|_, _| {
            Ok(ShResponse {
                exit_code: 0,
                stdout: Default::default(),
                stderr: Default::default(),
            })
        });
    babel
        .expect_run_sh()
        .with(predicate::eq("echo uploaded"), predicate::eq(None))
        .once()
        .returning(|_, _| {
            Ok(ShResponse {
                exit_code: 0,
                stdout: Default::default(),
                stderr: Default::default(),
            })
        });

    babel
        .expect_create_job()
        .with(
            predicate::eq("init_job"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh(
                    "openssl rand -hex 32 > /blockjoy/blockchain_data/A/jwt.txt".to_string(),
                ),
                restart: babel_api::engine::RestartPolicy::Never,
                shutdown_timeout_secs: Some(120),
                shutdown_signal: Some(babel_api::engine::PosixSignal::SIGINT),
                needs: Some(vec!["other_init_job_name".to_string()]),
                run_as: Some("some_user".to_string()),
            }),
        )
        .times(2)
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("init_job"))
        .times(2)
        .returning(|_| Ok(()));
    babel
        .expect_is_download_completed()
        .times(2)
        .returning(|| Ok(false));
    babel
        .expect_has_blockchain_archive()
        .once()
        .returning(|| Ok(true));
    babel
        .expect_has_blockchain_archive()
        .once()
        .returning(|| Ok(false));

    babel
        .expect_create_job()
        .with(
            predicate::eq("download"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::Download {
                    destination: None,
                    max_connections: Some(5),
                    max_runners: Some(8),
                },
                restart: babel_api::engine::RestartPolicy::OnFailure(RestartConfig {
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 1000,
                    max_retries: Some(5),
                }),
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec!["init_job".to_string()]),
                run_as: None,
            }),
        )
        .once()
        .returning(|_, _| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("download"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh(
                    r#"/usr/bin/wget -q -O - some_url"#.to_string(),
                ),
                restart: babel_api::engine::RestartPolicy::OnFailure(RestartConfig {
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 10000,
                    max_retries: Some(3),
                }),
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec!["init_job".to_string()]),
                run_as: Some("some_user".to_string()),
            }),
        )
        .once()
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("download"))
        .times(2)
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("post_download_job"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh("echo restoreDB".to_string()),
                restart: babel_api::engine::RestartPolicy::Never,
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec!["download".to_string()]),
                run_as: None,
            }),
        )
        .times(2)
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("post_download_job"))
        .times(2)
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("blockchain_service_a"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh(
                    r#"/usr/bin/blockchain_service_a start --home=/blockjoy/blockchain_data/A --chain=main --rest-server --seeds main seed "$@""#.to_string(),
                ),
                restart: babel_api::engine::RestartPolicy::Always(RestartConfig{
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 1000,
                    max_retries: Some(13),
                }),
                shutdown_timeout_secs: Some(120),
                shutdown_signal: Some(babel_api::engine::PosixSignal::SIGINT),
                needs: Some(vec!["post_download_job".to_string()]),
                run_as: Some("some_user".to_string()),
            }),
        )
        .times(2)
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("blockchain_service_a"))
        .times(2)
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("blockchain_service_b"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh(
                    r#"/usr/bin/blockchain_service_b --chain=main --datadir=/blockjoy/blockchain_data/A --snapshots=false"#.to_string(),
                ),
                restart: babel_api::engine::RestartPolicy::Always(RestartConfig{
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 1000,
                    max_retries: None,
                }),
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec!["post_download_job".to_string()]),
                run_as: None,
            }),
        )
        .times(2)
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("blockchain_service_b"))
        .times(2)
        .returning(|_| Ok(()));
    babel
        .expect_add_task()
        .with(
            predicate::eq("some_task"),
            predicate::eq("* * * * * * *"),
            predicate::eq("fn_name"),
            predicate::eq("param_value"),
        )
        .times(2)
        .returning(|_, _, _, _| Ok(()));

    babel
        .expect_stop_job()
        .with(predicate::eq("blockchain_service_a"))
        .once()
        .returning(|_| Ok(()));
    babel
        .expect_stop_job()
        .with(predicate::eq("blockchain_service_b"))
        .once()
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("pre_upload_job"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh("echo dumpDB".to_string()),
                restart: babel_api::engine::RestartPolicy::Never,
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec![]),
                run_as: None,
            }),
        )
        .once()
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("pre_upload_job"))
        .once()
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("upload"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::Upload {
                    source: None,
                    exclude: Some(vec![
                        "**/something_to_ignore*".to_string(),
                        ".gitignore".to_string(),
                        "some_subdir/*.bak".to_string(),
                    ]),
                    compression: Some(babel_api::engine::Compression::ZSTD(5)),
                    max_connections: Some(4),
                    max_runners: Some(12),
                    number_of_chunks: Some(700),
                    url_expires_secs: Some(240000),
                    data_version: Some(3),
                },
                restart: babel_api::engine::RestartPolicy::OnFailure(RestartConfig {
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 1000,
                    max_retries: Some(5),
                }),
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec!["pre_upload_job".to_string()]),
                run_as: None,
            }),
        )
        .once()
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("upload"))
        .once()
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("post_upload_job"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh(
                    "echo cleanup_after_upload".to_string(),
                ),
                restart: babel_api::engine::RestartPolicy::Never,
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec!["upload".to_string()]),
                run_as: None,
            }),
        )
        .once()
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("post_upload_job"))
        .once()
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("blockchain_service_a"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh(
                    r#"/usr/bin/blockchain_service_a start --home=/blockjoy/blockchain_data/A --chain=main --rest-server --seeds main seed "$@""#.to_string(),
                ),
                restart: babel_api::engine::RestartPolicy::Always(RestartConfig{
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 1000,
                    max_retries: Some(13),
                }),
                shutdown_timeout_secs: Some(120),
                shutdown_signal: Some(babel_api::engine::PosixSignal::SIGINT),
                needs: Some(vec!["post_upload_job".to_string()]),
                run_as: Some("some_user".to_string()),
            }),
        )
        .once()
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("blockchain_service_a"))
        .once()
        .returning(|_| Ok(()));

    babel
        .expect_create_job()
        .with(
            predicate::eq("blockchain_service_b"),
            predicate::eq(JobConfig {
                job_type: babel_api::engine::JobType::RunSh(
                    r#"/usr/bin/blockchain_service_b --chain=main --datadir=/blockjoy/blockchain_data/A --snapshots=false"#.to_string(),
                ),
                restart: babel_api::engine::RestartPolicy::Always(RestartConfig{
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 1000,
                    max_retries: None,
                }),
                shutdown_timeout_secs: None,
                shutdown_signal: None,
                needs: Some(vec!["post_upload_job".to_string()]),
                run_as: None,
            }),
        )
        .once()
        .returning(|_, _| Ok(()));
    babel
        .expect_start_job()
        .with(predicate::eq("blockchain_service_b"))
        .once()
        .returning(|_| Ok(()));

    babel
        .expect_get_jobs()
        .once()
        .returning(|| Ok(Default::default()));
    babel
        .expect_run_jrpc()
        .with(
            predicate::eq(babel_api::engine::JrpcRequest {
                host: "http://localhost:4467/".to_string(),
                method: "health.health".to_string(),
                params: None,
                headers: Some(vec![(
                    "content-type".to_string(),
                    "application/json".to_string(),
                )]),
            }),
            predicate::eq(None),
        )
        .once()
        .returning(|_, _| {
            Ok(HttpResponse {
                status_code: 200,
                body: r#"{"healthy": true}"#.to_string(),
            })
        });

    let script = fs::read_to_string("examples/plugin_config.rhai")?;
    let plugin = rhai_plugin::RhaiPlugin::new(&script, babel)?;

    plugin.init().unwrap();
    plugin.init().unwrap();
    plugin.upload().unwrap();
    assert_eq!(
        ApplicationStatus::Broadcasting,
        plugin.application_status().unwrap()
    );
    Ok(())
}
