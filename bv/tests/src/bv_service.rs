use crate::src::utils::{stub_server::StubHostsServer, test_env, token};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::services::api::pb;
use predicates::prelude::*;
use serial_test::serial;
use std::{fs, net::ToSocketAddrs, path::Path};
use tokio::time::{sleep, Duration};
use tonic::{transport::Server, Request};

pub mod ui_pb {
    tonic::include_proto!("blockjoy.api.ui_v1");
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

#[test]
#[serial]
fn test_bv_service_restart_with_cli() {
    test_env::bv_run(&["stop"], "blockvisor service stopped successfully", None);
    test_env::bv_run(&["status"], "Service stopped", None);
    test_env::bv_run(&["start"], "blockvisor service started successfully", None);
    test_env::bv_run(&["status"], "Service running", None);
    test_env::bv_run(&["start"], "Service already running", None);
}

#[tokio::test]
#[serial]
async fn test_bvup_and_reset() {
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

#[tokio::test]
#[serial]
async fn test_bv_service_e2e() {
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
    test_env::bv_run(&["stop"], "blockvisor service stopped successfully", None);
    test_env::bv_run(&["start"], "blockvisor service started successfully", None);

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
            created_by: None,
            created_by_name: None,
            created_by_email: None,
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
    test_env::bv_run(&["node", "status", &node_id], "Running", None);

    let mut client = ui_pb::command_service_client::CommandServiceClient::connect(url)
        .await
        .unwrap();

    println!("check node keys");
    test_env::bv_run(&["node", "keys", &node_id], "first", None);

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
    test_env::bv_run(&["node", "status", &node_id], "Stopped", None);

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
