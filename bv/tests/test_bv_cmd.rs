use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::linux_platform::LinuxPlatform;
use blockvisord::services::api::{self, pb};
use blockvisord::set_bv_status;
use futures_util::FutureExt;
use predicates::prelude::*;
use serial_test::serial;
use std::{env, fs};
use std::{net::ToSocketAddrs, sync::Arc};
use sysinfo::{Pid, PidExt, ProcessExt, ProcessRefreshKind, System, SystemExt};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::{sleep, Duration};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tonic::{
    transport::{Endpoint, Server},
    Request,
};

pub mod ui_pb {
    tonic::include_proto!("blockjoy.api.ui_v1");
}

mod stub_server;

mod token;

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
//// =========== HOST INTEGRATION: bv cli host only
#[test]
#[serial]
fn test_bv_cmd_start_no_init() {
    let tmp_dir = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr("Error: Host is not registered, please run `bvup` first\n");
}

#[test]
#[serial]
fn test_bv_cmd_restart() {
    bv_run(&["stop"], "blockvisor service stopped successfully");
    bv_run(&["status"], "Service stopped");
    bv_run(&["start"], "blockvisor service started successfully");
    bv_run(&["status"], "Service running");
    bv_run(&["start"], "Service already running");
}

#[test]
#[serial]
fn test_bv_host_metrics() {
    bv_run(&["host", "metrics"], "Used cpu:");
}

#[test]
#[serial]
fn test_bv_chain_list() {
    bv_run(&["chain", "list", "testing", "validator"], "");
}

#[test]
#[serial]
fn test_bv_cmd_delete_all() {
    bv_run(&["node", "rm", "--all", "--yes"], "");
}

