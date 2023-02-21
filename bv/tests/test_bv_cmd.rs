mod utils;

use crate::utils::stub_server::{StubCommandsServer, StubHostsServer, StubNodesServer};
use crate::utils::token;
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::config::default_blockvisor_port;
use blockvisord::linux_platform::LinuxPlatform;
use blockvisord::services::api::{self, pb};
use blockvisord::set_bv_status;
use predicates::prelude::*;
use serial_test::serial;
use std::{env, fs};
use std::{net::ToSocketAddrs, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};
use tonic::{transport::Server, Request};

pub mod ui_pb {
    tonic::include_proto!("blockjoy.api.ui_v1");
}

fn bv_run(commands: &[&str], stdout_pattern: &str) {
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(commands)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains(stdout_pattern));
}

fn create_node(image: &str) -> String {
    use std::str;

    let mut cmd = Command::cargo_bin("bv").unwrap();
    let cmd = cmd.args([
        "node",
        "create",
        image,
        "--props",
        r#"{"TESTING_PARAM":"anything"}"#,
        "--gateway",
        "216.18.214.193",
        "--ip",
        "216.18.214.195",
    ]);
    let output = cmd.output().unwrap();
    let stdout = str::from_utf8(&output.stdout).unwrap();
    let stderr = str::from_utf8(&output.stderr).unwrap();
    println!("create stdout: {stdout}");
    println!("create stderr: {stderr}");
    stdout
        .trim_start_matches(&format!("Created new node from `{image}` image with ID "))
        .split('`')
        .nth(1)
        .unwrap()
        .to_string()
}

//// =========== E2E: bv cli and blockvisord service - host only
#[test]
fn test_bv_cli_service_restart() {
    bv_run(&["stop"], "blockvisor service stopped successfully");
    bv_run(&["status"], "Service stopped");
    bv_run(&["start"], "blockvisor service started successfully");
    bv_run(&["status"], "Service running");
    bv_run(&["start"], "Service already running");
}

