use crate::src::utils::stub_server::StubDiscoveryService;
use crate::src::utils::{
    stub_server::{StubCommandsServer, StubNodesServer},
    test_env::TestEnv,
};
use anyhow::{bail, Result};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::{
    config::{Config, SharedConfig},
    node::FC_BIN_NAME,
    nodes::Nodes,
    server::bv_pb,
    services,
    services::{api, api::pb},
    set_bv_status, utils,
};
use bv_utils::run_flag::RunFlag;
use pb::node_info::ContainerStatus;
use serde_json::json;
use std::{net::ToSocketAddrs, sync::Arc};
use sysinfo::{Pid, PidExt, ProcessExt, ProcessRefreshKind, System, SystemExt};
use tokio::{
    sync::{Mutex, RwLock},
    time::{sleep, Duration},
};
use tonic::transport::Server;
use uuid::Uuid;

pub mod ui_pb {
    tonic::include_proto!("blockjoy.api.ui_v1");
}

#[test]
fn test_bv_cli_start_without_init() {
    let tmp_dir = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr("Error: Host is not registered, please run `bvup` first\n");
}

#[tokio::test]
async fn test_bv_host_metrics() -> Result<()> {
    let test_env = TestEnv::new().await?;
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
    let mut nodes: Vec<String> = Default::default();
    for _ in 0..NODES_COUNT {
        nodes.push(test_env.create_node("testing/validator/0.0.1"));
    }

    println!("start all created nodes");
    test_env.bv_run(&["node", "start"], "Started node");
    println!("check all nodes are running");
    for id in &nodes {
        test_env.bv_run(&["node", "status", id], "Running");
    }
    println!("stop all nodes");
    test_env.bv_run(&["node", "stop"], "Stopped node");
    println!("check all nodes are stopped");
    for id in &nodes {
        test_env.bv_run(&["node", "status", id], "Stopped");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_logs() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    println!("create a node");
    let vm_id = &test_env.create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("start node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("get logs");
    test_env.bv_run(
        &["node", "logs", vm_id],
        "Testing entry_point not configured, but parametrized with anything!",
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
    let vm_id = &test_env.create_node("testing/validator/0.0.1");
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
    test_env.bv_run(&["node", "metrics", vm_id], "In consensus:        false");

    println!("list running node before service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("stop service");
    run.stop();
    bv_handle.await.ok();

    println!("start service again");
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("list running node after service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("upgrade running node");
    test_env.bv_run(
        &["node", "upgrade", vm_id, "testing/validator/0.0.2"],
        "Upgraded node",
    );

    println!("list running node after node upgrade");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("generate node keys");
    test_env.bv_run(&["node", "run", vm_id, "generate_keys"], "");

    println!("check node keys");
    test_env.bv_run(&["node", "keys", vm_id], "first");

    println!("delete started node");
    test_env.bv_run(&["node", "delete", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_recovery() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("create a node");
    let vm_id = &test_env.create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    let process_id = utils::get_process_pid(FC_BIN_NAME, vm_id).unwrap();
    println!("impolitely kill node with process id {process_id}");
    utils::run_cmd("kill", ["-9", &process_id.to_string()])
        .await
        .unwrap();
    // wait until process is actually killed
    let is_process_running = |pid| {
        let mut sys = System::new();
        sys.refresh_process_specifics(Pid::from_u32(pid), ProcessRefreshKind::new())
            .then(|| sys.process(Pid::from_u32(pid)).map(|proc| proc.status()))
            .flatten()
            .map_or(false, |status| status != sysinfo::ProcessStatus::Zombie)
    };
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
    let _ = test_env.try_bv_run(&["node", "run", vm_id, "stop_babelsup"], "");

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
    let vm_id = &test_env.create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("disable and stop babelsup - permanently break node");
    test_env.bv_run(&["node", "run", vm_id, "disable_babelsup"], "");
    // it may fail because it stop babalsup so ignore result
    let _ = test_env.try_bv_run(&["node", "run", vm_id, "stop_babelsup"], "");

    println!("list running node before recovery");
    test_env.bv_run(&["node", "status", vm_id], "Failed");
    test_env
        .wait_for_node_fail(vm_id, Duration::from_secs(300))
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
    let node_id = Uuid::new_v4().to_string();
    let id = node_id.clone();
    let command_id = Uuid::new_v4().to_string();

    println!("preparing server");
    let commands = vec![
        // create
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: node_name.clone(),
                    image: Some(pb::ContainerImage {
                        protocol: "testing".to_string(),
                        node_type: "validator".to_string(),
                        node_version: "0.0.1".to_string(),
                        status: 1, // Development
                    }),
                    blockchain: "testing".to_string(),
                    r#type: json!({"id": 3, "properties": []}).to_string(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    self_update: false,
                    properties: vec![pb::Parameter {
                        name: "TESTING_PARAM".to_string(),
                        value: "anything".to_string(),
                    }],
                })),
            })),
        },
        // create with same node id
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: "some-new-name".to_string(),
                    image: Some(pb::ContainerImage {
                        protocol: "testing".to_string(),
                        node_type: "validator".to_string(),
                        node_version: "0.0.1".to_string(),
                        status: 1, // Development
                    }),
                    blockchain: "testing".to_string(),
                    r#type: json!({"id": 3, "properties": []}).to_string(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    self_update: false,
                    properties: vec![pb::Parameter {
                        name: "TESTING_PARAM".to_string(),
                        value: "anything".to_string(),
                    }],
                })),
            })),
        },
        // create with same node name
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: Uuid::new_v4().to_string(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: node_name.clone(),
                    image: Some(pb::ContainerImage {
                        protocol: "testing".to_string(),
                        node_type: "validator".to_string(),
                        node_version: "0.0.1".to_string(),
                        status: 1, // Development
                    }),
                    blockchain: "testing".to_string(),
                    r#type: json!({"id": 3, "properties": []}).to_string(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                    self_update: false,
                    properties: vec![pb::Parameter {
                        name: "TESTING_PARAM".to_string(),
                        value: "anything".to_string(),
                    }],
                })),
            })),
        },
        // stop stopped
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Stop(pb::NodeStop {})),
            })),
        },
        // start
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Start(pb::NodeStart {})),
            })),
        },
        // start running
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Start(pb::NodeStart {})),
            })),
        },
        // stop
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Stop(pb::NodeStop {})),
            })),
        },
        // restart stopped
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Restart(pb::NodeRestart {})),
            })),
        },
        // restart running
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Restart(pb::NodeRestart {})),
            })),
        },
        // upgrade running
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Upgrade(pb::NodeUpgrade {
                    image: Some(pb::ContainerImage {
                        protocol: "testing".to_string(),
                        node_type: "validator".to_string(),
                        node_version: "0.0.2".to_string(),
                        status: 1, // Development
                    }),
                })),
            })),
        },
        // delete
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                node_id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                host_id: host_id.clone(),
                command: Some(pb::node_command::Command::Delete(pb::NodeDelete {})),
            })),
        },
    ];

    let commands_updates = Arc::new(Mutex::new(vec![]));
    let commands_server = StubCommandsServer {
        commands: Arc::new(Mutex::new(commands)),
        updates: commands_updates.clone(),
    };

    let nodes_updates = Arc::new(Mutex::new(vec![]));
    let nodes_server = StubNodesServer {
        updates: nodes_updates.clone(),
    };

    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::commands_server::CommandsServer::new(commands_server))
            .add_service(pb::nodes_server::NodesServer::new(nodes_server))
            .serve("0.0.0.0:8089".to_socket_addrs().unwrap().next().unwrap())
            .await
            .unwrap()
    };
    set_bv_status(bv_pb::ServiceStatus::Ok).await;
    let config = Config {
        id: Uuid::new_v4().to_string(),
        token: "any token".to_string(),
        blockjoy_api_url: "http://localhost:8089".to_string(),
        blockjoy_keys_url: Some("http://localhost:8089".to_string()),
        blockjoy_registry_url: Some("http://localhost:50059".to_string()),
        blockjoy_mqtt_url: Some("mqtt://localhost:1889".to_string()),
        update_check_interval_secs: None,
        blockvisor_port: 0,
    };

    let nodes = Arc::new(RwLock::new(
        Nodes::load(
            test_env.build_dummy_platform(),
            SharedConfig::new(config.clone()),
        )
        .await?,
    ));

    let client_future = async {
        match api::CommandsService::connect(config).await {
            Ok(mut client) => {
                if let Err(e) = client
                    .get_and_process_pending_commands(&host_id, nodes.clone())
                    .await
                {
                    println!("Error processing pending commands: {:?}", e);
                }
            }
            Err(e) => println!("Error connecting to api: {:?}", e),
        }
    };

    println!("run server");
    tokio::select! {
        _ = server_future => {},
        _ = client_future => {},
        _ = sleep(Duration::from_secs(240)) => {},
    }

    println!("check received updates");
    println!("got nodes updates: {:?}", nodes_updates.lock().await);
    let expected_updates = vec![
        ContainerStatus::Creating,
        ContainerStatus::Stopped,
        ContainerStatus::Starting,
        ContainerStatus::Running,
        ContainerStatus::Stopping,
        ContainerStatus::Stopped,
        ContainerStatus::Starting,
        ContainerStatus::Running,
        ContainerStatus::Stopping,
        ContainerStatus::Stopped,
        ContainerStatus::Starting,
        ContainerStatus::Running,
        ContainerStatus::Upgrading,
        ContainerStatus::Stopping,
        ContainerStatus::Stopped,
        ContainerStatus::Starting,
        ContainerStatus::Running,
        ContainerStatus::Upgraded,
    ];
    for (actual, ref expected) in nodes_updates.lock().await.iter().zip(expected_updates) {
        assert_eq!(actual, expected);
    }

    println!("got commands updates: {:?}", commands_updates.lock().await);
    let expected_updates = vec![
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: Some("Node with name `beautiful-node-name` exists".to_string()),
            exit_code: Some(1),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
        pb::CommandInfo {
            id: command_id.clone(),
            response: None,
            exit_code: Some(0),
        },
    ];
    for (actual, ref expected) in commands_updates.lock().await.iter().zip(expected_updates) {
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[tokio::test]
async fn test_discovery_on_connection_error() -> Result<()> {
    let discovery_service = StubDiscoveryService;
    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::discovery_server::DiscoveryServer::new(
                discovery_service,
            ))
            .serve("0.0.0.0:8091".to_socket_addrs().unwrap().next().unwrap())
            .await
            .unwrap()
    };
    let config = Config {
        id: Uuid::new_v4().to_string(),
        token: "any token".to_string(),
        blockjoy_api_url: "http://localhost:8091".to_string(),
        blockjoy_keys_url: None,
        blockjoy_registry_url: None,
        blockjoy_mqtt_url: None,
        update_check_interval_secs: None,
        blockvisor_port: 0,
    };
    let connect_future = services::connect(SharedConfig::new(config.clone()), |config| async {
        if config.blockjoy_keys_url.is_none()
            && config.blockjoy_registry_url.is_none()
            && config.blockjoy_mqtt_url.is_none()
        {
            bail!("first try without urls")
        }
        Ok(config)
    });
    println!("run server");
    let final_cfg = tokio::select! {
        _ = server_future => {unreachable!()},
        res = connect_future => {res},
    }?;
    assert_eq!("key_service_url", &final_cfg.blockjoy_keys_url.unwrap());
    assert_eq!("registry_url", &final_cfg.blockjoy_registry_url.unwrap());
    assert_eq!("notification_url", &final_cfg.blockjoy_mqtt_url.unwrap());
    Ok(())
}