#[test]
#[serial]
fn test_bv_cmd_node_start_and_stop_all() {
    const NODES_COUNT: usize = 2;
    println!("create {NODES_COUNT} nodes");
    let mut nodes: Vec<String> = Default::default();
    for _ in 0..NODES_COUNT {
        nodes.push(create_node("testing/validator/0.0.1"));
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
fn test_bv_cmd_logs() {
    println!("create a node");
    let vm_id = &create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("start node");
    bv_run(&["node", "start", vm_id], "Started node");

    println!("get logs");
    bv_run(
        &["node", "logs", vm_id],
        "Testing entry_point not configured, but parametrized with anything!",
    );

    println!("stop started node");
    bv_run(&["node", "stop", vm_id], "Stopped node");
}

#[tokio::test]
#[serial]
async fn test_bv_cmd_node_lifecycle() {
    println!("create a node");
    let vm_id = &create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("stop stopped node");
    bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("start stopped node");
    bv_run(&["node", "start", vm_id], "Started node");

    println!("stop started node");
    bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("restart stopped node");
    bv_run(&["node", "start", vm_id], "Started node");

    println!("query metrics");
    bv_run(&["node", "metrics", vm_id], "In consensus:        false");

    println!("list running node before service restart");
    bv_run(&["node", "status", vm_id], "Running");

    println!("stop service");
    bv_run(&["stop"], "blockvisor service stopped successfully");

    println!("start service");
    bv_run(&["start"], "blockvisor service started successfully");

    println!("list running node after service restart");
    bv_run(&["node", "status", vm_id], "Running");

    println!("upgrade running node");
    bv_run(
        &["node", "upgrade", vm_id, "testing/validator/0.0.2"],
        "Upgraded node",
    );

    println!("list running node after node upgrade");
    bv_run(&["node", "status", vm_id], "Running");

    println!("generate node keys");
    bv_run(&["node", "run", vm_id, "generate_keys"], "");

    println!("check node keys");
    bv_run(&["node", "keys", vm_id], "first");

    println!("delete started node");
    bv_run(&["node", "delete", vm_id], "Deleted node");
}

#[tokio::test]
#[serial]
async fn test_bv_cmd_node_recovery() {
    use blockvisord::{node::FC_BIN_NAME, utils};

    println!("create a node");
    let vm_id = &create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    bv_run(&["node", "status", vm_id], "Running");

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
    bv_run(&["node", "status", vm_id], "Failed");

    sleep(Duration::from_secs(10)).await;

    println!("list running node after recovery");
    bv_run(&["node", "status", vm_id], "Running");

    println!("delete started node");
    bv_run(&["node", "delete", vm_id], "Deleted node");
}

//// =========== E2E: bv cli host + backend (otp)
#[test]
#[serial]
fn test_bv_cmd_init_unknown_otp() {
    let tmp_dir = TempDir::new().unwrap();

    let otp = "NOT_FOUND";
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";

    let mut cmd = Command::cargo_bin("bvup").unwrap();
    cmd.args([otp, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--keys", url])
        .args(["--registry", url])
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Host provision not found: no rows returned by a query that expected to return at least one row"));
}

//// =========== E2E: bv cli host + ui api + backend (otp)
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

    Command::cargo_bin("bvup")
        .unwrap()
        .args([&otp, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--keys", url])
        .args(["--registry", registry])
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

    tokio::task::spawn_blocking(move || {
        let tmp_dir = TempDir::new().unwrap();
        let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
        let url = "http://localhost:8082";
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

//// =========== HOST+BACKEND INTEGRATION: bv nodes host + backend (cmds)
#[tokio::test]
#[serial]
async fn test_bv_cmd_grpc_commands() {
    use blockvisord::config::Config;
    use blockvisord::nodes::Nodes;
    use blockvisord::server::bv_pb;
    use blockvisord::services::api::process_commands_stream;
    use serde_json::json;
    use std::str::FromStr;
    use stub_server::StubServer;
    use uuid::Uuid;

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
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
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
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
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
                id: Uuid::new_v4().to_string(),
                api_command_id: command_id.clone(),
                created_at: None,
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
        // upgrade running
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: id.clone(),
                api_command_id: command_id.clone(),
                created_at: None,
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
    set_bv_status(bv_pb::ServiceStatus::Ok).await;
    let config = Config {
        id: Uuid::new_v4().to_string(),
        token: "any token".to_string(),
        blockjoy_api_url: "http://localhost:8081".to_string(),
        blockjoy_keys_url: "http://localhost:8081".to_string(),
        blockjoy_registry_url: "http://localhost:50051".to_string(),
        update_check_interval_secs: None,
    };

    let nodes = Nodes::load(LinuxPlatform::new().unwrap(), config.clone())
        .await
        .unwrap();
    let updates_tx = nodes.get_updates_sender().await.unwrap().clone();
    let nodes = Arc::new(RwLock::new(nodes));

    let token = api::AuthToken(config.token);
    let endpoint = Endpoint::from_str(&config.blockjoy_api_url).unwrap();
    let client_future = async {
        sleep(Duration::from_secs(5)).await;
        let channel = Endpoint::connect(&endpoint).await.unwrap();
        let mut client = api::CommandsClient::with_auth(channel, token);
        process_commands_stream(&mut client, nodes.clone(), updates_tx.clone())
            .await
            .unwrap();
    };

    println!("run server");
    tokio::select! {
        _ = server_future => {},
        _ = client_future => {},
        _ = sleep(Duration::from_secs(240)) => {},
    }

    println!("check received updates");
    let updates: Vec<_> = ReceiverStream::new(updates_rx)
        .timeout(Duration::from_secs(1))
        .take_while(Result::is_ok)
        .collect()
        .await;
    let updates_count = updates.len();

    println!("got updates: {updates:?}");
    let expected_updates = vec![
        ack_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Creating),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        error_command_update(&command_id, format!("Node with name `{node_name}` exists")),
        ack_command_update(&command_id),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopping),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopping),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        node_update(&node_id, pb::node_info::ContainerStatus::Upgrading),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopping),
        node_update(&node_id, pb::node_info::ContainerStatus::Stopped),
        node_update(&node_id, pb::node_info::ContainerStatus::Starting),
        node_update(&node_id, pb::node_info::ContainerStatus::Running),
        node_update(&node_id, pb::node_info::ContainerStatus::Upgraded),
        success_command_update(&command_id),
        ack_command_update(&command_id),
        success_command_update(&command_id),
    ];
    let expected_count = expected_updates.len();

    for (actual, expected) in updates.into_iter().zip(expected_updates) {
        assert_eq!(actual.unwrap(), expected);
    }

    assert_eq!(updates_count, expected_count);
}

fn node_update(node_id: &str, status: pb::node_info::ContainerStatus) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Node(pb::NodeInfo {
            id: node_id.to_string(),
            container_status: Some(status.into()),
            ..Default::default()
        })),
    }
}

fn error_command_update(command_id: &str, message: String) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Command(pb::CommandInfo {
            id: command_id.to_string(),
            response: Some(message),
            exit_code: Some(1),
        })),
    }
}

fn ack_command_update(command_id: &str) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Command(pb::CommandInfo {
            id: command_id.to_string(),
            response: None,
            exit_code: None,
        })),
    }
}

fn success_command_update(command_id: &str) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Command(pb::CommandInfo {
            id: command_id.to_string(),
            response: None,
            exit_code: Some(0),
        })),
    }
}