//// =========== E2E: bvup host + backend (otp)
#[test]
#[serial]
fn test_bv_cmd_init_unknown_otp() {
    let tmp_dir = TempDir::new().unwrap();

    let otp = "NOT_FOUND";
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";
    let mqtt = "mqtt://localhost:1883";

    let mut cmd = Command::cargo_bin("bvup").unwrap();
    cmd.args([otp, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--keys", url])
        .args(["--registry", url])
        .args(["--mqtt", mqtt])
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Host provision not found: no rows returned by a query that expected to return at least one row"));
}

//// =========== E2E: bvup + cli host + ui api + backend (otp)
#[tokio::test]
#[serial]
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
    let id = Uuid::parse_str(&user_id).unwrap();

    println!("confirm user");
    let mut client =
        ui_pb::authentication_service_client::AuthenticationServiceClient::connect(url)
            .await
            .unwrap();
    let confirm_user = ui_pb::ConfirmRegistrationRequest {
        meta: Some(ui_pb::RequestMeta::default()),
    };
    let refresh_token = token::TokenGenerator::create_refresh(id, "23942390".to_string());
    let register_token =
        token::TokenGenerator::create_register(id, "23ß357320".to_string(), email.to_string());
    client
        .confirm(with_auth(confirm_user, &register_token, &refresh_token))
        .await
        .unwrap();

    println!("login user");
    let login_user = ui_pb::LoginUserRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        email: email.to_string(),
        password: password.to_string(),
    };
    let login: ui_pb::LoginUserResponse = client.login(login_user).await.unwrap().into_inner();
    println!("user login: {login:?}");
    let login_token = login.token.unwrap().value;
    let login_auth: token::AuthClaim =
        token::TokenGenerator::from_encoded("1245456".to_string(), &login_token).unwrap();
    let login_data = login_auth.data.unwrap();
    let org_id = login_data.get("org_id").unwrap();

    let auth_token = token::TokenGenerator::create_auth(
        id,
        "1245456".to_string(),
        org_id.clone(),
        email.to_string(),
    );

    println!("create host provision");
    let mut client = ui_pb::host_provision_service_client::HostProvisionServiceClient::connect(url)
        .await
        .unwrap();

    let provision_create = ui_pb::CreateHostProvisionRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        host_provision: Some(ui_pb::HostProvision {
            id: None,
            host_id: None,
            created_at: None,
            claimed_at: None,
            install_cmd: Some("install cmd".to_string()),
            ip_gateway: "216.18.214.193".to_string(),
            ip_range_from: "216.18.214.195".to_string(),
            ip_range_to: "216.18.214.206".to_string(),
            org_id: None,
        }),
    };
    let provision: ui_pb::CreateHostProvisionResponse = client
        .create(with_auth(provision_create, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("host provision: {provision:?}");
    let otp = get_first_message(provision.meta);

    println!("bvup");
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";
    let registry = "http://localhost:50051";
    let mqtt = "mqtt://localhost:1883";

    Command::cargo_bin("bvup")
        .unwrap()
        .args([&otp, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--keys", url])
        .args(["--registry", registry])
        .args(["--mqtt", mqtt])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Provision and init blockvisor configuration",
        ));

    println!("read host id");
    let config_path = "/etc/blockvisor.toml";
    let config = fs::read_to_string(config_path).unwrap();
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

    let mut node_client = ui_pb::node_service_client::NodeServiceClient::connect(url)
        .await
        .unwrap();

    let node_create = ui_pb::CreateNodeRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        node: Some(ui_pb::Node {
            id: None,
            org_id: Some(org_id.clone()),
            host_id: Some(host_id.to_string()),
            host_name: None,
            blockchain_id: Some(blockchain_id.to_string()),
            name: None,
            groups: vec![],
            version: Some("0.0.1".to_string()),
            ip: None,
            r#type: Some(
                json!({"id": 3, "properties": [{"name": "TESTING_PARAM","label":"label","description":"description","ui_type":"unknown","disabled":false,"required":true,"value": "anything"}]})
                    .to_string(),
            ), // validator
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
            self_update: Some(false),
            network: Some("".to_string()),
            blockchain_name: Some("aetherium".to_string()),
        }),
    };
    let node: ui_pb::CreateNodeResponse = node_client
        .create(with_auth(node_create, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("created node: {node:?}");
    let node_id = get_first_message(node.meta);

    sleep(Duration::from_secs(30)).await;

    println!("list created node, should be auto-started");
    bv_run(&["node", "status", &node_id], "Running");

    let mut client = ui_pb::command_service_client::CommandServiceClient::connect(url)
        .await
        .unwrap();

    println!("check node keys");
    bv_run(&["node", "keys", &node_id], "first");

    let node_stop = ui_pb::CommandRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        id: host_id.to_string(),
        params: vec![ui_pb::Parameter {
            name: "resource_id".to_string(),
            value: node_id.clone(),
        }],
    };
    let command: ui_pb::CommandResponse = client
        .stop_node(with_auth(node_stop, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("executed stop node command: {command:?}");

    sleep(Duration::from_secs(15)).await;

    println!("get node status");
    bv_run(&["node", "status", &node_id], "Stopped");

    let node_delete = ui_pb::DeleteNodeRequest {
        meta: Some(ui_pb::RequestMeta::default()),
        id: node_id.clone(),
    };
    node_client
        .delete(with_auth(node_delete, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();

    sleep(Duration::from_secs(10)).await;

    println!("check node is deleted");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(["node", "status", &node_id])
        .env("NO_COLOR", "1")
        .assert()
        .failure();
}

#[tokio::test]
#[serial]
async fn test_bv_cmd_grpc_stub_init_reset() {
    use std::path::Path;

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

    tokio::task::spawn_blocking(move || {
        let tmp_dir = TempDir::new().unwrap();
        let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
        let url = "http://localhost:8082";
        let mqtt = "mqtt://localhost:1883";
        let otp = "AWESOME";
        let config_path = format!("{}/etc/blockvisor.toml", tmp_dir.to_string_lossy());

        println!("bvup");
        Command::cargo_bin("bvup")
            .unwrap()
            .args([otp, "--skip-download"])
            .args(["--ifa", ifa])
            .args(["--api", url])
            .args(["--keys", url])
            .args(["--registry", url])
            .args(["--mqtt", mqtt])
            .env("BV_ROOT", tmp_dir.as_os_str())
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "Provision and init blockvisor configuration",
            ));

        assert!(Path::new(&config_path).exists());

        println!("bv reset");
        Command::cargo_bin("bv")
            .unwrap()
            .args(["reset", "--yes"])
            .env("BV_ROOT", tmp_dir.as_os_str())
            .assert()
            .success()
            .stdout(predicate::str::contains("Deleting host"));

        assert!(!Path::new(&config_path).exists());
    })
    .await
    .unwrap();
}

fn get_first_message(meta: Option<ui_pb::ResponseMeta>) -> String {
    meta.unwrap().messages.first().unwrap().clone()
}

fn with_auth<T>(inner: T, auth_token: &str, refresh_token: &str) -> Request<T> {
    let mut request = Request::new(inner);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", auth_token).parse().unwrap(),
    );
    request.metadata_mut().insert(
        "cookie",
        format!("refresh={}", refresh_token).parse().unwrap(),
    );
    println!("{:?}", request.metadata());
    request
}

//// =========== HOST+COOKBOOK INTEGRATION: bv cli host + cookbook
#[tokio::test]
async fn test_bv_chain_list() {
    bv_run(&["chain", "list", "testing", "validator"], "");
}

#[tokio::test]
#[serial]
async fn test_bv_cmd_cookbook_download() {
    use blockvisord::node_data::NodeImage;
    use blockvisord::services::cookbook::CookbookService;
    use std::path::Path;

    let folder = CookbookService::get_image_download_folder_path(
        Path::new("/"),
        &NodeImage {
            protocol: "testing".to_string(),
            node_type: "validator".to_string(),
            node_version: "0.0.3".to_string(),
        },
    );
    let _ = tokio::fs::remove_dir_all(&folder).await;

    println!("create a node");
    let vm_id = &create_node("testing/validator/0.0.3");
    println!("create vm_id: {vm_id}");

    println!("delete node");
    bv_run(&["node", "delete", vm_id], "Deleted node");

    assert!(Path::new(&folder.join("kernel")).exists());
    assert!(Path::new(&folder.join("os.img")).exists());
    assert!(Path::new(&folder.join("babel.toml")).exists());
}

//// =========== HOST+BACKEND MOCK INTEGRATION: bv nodes host + backend (cmds)
#[tokio::test]
#[serial]
async fn test_bv_cmd_grpc_commands() {
    use blockvisord::config::Config;
    use blockvisord::nodes::Nodes;
    use blockvisord::server::bv_pb;
    use pb::node_info::ContainerStatus;
    use serde_json::json;
    use uuid::Uuid;

    let host_id = Uuid::new_v4().to_string();
    let node_name = "beautiful-node-name".to_string();
    let node_id = Uuid::new_v4().to_string();
    let id = node_id.clone();
    let command_id = Uuid::new_v4().to_string();

    let babel_dir = fs::canonicalize(env::current_exe().unwrap())
        .unwrap()
        .parent()
        .unwrap()
        .join("../../babel/bin");
    fs::create_dir_all(&babel_dir).unwrap();
    fs::copy(
        "/opt/blockvisor/current/babel/bin/babel",
        babel_dir.join("babel"),
    )
    .unwrap();
    println!("delete existing node, if any");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(["node", "delete", &node_name]).assert();

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
                        node_version: "0.0.2".to_string(),
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
                        node_version: "0.0.2".to_string(),
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
                        node_version: "0.0.2".to_string(),
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
                        node_version: "0.0.3".to_string(),
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
        blockjoy_keys_url: "http://localhost:8089".to_string(),
        blockjoy_registry_url: "http://localhost:50059".to_string(),
        blockjoy_mqtt_url: "mqtt://localhost:1889".to_string(),
        update_check_interval_secs: None,
        blockvisor_port: default_blockvisor_port(),
    };

    let nodes = Nodes::load(LinuxPlatform::new().unwrap(), config.clone())
        .await
        .unwrap();
    let nodes = Arc::new(RwLock::new(nodes));

    let client_future = async {
        match api::CommandsService::connect(&config.blockjoy_api_url, &config.token).await {
            Ok(mut client) => {
                if let Err(e) = client
                    .get_and_process_pending_commands(&config.id, nodes.clone())
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
}
