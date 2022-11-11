#[cfg(target_os = "linux")]
use assert_cmd::Command;
#[cfg(target_os = "linux")]
use assert_fs::TempDir;
#[cfg(target_os = "linux")]
use blockvisord::grpc::{self, pb};
#[cfg(target_os = "linux")]
use futures_util::FutureExt;
#[cfg(target_os = "linux")]
use predicates::prelude::*;
#[cfg(target_os = "linux")]
use serial_test::serial;
#[cfg(target_os = "linux")]
use std::{net::ToSocketAddrs, sync::Arc};
#[cfg(target_os = "linux")]
use tokio::sync::{mpsc, Mutex};
#[cfg(target_os = "linux")]
use tokio::time::{sleep, Duration};
#[cfg(target_os = "linux")]
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
#[cfg(target_os = "linux")]
use tonic::{
    transport::{Endpoint, Server},
    Request,
};

#[cfg(target_os = "linux")]
pub mod ui_pb {
    // https://github.com/tokio-rs/prost/issues/661
    #![allow(clippy::derive_partial_eq_without_eq)]
    tonic::include_proto!("blockjoy.api.ui_v1");
}

#[cfg(target_os = "linux")]
mod stub_server;

#[cfg(target_os = "linux")]
mod token;

#[cfg(target_os = "linux")]
fn bv_run(commands: &[&str], stdout_pattern: &str) {
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(commands)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains(stdout_pattern));
}

