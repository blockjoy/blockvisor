use crate::src::utils::{stub_server::StubHostsServer, test_env, token};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::services::api::pb;
use predicates::prelude::*;
use serial_test::serial;
use std::{fs, net::ToSocketAddrs, path::Path};
use tokio::time::{sleep, Duration};
use tonic::{transport::Server, Request};

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

    let url = "http://localhost:8080";
    let email = "user1@example.com";
    let password = "user1pass";

    let mut client = pb::users_client::UsersClient::connect(url).await.unwrap();

    println!("create user");
    let create_user = pb::CreateUserRequest {
        email: email.to_string(),
        first_name: "first".to_string(),
        last_name: "last".to_string(),
        password: password.to_string(),
    };
    let resp: pb::CreateUserResponse = client.create(create_user).await.unwrap().into_inner();
    println!("user created: {resp:?}");
    let user_id = resp.user.unwrap().id.parse().unwrap();

    println!("confirm user");
    let mut client = pb::authentication_client::AuthenticationClient::connect(url)
        .await
        .unwrap();
    let confirm_user = pb::ConfirmRegistrationRequest {};
    let refresh_token = token::TokenGenerator::create_refresh(user_id, "23942390".to_string());
    let register_token =
        token::TokenGenerator::create_register(user_id, "23ß357320".to_string(), email.to_string());
    client
        .confirm(with_auth(confirm_user, &register_token, &refresh_token))
        .await
        .unwrap();

    println!("login user");
    let login_user = pb::LoginUserRequest {
        email: email.to_string(),
        password: password.to_string(),
    };
    let login: pb::LoginUserResponse = client.login(login_user).await.unwrap().into_inner();
    println!("user login: {login:?}");
    let login_auth: token::AuthClaim =
        token::TokenGenerator::from_encoded("1245456".to_string(), &login.token).unwrap();
    let login_data = login_auth.data.unwrap();
    let org_id = login_data.get("org_id").unwrap();

    let auth_token = token::TokenGenerator::create_auth(
        user_id,
        "1245456".to_string(),
        org_id.clone(),
        email.to_string(),
    );

    println!("add blockchain");
    let db_url = "postgres://blockvisor:password@database:5432/blockvisor_db";
    let db_query = format!(
        r#"INSERT INTO blockchains (name, status, supported_node_types) values ('Testing', 'production', '[{{"id": 3, "version": "0.0.3", "properties": [{{"name": "self-hosted", "default": "false", "ui_type": "switch", "disabled": true, "required": true}}]}}]');"#
    );

    Command::new("docker")
        .args(&[
            "compose", "run", "-it", "database", "psql", db_url, "-c", &db_query,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("INSERT"));

    println!("create host provision");
    let mut client = pb::host_provisions_client::HostProvisionsClient::connect(url)
        .await
        .unwrap();

    let provision_create = pb::CreateHostProvisionRequest {
        ip_gateway: "216.18.214.193".to_string(),
        ip_range_from: "216.18.214.195".to_string(),
        ip_range_to: "216.18.214.206".to_string(),
    };
    let response: pb::CreateHostProvisionResponse = client
        .create(with_auth(provision_create, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("host provision: {response:?}");
    let otp = response.host_provision.unwrap().id;

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
    let mut client = pb::blockchains_client::BlockchainsClient::connect(url)
        .await
        .unwrap();

    let list_blockchains = pb::ListBlockchainsRequest {};
    let list: pb::ListBlockchainsResponse = client
        .list(with_auth(list_blockchains, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    let blockchain = list.blockchains.first().unwrap();
    println!("got blockchain: {:?}", blockchain);

    let mut node_client = pb::nodes_client::NodesClient::connect(url).await.unwrap();

    let node_create = pb::CreateNodeRequest {
        org_id: org_id.clone(),
        blockchain_id: blockchain.id.clone(),
        version: "0.0.1".to_string(),
        node_type: pb::node::NodeType::Validator.into(),
        properties: vec![pb::node::NodeProperty {
            name: "TESTING_PARAM".to_string(),
            label: "testeronis".to_string(),
            description: "this param is for testing".to_string(),
            ui_type: pb::UiType::Text.into(),
            disabled: false,
            required: true,
            value: Some("I guess just some test value".to_string()),
        }],
        network: "".to_string(),
        scheduler: Some(pb::NodeScheduler {
            similarity: None,
            resource: pb::node_scheduler::ResourceAffinity::LeastResources.into(),
        }),
    };
    let resp: pb::CreateNodeResponse = node_client
        .create(with_auth(node_create, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("created node: {resp:?}");
    let node_id = resp.node.unwrap().id;

    sleep(Duration::from_secs(30)).await;

    println!("list created node, should be auto-started");
    test_env::bv_run(&["node", "status", &node_id], "Running", None);

    let mut client = pb::commands_client::CommandsClient::connect(url)
        .await
        .unwrap();

    println!("check node keys");
    test_env::bv_run(&["node", "keys", &node_id], "first", None);

    let node_stop = pb::CreateCommandRequest {
        command: Some(pb::create_command_request::Command::StopNode(
            pb::StopNodeCommand {
                node_id: node_id.clone(),
            },
        )),
    };
    let resp: pb::CreateCommandResponse = client
        .create(with_auth(node_stop, &auth_token, &refresh_token))
        .await
        .unwrap()
        .into_inner();
    println!("executed stop node command: {resp:?}");

    sleep(Duration::from_secs(15)).await;

    println!("get node status");
    test_env::bv_run(&["node", "status", &node_id], "Stopped", None);

    let node_delete = pb::DeleteNodeRequest {
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
