use crate::src::utils::{
    stub_server::{StubCommandsServer, StubDiscoveryService},
    test_env::TestEnv,
    token::TokenGenerator,
};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::{
    config::{Config, SharedConfig},
    firecracker_machine::FC_BIN_NAME,
    nodes_manager::NodesManager,
    services,
    services::api::{self, common, pb},
    set_bv_status, utils, ServiceStatus,
};
use bv_utils::{cmd::run_cmd, run_flag::RunFlag, system::is_process_running};
use eyre::{bail, Result};
use std::{net::ToSocketAddrs, sync::Arc};
use tokio::{
    sync::Mutex,
    time::{sleep, Duration},
};
use tonic::transport::Server;
use uuid::Uuid;

#[test]
fn test_bv_cli_start_without_init() {
    let tmp_dir = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Error: Host is not registered, please run `bvup` first",
        ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_host_metrics() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    test_env.bv_run(&["host", "metrics"], "Used cpu:");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_delete_all() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    test_env.bv_run(&["node", "rm", "--all", "--yes"], "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_start_and_stop_all() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    const NODES_COUNT: usize = 2;
    println!("create {NODES_COUNT} nodes");
    let mut nodes: Vec<(String, String)> = Default::default();
    for i in 0..NODES_COUNT {
        nodes.push(test_env.create_node("testing/validator/0.0.1", &format!("216.18.214.{i}")));
    }

    println!("start all created nodes");
    test_env.bv_run(&["node", "start"], "Started node");
    println!("check all nodes are running");
    for (vm_id, _) in &nodes {
        test_env.bv_run(&["node", "status", vm_id], "Running");
    }
    println!("stop all nodes");
    test_env.bv_run(&["node", "stop"], "Stopped node");
    println!("check all nodes are stopped");
    for (vm_id, _) in &nodes {
        test_env.bv_run(&["node", "status", vm_id], "Stopped");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_jobs() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    println!("create a node");
    let (vm_id, _) = &test_env.create_node("testing/validator/0.0.1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("start node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("check jobs");
    test_env.bv_run(&["node", "job", vm_id, "ls"], "upload");

    println!("stop job");
    test_env.bv_run(&["node", "job", vm_id, "stop", "upload"], "");

    println!("job info");
    test_env.bv_run(&["node", "job", vm_id, "info", "upload"], "status: Stopped");

    println!("start job");
    test_env.bv_run(&["node", "job", vm_id, "start", "upload"], "");

    println!("wait for upload finished");
    let start = std::time::Instant::now();
    while let Err(err) = test_env.try_bv_run(
        &["node", "job", vm_id, "info", "upload"],
        "status: Finished {\n        exit_code: Some(\n            0,\n        ),\n        message: \"\",\n    }",
    ) {
        if start.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_secs(1));
        } else {
            panic!("timeout expired: {err:#}")
        }
    }

    let _ = test_env.sh_inside(vm_id, "touch /var/lib/babel/jobs/status/upload.parts");
    println!("cleanup job");
    test_env.bv_run(&["node", "job", vm_id, "cleanup", "upload"], "");
    assert!(test_env
        .sh_inside(
            vm_id,
            "if ! [[ -e /var/lib/babel/jobs/status/upload.parts ]]; then echo cleanup_done; fi;"
        )
        .contains("cleanup_done"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_logs() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    println!("create a node");
    let (vm_id, _) = &test_env.create_node("testing/validator/0.0.1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("start node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("wait for logs");
    let start = std::time::Instant::now();
    while let Err(err) = test_env.try_bv_run(
        &["node", "logs", vm_id],
        "Testing entry_point not configured, but parametrized with anything!",
    ) {
        if start.elapsed() < Duration::from_secs(15) {
            sleep(Duration::from_secs(1)).await;
        } else {
            panic!("timeout expired: {err:#}")
        }
    }

    println!("get babel logs");
    test_env.bv_run(
        &["node", "babel-logs", "-m", "256", vm_id],
        "Reading job config file: /var/lib/babel/jobs/config/echo.cfg",
    );

    println!("stop started node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_lifecycle() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    let mut run = RunFlag::default();
    let bv_handle = test_env.run_blockvisord(run.clone()).await?;
    println!("create a node");
    let (vm_id, vm_name) = &test_env.create_node("testing/validator/0.0.1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("stop stopped node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("stop started node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("restart stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("query metrics");
    test_env.bv_run(&["node", "metrics", vm_id], "In consensus:          false");

    println!("list running node before service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("list running node using vm name");
    test_env.bv_run(&["node", "status", vm_name], "Running");

    println!("stop service");
    run.stop();
    bv_handle.await.ok();

    println!("start service again");
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("list running node after service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("upgrade running node");
    test_env.bv_run(
        &["node", "upgrade", "testing/validator/0.0.2", vm_id],
        "Upgraded node",
    );

    println!("list running node after node upgrade");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("check jobs after node upgrade");
    test_env.bv_run(&["node", "job", vm_id, "info", "echo"], "status: Running");

    println!("delete started node");
    test_env.bv_run(&["node", "delete", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_recovery() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("create a node");
    let (vm_id, _) = &test_env.create_node("testing/validator/0.0.1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    let process_id = utils::get_process_pid(FC_BIN_NAME, vm_id).unwrap();
    println!("impolitely kill node with process id {process_id}");
    run_cmd("kill", ["-9", &process_id.to_string()])
        .await
        .unwrap();
    // wait until process is actually killed
    while is_process_running(process_id) {
        sleep(Duration::from_millis(10)).await;
    }

    println!("list running node before recovery");
    test_env.bv_run(&["node", "status", vm_id], "Failed");
    test_env
        .wait_for_running_node(vm_id, Duration::from_secs(60))
        .await;

    println!("stop babelsup - break node");
    // it may fail because it stop babalsup so ignore result
    let _ = test_env.try_bv_run(&["node", "run", "stop_babelsup", vm_id], "");

    println!("list running node before recovery");
    test_env.bv_run(&["node", "status", vm_id], "Failed");
    test_env
        .wait_for_running_node(vm_id, Duration::from_secs(60))
        .await;

    println!("delete started node");
    test_env.bv_run(&["node", "delete", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_recovery_fail() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("create a node");
    let (vm_id, _) = &test_env.create_node("testing/validator/0.0.1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("disable and stop babelsup - permanently break node");
    test_env.bv_run(&["node", "run", "disable_babelsup", vm_id], "");
    // it may fail because it stops babalsup so ignore result
    let _ = test_env.try_bv_run(&["node", "run", "stop_babelsup", vm_id], "");

    println!("list running node before recovery");
    test_env.bv_run(&["node", "status", vm_id], "Failed");
    test_env
        .wait_for_node_fail(vm_id, Duration::from_secs(600))
        .await;

    println!("delete started node");
    test_env.bv_run(&["node", "delete", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test]
async fn test_bv_nodes_via_pending_grpc_commands() -> Result<()> {
    let test_env = TestEnv::new().await?;
    let host_id = Uuid::new_v4().to_string();
    let node_name = "beautiful-node-name".to_string();
    let id = Uuid::new_v4().to_string();
    let failed_node_id = Uuid::new_v4().to_string();
    let command_id = Uuid::new_v4().to_string();
    let rules = vec![common::FirewallRule {
        name: "Rule X".to_string(),
        action: common::FirewallAction::Allow.into(),
        direction: common::FirewallDirection::Inbound.into(),
        protocol: Some(common::FirewallProtocol::Tcp.into()),
        ips: Some("192.167.0.1/24".to_string()),
        ports: vec![8080, 8000],
    }];
    let properties = vec![pb::Parameter {
        name: "TESTING_PARAM".to_string(),
        value: "anything".to_string(),
    }];
    let image = Some(common::ImageIdentifier {
        protocol: "testing".to_string(),
        node_type: common::NodeType::Validator.into(),
        node_version: "0.0.1".to_string(),
    });
    let image_v2 = Some(common::ImageIdentifier {
        protocol: "testing".to_string(),
        node_type: common::NodeType::Validator.into(),
        node_version: "0.0.2".to_string(),
    });

    println!("preparing server");

    let cmd = |cmd| pb::Command {
        id: command_id.clone(),
        exit_message: None,
        exit_code: None,
        created_at: None,
        acked_at: None,
        command: Some(cmd),
        retry_hint_seconds: None,
    };
    let mut commands = vec![
        vec![
            // create
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: node_name.clone(),
                    image: image.clone(),
                    blockchain: "testing".to_string(),
                    node_type: common::NodeType::Validator.into(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    rules: rules.clone(),
                    properties: properties.clone(),
                    network: "test".to_string(),
                })),
            })),
            // create with same node id
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: "some-new-name".to_string(),
                    image: image.clone(),
                    blockchain: "testing".to_string(),
                    node_type: common::NodeType::Validator.into(),
                    ip: "216.18.214.196".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    rules: rules.clone(),
                    properties: properties.clone(),
                    network: "test".to_string(),
                })),
            })),
            // create with same node name
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: failed_node_id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: node_name.clone(),
                    image: image.clone(),
                    blockchain: "testing".to_string(),
                    node_type: common::NodeType::Validator.into(),
                    ip: "216.18.214.197".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    rules: rules.clone(),
                    properties: properties.clone(),
                    network: "test".to_string(),
                })),
            })),
            // delete - this one should not be executed since previous one is expected to fail
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: failed_node_id,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Delete(pb::NodeDelete {})),
            })),
        ],
        vec![
            // create with same node ip address
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: Uuid::new_v4().to_string(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: "some-new-name".to_string(),
                    image: image.clone(),
                    blockchain: "testing".to_string(),
                    node_type: common::NodeType::Validator.into(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    rules: rules.clone(),
                    properties: properties.clone(),
                    network: "test".to_string(),
                })),
            })),
        ],
        vec![
            // create with invalid node ip address
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: Uuid::new_v4().to_string(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: "some-new-name".to_string(),
                    image: image.clone(),
                    blockchain: "testing".to_string(),
                    node_type: common::NodeType::Validator.into(),
                    ip: "invalid_ip".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    rules: rules.clone(),
                    properties: properties.clone(),
                    network: "test".to_string(),
                })),
            })),
        ],
        vec![
            // create with invalid gateway ip address
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: Uuid::new_v4().to_string(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: "some-new-name".to_string(),
                    image: image.clone(),
                    blockchain: "testing".to_string(),
                    node_type: common::NodeType::Validator.into(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "invalid_ip".to_string(),
                    rules: rules.clone(),
                    properties: properties.clone(),
                    network: "test".to_string(),
                })),
            })),
        ],
        vec![
            // stop stopped
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Stop(pb::NodeStop {})),
            })),
            // start
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Start(pb::NodeStart {})),
            })),
            // start running
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Start(pb::NodeStart {})),
            })),
            // stop
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Stop(pb::NodeStop {})),
            })),
            // restart stopped
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Restart(pb::NodeRestart {})),
            })),
            // restart running
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Restart(pb::NodeRestart {})),
            })),
            // upgrade running
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Upgrade(pb::NodeUpgrade {
                    image: image_v2,
                })),
            })),
        ],
        vec![
            // update with invalid rules
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Update(pb::NodeUpdate {
                    rules: vec![common::FirewallRule {
                        name: "Rule B".to_string(),
                        action: common::FirewallAction::Allow.into(),
                        direction: common::FirewallDirection::Inbound.into(),
                        protocol: Some(common::FirewallProtocol::Both.into()),
                        ips: Some("invalid_ip".to_string()),
                        ports: vec![8080],
                    }],
                })),
            })),
        ],
        vec![
            // update with too many rules
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Update(pb::NodeUpdate {
                    rules: rules.into_iter().cycle().take(129).collect(),
                })),
            })),
        ],
        vec![
            // update firewall rules
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Update(pb::NodeUpdate {
                    rules: vec![common::FirewallRule {
                        name: "Rule A".to_string(),
                        action: common::FirewallAction::Allow.into(),
                        direction: common::FirewallDirection::Inbound.into(),
                        protocol: Some(common::FirewallProtocol::Tcp.into()),
                        ips: Some("192.168.0.1/24".to_string()),
                        ports: vec![8080, 8000],
                    }],
                })),
            })),
            // delete
            cmd(pb::command::Command::Node(pb::NodeCommand {
                node_id: id.clone(),
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Delete(pb::NodeDelete {})),
            })),
        ],
    ];
    commands.reverse();

    let pending_commands = Arc::new(Mutex::new(commands));
    let commands_updates = Arc::new(Mutex::new(vec![]));
    let commands_server = StubCommandsServer {
        commands: pending_commands.clone(),
        updates: commands_updates.clone(),
    };

    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::command_service_server::CommandServiceServer::new(
                commands_server,
            ))
            .serve("0.0.0.0:8089".to_socket_addrs().unwrap().next().unwrap())
            .await
            .unwrap()
    };
    set_bv_status(ServiceStatus::Ok).await;
    let id = Uuid::new_v4();
    let config = Config {
        id: id.to_string(),
        token: TokenGenerator::create_host(id, "1245456"),
        refresh_token: "any refresh token".to_string(),
        blockjoy_api_url: "http://localhost:8089".to_string(),
        blockjoy_mqtt_url: Some("mqtt://localhost:1889".to_string()),
        update_check_interval_secs: None,
        blockvisor_port: 0,
        iface: "bvbr0".to_string(),
        cluster_id: None,
        cluster_seed_urls: None,
    };
    let config = SharedConfig::new(config.clone(), "/conf.jason".into());
    let config_clone = config.clone();

    let nodes_manager =
        Arc::new(NodesManager::load(test_env.build_dummy_platform(), config).await?);

    let client_future = async {
        while !pending_commands.lock().await.is_empty() {
            api::get_and_process_pending_commands(&config_clone, nodes_manager.clone()).await;
        }
    };

    println!("run server");
    tokio::select! {
        _ = server_future => {},
        _ = client_future => {},
        _ = sleep(Duration::from_secs(240)) => {},
    }

    println!("check received updates");
    println!("got commands updates: {:?}", commands_updates.lock().await);
    let expected_updates = [
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (
            &command_id,
            Some("BV internal error: Node with name `beautiful-node-name` exists"),
            Some(pb::CommandExitCode::InternalError.into()),
        ),
        (
            &command_id,
            Some("BV internal error: Node with ip address `216.18.214.195` exists"),
            Some(pb::CommandExitCode::InternalError.into()),
        ),
        (
            &command_id,
            Some("BV internal error: invalid ip `invalid_ip`: invalid IP address syntax"),
            Some(pb::CommandExitCode::InternalError.into()),
        ),
        (
            &command_id,
            Some("BV internal error: invalid gateway `invalid_ip`: invalid IP address syntax"),
            Some(pb::CommandExitCode::InternalError.into()),
        ),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (
            &command_id,
            Some("BV internal error: invalid ip address `invalid_ip` in firewall rule `Rule B`"),
            Some(pb::CommandExitCode::InternalError.into()),
        ),
        (
            &command_id,
            Some("BV internal error: Can't configure more than 128 rules!"),
            Some(pb::CommandExitCode::InternalError.into()),
        ),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
        (&command_id, None, Some(pb::CommandExitCode::Ok.into())),
    ];
    for (idx, expected) in expected_updates.iter().enumerate() {
        let actual = &commands_updates.lock().await[idx];
        assert_eq!(&actual.id, expected.0);
        let is_response_ok = match (actual.exit_message.as_deref(), expected.1) {
            (None, None) => true,
            (Some(a), Some(e)) => a.contains(e),
            _ => false,
        };
        assert!(is_response_ok);
        assert_eq!(actual.exit_code, expected.2);
    }
    Ok(())
}