#[cfg(target_os = "linux")]
fn create_node(chain_id: &str) -> String {
    use std::str;

    let mut cmd = Command::cargo_bin("bv").unwrap();
    let cmd = cmd.args(&["node", "create", chain_id]);
    let output = cmd.output().unwrap();
    let stdout = str::from_utf8(&output.stdout).unwrap();
    let stderr = str::from_utf8(&output.stderr).unwrap();
    println!("create stdout: {stdout}");
    println!("create stderr: {stderr}");
    stdout
        .trim_start_matches(&format!(
            "Created new node from `{chain_id}` image with ID "
        ))
        .split('`')
        .nth(1)
        .unwrap()
        .to_string()
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_start_no_init() {
    let tmp_dir = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .env("HOME", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr("Error: Host is not registered, please run `init` first\n");
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_restart() {
    bv_run(&["stop"], "blockvisor service stopped successfully");
    bv_run(&["status"], "Service stopped");
    bv_run(&["start"], "blockvisor service started successfully");
    bv_run(&["status"], "Service running");
    bv_run(&["start"], "Service already running");
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_node_start_and_stop_all() {
    const NODES_COUNT: usize = 2;
    println!("create {NODES_COUNT} nodes");
    let mut nodes: Vec<String> = Default::default();
    for _ in 0..NODES_COUNT {
        nodes.push(create_node("debian.ext4"));
    }

    println!("start all created nodes");
    bv_run(&["node", "start"], "Started node");
    println!("check all nodes are running");
    for id in &nodes {
        bv_run(&["node", "status", id], "Running");
    }
    println!("stop all nodes");
    bv_run(&["node", "stop"], "Stopped node");
    println!("check all nodes are stopped");
    for id in &nodes {
        bv_run(&["node", "status", id], "Stopped");
    }
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_node_lifecycle() {
    println!("create a node");
    let vm_id = &create_node("debian.ext4");
    println!("create vm_id: {vm_id}");

    println!("stop stopped node");
    bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("start stopped node");
    bv_run(&["node", "start", vm_id], "Started node");

    println!("stop started node");
    bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("restart stopped node");
    bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node before service restart");
    bv_run(&["node", "status", vm_id], "Running");

    println!("stop service");
    bv_run(&["stop"], "blockvisor service stopped successfully");

    println!("start service");
    bv_run(&["start"], "blockvisor service started successfully");

    println!("list running node after service restart");
    bv_run(&["node", "status", vm_id], "Running");

    println!("delete started node");
    bv_run(&["node", "delete", vm_id], "Deleted node");
}

#[tokio::test]
#[serial]
#[cfg(target_os = "linux")]
async fn test_bv_cmd_node_recovery() {
    use blockvisord::{node::FC_BIN_NAME, utils};

    let chain_id = "test_bv_cmd_node_recovery".to_string();
    println!("create a node");
    let vm_id = &create_node("debian.ext4");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    bv_run(&["node", "status", vm_id], "Running");

    let process_id = utils::get_process_pid(FC_BIN_NAME, vm_id).unwrap();
    println!("impolitelly kill node with process id {process_id}");
    utils::run_cmd("kill", &["-9", &process_id.to_string()])
        .await
        .unwrap();

    println!("list running node before recovery");
    bv_run(&["node", "status", vm_id], "Failed");

    sleep(Duration::from_secs(10)).await;

    println!("list running node after recovery");
    bv_run(&["node", "status", vm_id], "Running");

    println!("delete started node");
    bv_run(&["node", "delete", vm_id], "Deleted node");
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_init_unknown_otp() {
    let tmp_dir = TempDir::new().unwrap();

    let otp = "NOT_FOUND";
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["init", otp])
        .args(&["--ifa", ifa])
        .args(&["--url", url])
        .env("HOME", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Record not found"));
}

#[tokio::test]
#[serial]
#[cfg(target_os = "linux")]
async fn test_bv_cmd_init_localhost() {
    use blockvisord::config::Config;
    use serde_json::json;
    use uuid::Uuid;

    let request_id = Uuid::new_v4().to_string();

    let url = "http://localhost:8080";
    let email = "user1@example.com";
    let password = "user1pass";

    let mut client = ui_pb::user_service_client::UserServiceClient::connect(url)
        .await
        .unwrap();

    println!("create user");
    let create_user = ui_pb::CreateUserRequest {
        meta: Some(ui_pb::RequestMeta {
            id: Some(request_id.clone()),
            token: None,
            fields: vec![],
            pagination: None,
        }),
        user: Some(ui_pb::User {
            id: None,
            email: Some(email.to_string()),
            first_name: Some("first".to_string()),
            last_name: Some("last".to_string()),
            created_at: None,
            updated_at: None,
        }),
        password: password.to_string(),
        password_confirmation: password.to_string(),
    };
    let user: ui_pb::CreateUserResponse = client.create(create_user).await.unwrap().into_inner();
    println!("user created: {user:?}");
    assert_eq!(user.meta.as_ref().unwrap().origin_request_id, request_id);
    let user_id = get_first_message(user.meta);

    let id = uuid::Uuid::parse_str(&user_id).unwrap();
    let auth_token = token::TokenGenerator::create_auth(id, "1245456".to_string());
    let refresh_token = token::TokenGenerator::create_refresh(id, "23942390".to_string());

    println!("get user organization id");
    let mut client = ui_pb::organization_service_client::OrganizationServiceClient::connect(url)
        .await
        .unwrap();

    let org_get = ui_pb::GetOrganizationsRequest {
        meta: Some(ui_pb::RequestMeta::default()),
    };
    let orgs: ui_pb::GetOrganizationsResponse = client
        .get(with_auth(org_get, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("user org: {orgs:?}");
    let org_id = orgs.organizations.first().unwrap().id.as_ref().unwrap();

    println!("create host provision");
    let mut client = ui_pb::host_provision_service_client::HostProvisionServiceClient::connect(url)
        .await
        .unwrap();

    let provision_create = ui_pb::CreateHostProvisionRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        host_provision: Some(ui_pb::HostProvision {
            id: None,
            org_id: org_id.clone(),
            host_id: None,
            created_at: None,
            claimed_at: None,
            install_cmd: Some("install cmd".to_string()),
            ip_gateway: "216.18.214.193".to_string(),
            ip_range_from: "216.18.214.195".to_string(),
            ip_range_to: "216.18.214.206".to_string(),
        }),
    };
    let provision: ui_pb::CreateHostProvisionResponse = client
        .create(with_auth(provision_create, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("host provision: {provision:?}");
    let otp = get_first_message(provision.meta);

    println!("bv init");
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";

    Command::cargo_bin("bv")
        .unwrap()
        .args(&["init", &otp])
        .args(&["--ifa", ifa])
        .args(&["--url", url])
        .assert()
        .success()
        .stdout(predicate::str::contains("Configuring blockvisor"));

    println!("read host id");
    let config_path = "/root/.config/blockvisor.toml";
    let config = std::fs::read_to_string(config_path).unwrap();
    let config: Config = toml::from_str(&config).unwrap();
    let host_id = config.id;
    println!("got host id: {host_id}");

    println!("restart blockvisor");
    bv_run(&["stop"], "blockvisor service stopped successfully");
    bv_run(&["start"], "blockvisor service started successfully");

    println!("get blockchain id");
    let mut client = ui_pb::blockchain_service_client::BlockchainServiceClient::connect(url)
        .await
        .unwrap();

    let list_blockchains = ui_pb::ListBlockchainsRequest {
        meta: Some(ui_pb::RequestMeta::default()),
    };
    let list: ui_pb::ListBlockchainsResponse = client
        .list(with_auth(list_blockchains, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    let blockchain = list.blockchains.first().unwrap();
    println!("got blockchain: {:?}", &blockchain);
    let blockchain_id = blockchain.id.as_ref().unwrap();

    let mut client = ui_pb::node_service_client::NodeServiceClient::connect(url)
        .await
        .unwrap();

    let node_create = ui_pb::CreateNodeRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        node: Some(ui_pb::Node {
            id: None,
            org_id: Some(org_id.clone()),
            host_id: Some(host_id.to_string()),
            blockchain_id: Some(blockchain_id.to_string()),
            name: None,
            groups: vec![],
            version: None,
            ip: None,
            r#type: Some(json!({"id": 3, "properties": []}).to_string()),
            address: None,
            wallet_address: None,
            block_height: None,
            node_data: None,
            created_at: None,
            updated_at: None,
            status: Some(ui_pb::node::NodeStatus::Provisioning.into()),
            sync_status: None,
            staking_status: None,
            ip_gateway: None,
        }),
    };
    let node: ui_pb::CreateNodeResponse = client
        .create(with_auth(node_create, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("created node: {node:?}");
    let node_id = get_first_message(node.meta);

    sleep(Duration::from_secs(30)).await;

    println!("list created node");
    bv_run(&["node", "status", &node_id], "Stopped");

    let mut client = ui_pb::command_service_client::CommandServiceClient::connect(url)
        .await
        .unwrap();

    let node_start = ui_pb::CommandRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        id: host_id.to_string(),
        params: vec![ui_pb::Parameter {
            name: "resource_id".to_string(),
            value: node_id.clone(),
        }],
    };
    let command: ui_pb::CommandResponse = client
        .start_node(with_auth(node_start, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("executed start node command: {command:?}");

    sleep(Duration::from_secs(30)).await;

    println!("get node status");
    bv_run(&["node", "status", &node_id], "Running");
}

#[cfg(target_os = "linux")]
fn get_first_message(meta: Option<ui_pb::ResponseMeta>) -> String {
    meta.unwrap().messages.first().unwrap().clone()
}

#[cfg(target_os = "linux")]
fn with_auth<T>(inner: T, auth_token: &str, refresh_token: &str) -> Request<T> {
    let mut request = Request::new(inner);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", auth_token.to_string())
            .parse()
            .unwrap(),
    );
    request.metadata_mut().insert(
        "cookie",
        format!("refresh={}", refresh_token.to_string())
            .parse()
            .unwrap(),
    );
    println!("{:?}", request.metadata());
    request
}

#[tokio::test]
#[serial]
#[cfg(target_os = "linux")]
async fn test_bv_cmd_grpc_commands() {
    use blockvisord::grpc::process_commands_stream;
    use blockvisord::nodes::Nodes;
    use serde_json::json;
    use std::str::FromStr;
    use stub_server::StubServer;
    use uuid::Uuid;

    let node_name = "beautiful-node-name".to_string();
    let node_id = Uuid::new_v4().to_string();
    let id = node_id.clone();
    let command_id = Uuid::new_v4().to_string();

    println!("delete existing node, if any");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "delete", &node_name]).assert();

    println!("preparing server");
    let commands = vec![
        // create
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: node_name.clone(),
                    image: Some(pb::ContainerImage {
                        url: "debian.ext4".to_string(),
                    }),
                    blockchain: "helium".to_string(),
                    r#type: json!({"id": 3, "properties": []}).to_string(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                })),
            })),
        },
        // create with same node id
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: "some-new-name".to_string(),
                    image: Some(pb::ContainerImage {
                        url: "debian.ext4".to_string(),
                    }),
                    blockchain: "helium".to_string(),
                    r#type: json!({"id": 3, "properties": []}).to_string(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                })),
            })),
        },
        // create with same node name
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: Uuid::new_v4().to_string(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: node_name.clone(),
                    image: Some(pb::ContainerImage {
                        url: "debian.ext4".to_string(),
                    }),
                    blockchain: "helium".to_string(),
                    r#type: json!({"id": 3, "properties": []}).to_string(),
                    ip: "216.18.214.195".to_string(),
                    gateway: "216.18.214.193".to_string(),
                })),
            })),
        },
        // stop stopped
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Stop(pb::NodeStop {})),
            })),
        },
        // start
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Start(pb::NodeStart {})),
            })),
        },
        // start running
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Start(pb::NodeStart {})),
            })),
        },
        // stop
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Stop(pb::NodeStop {})),
            })),
        },
        // restart stopped
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Restart(pb::NodeRestart {})),
            })),
        },
        // restart running
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Restart(pb::NodeRestart {})),
            })),
        },
        // delete
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
                command: Some(pb::node_command::Command::Delete(pb::NodeDelete {})),
            })),
        },
    ];

    let (updates_tx, updates_rx) = mpsc::channel(128);
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);

    let server = StubServer {
        commands: Arc::new(Mutex::new(commands)),
        updates_tx,
        shutdown_tx,
    };

    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::command_flow_server::CommandFlowServer::new(server))
            .serve_with_shutdown(
                "0.0.0.0:8081".to_socket_addrs().unwrap().next().unwrap(),
                shutdown_rx.recv().map(drop),
            )
            .await
            .unwrap()
    };

    let nodes = Nodes::load().await.unwrap();
    let updates_tx = nodes.get_updates_sender().await.unwrap().clone();
    let nodes = Arc::new(Mutex::new(nodes));

    let token = grpc::AuthToken("any token".to_string());
    let endpoint = Endpoint::from_str("http://localhost:8081").unwrap();
    let client_future = async {
        sleep(Duration::from_secs(5)).await;
        let channel = Endpoint::connect(&endpoint).await.unwrap();
        let mut client = grpc::Client::with_auth(channel, token);
        process_commands_stream(&mut client, nodes.clone(), updates_tx.clone())
            .await
            .unwrap();
    };

    println!("run server");
    tokio::select! {
        _ = server_future => {},
        _ = client_future => {},
        _ = sleep(Duration::from_secs(120)) => {},
    };

    println!("check received updates");
    let updates: Vec<_> = ReceiverStream::new(updates_rx)
        .timeout(Duration::from_secs(1))
        .take_while(Result::is_ok)
        .collect()
        .await;
    let updates_count = updates.len();

    println!("got updates: {updates:?}");
    let expected_updates = vec![
        node_update(&node_id, pb::node_info::ContainerStatus::Creating),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        success_command_update(&command_id),
        error_command_update(&command_id, format!("Node with id `{node_id}` exists")),
        error_command_update(&command_id, format!("Node with name `{node_name}` exists")),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopping),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        success_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        success_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        success_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopping),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        success_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopping),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        success_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopping),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        success_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Deleting),
        node_update(&node_id, pb::node_info::ContainerStatus::Deleted),
        success_command_update(&command_id),
    ];

    for (actual, expected) in updates.into_iter().zip(expected_updates) {
        assert_eq!(actual.unwrap(), expected);
    }

    assert_eq!(updates_count, 30);
}

#[cfg(target_os = "linux")]
fn node_update(node_id: &str, status: pb::node_info::ContainerStatus) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Node(pb::NodeInfo {
            id: node_id.to_string(),
            container_status: Some(status.into()),
            ..Default::default()
        })),
    }
}

#[cfg(target_os = "linux")]
fn error_command_update(command_id: &str, message: String) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Command(pb::CommandInfo {
            id: command_id.to_string(),
            response: Some(message),
            exit_code: Some(1),
        })),
    }
}

#[cfg(target_os = "linux")]
fn success_command_update(command_id: &str) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Command(pb::CommandInfo {
            id: command_id.to_string(),
            response: None,
            exit_code: Some(0),
        })),
    }
}

#[tokio::test]
#[serial]
#[cfg(target_os = "linux")]
async fn test_bv_cmd_grpc_stub_init_reset() {
    use std::path::Path;
    use stub_server::StubHostsServer;

    let server = StubHostsServer {};

    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::hosts_server::HostsServer::new(server))
            .serve("0.0.0.0:8082".to_socket_addrs().unwrap().next().unwrap())
            .await
            .unwrap()
    };

    tokio::spawn(server_future);
    sleep(Duration::from_secs(5)).await;

    let _ = tokio::task::spawn_blocking(move || {
        let tmp_dir = TempDir::new().unwrap();
        let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
        let url = "http://localhost:8082";
        let otp = "AWESOME";
        let config_path = format!("{}/.config/blockvisor.toml", tmp_dir.to_string_lossy());

        println!("bv init");
        Command::cargo_bin("bv")
            .unwrap()
            .args(&["init", otp])
            .args(&["--ifa", ifa])
            .args(&["--url", url])
            .env("HOME", tmp_dir.as_os_str())
            .assert()
            .success()
            .stdout(predicate::str::contains("Configuring blockvisor"));

        assert_eq!(Path::new(&config_path).exists(), true);

        println!("bv reset");
        Command::cargo_bin("bv")
            .unwrap()
            .args(&["reset", "--yes"])
            .env("HOME", tmp_dir.as_os_str())
            .assert()
            .success()
            .stdout(predicate::str::contains("Deleting host"));

        assert_eq!(Path::new(&config_path).exists(), false);
    })
    .await
    .unwrap();
}