#[tokio::test]
async fn test_discovery_on_connection_error() -> Result<()> {
    let discovery_service = StubDiscoveryService;
    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::discovery_service_server::DiscoveryServiceServer::new(
                discovery_service,
            ))
            .serve("0.0.0.0:8091".to_socket_addrs().unwrap().next().unwrap())
            .await
            .unwrap()
    };
    let id = Uuid::new_v4();
    let config = Config {
        id: id.to_string(),
        token: TokenGenerator::create_host(id, "1245456"),
        refresh_token: "any refresh token".to_string(),
        blockjoy_api_url: "http://localhost:8091".to_string(),
        blockjoy_mqtt_url: None,
        update_check_interval_secs: None,
        blockvisor_port: 0,
        iface: "bvbr0".to_string(),
        cluster_id: None,
        cluster_seed_urls: None,
    };
    let config = SharedConfig::new(config, "/some/dir/conf.json".into());
    let connect_future = services::connect_with_discovery(&config, |config| async {
        let config = config.read().await;
        if config.blockjoy_mqtt_url.is_none() {
            bail!("first try without urls")
        }
        Ok(config)
    });
    println!("run server");
    let final_cfg = tokio::select! {
        _ = server_future => {unreachable!()},
        res = connect_future => {res},
    }?;
    assert_eq!("notification_url", &final_cfg.blockjoy_mqtt_url.unwrap());
    Ok(())
}
