use crate::src::utils::{stub_server::StubHostsServer, test_env, token};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::services::api::pb;
use predicates::prelude::*;
use serial_test::serial;
use std::{fs, net::ToSocketAddrs, path::Path};
use tokio::time::{sleep, Duration};
use tonic::{transport::Server, Request};

fn with_auth<T>(inner: T, auth_token: &str) -> Request<T> {
    let mut request = Request::new(inner);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", auth_token).parse().unwrap(),
    );
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
async fn test_bvup() {
    let server = StubHostsServer {};

    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::host_service_server::HostServiceServer::new(server))
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
        let provision_token = "AWESOME";
        let config_path = format!("{}/etc/blockvisor.json", tmp_dir.to_string_lossy());

        println!("bvup");
        Command::cargo_bin("bvup")
            .unwrap()
            .args([provision_token, "--skip-download"])
            .args(["--ifa", ifa])
            .args(["--api", url])
            .args(["--keys", url])
            .args(["--registry", url])
            .args(["--mqtt", mqtt])
            .args(["--ip-gateway", "216.18.214.193"])
            .args(["--ip-range-from", "216.18.214.195"])
            .args(["--ip-range-to", "216.18.214.206"])
            .env("BV_ROOT", tmp_dir.as_os_str())
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "Provision and init blockvisor configuration",
            ));

        assert!(Path::new(&config_path).exists());
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

    let mut client = pb::user_service_client::UserServiceClient::connect(url)
        .await
        .unwrap();

    println!("create user");
    let create_user = pb::UserServiceCreateRequest {
        email: email.to_string(),
        first_name: "first".to_string(),
        last_name: "last".to_string(),
        password: password.to_string(),
    };
    let resp = client.create(create_user).await.unwrap().into_inner();
    println!("user created: {resp:?}");
    let user_id = resp.user.unwrap().id.parse().unwrap();

    println!("confirm user");
    let mut client = pb::auth_service_client::AuthServiceClient::connect(url)
        .await
        .unwrap();
    let confirm_user = pb::AuthServiceConfirmRequest {};
    let register_token = token::TokenGenerator::create_register(user_id, "1245456");
    client
        .confirm(with_auth(confirm_user, &register_token))
        .await
        .unwrap();

    println!("login user");
    let login_user = pb::AuthServiceLoginRequest {
        email: email.to_string(),
        password: password.to_string(),
    };
    let login = client.login(login_user).await.unwrap().into_inner();
    println!("user login: {login:?}");

    println!("get user org and token");
    let mut client = pb::org_service_client::OrgServiceClient::connect(url)
        .await
        .unwrap();
    let orgs = client
        .list(with_auth(
            pb::OrgServiceListRequest {
                member_id: Some(user_id.to_string()),
            },
            &login.token,
        ))
        .await
        .unwrap()
        .into_inner();
    let org_id = orgs.orgs[0].id.clone();

    let auth_token = token::TokenGenerator::create_auth(user_id, "1245456");

    let get_token = pb::OrgServiceGetProvisionTokenRequest {
        user_id: user_id.to_string(),
        org_id: org_id.clone(),
    };

    let response = client
        .get_provision_token(with_auth(get_token, &auth_token))
        .await
        .unwrap()
        .into_inner();
    let provision_token = response.token;
    println!("host provision token: {provision_token}");

    println!("add blockchain");
    let db_url = "postgres://blockvisor:password@database:5432/blockvisor_db";
    let db_query =
        r#"INSERT INTO blockchains (id, name, status) values ('ab5d8cfc-77b1-4265-9fee-ba71ba9de092', 'Testing', 'production');
        INSERT INTO blockchain_properties VALUES ('5972a35a-333c-421f-ab64-a77f4ae17533', 'ab5d8cfc-77b1-4265-9fee-ba71ba9de092', '0.0.1', 'validator', 'keystore-file', NULL, 'file_upload', FALSE, FALSE);
        INSERT INTO blockchain_properties VALUES ('a989ad08-b455-4a57-9fe0-696405947e48', 'ab5d8cfc-77b1-4265-9fee-ba71ba9de092', '0.0.1', 'validator', 'TESTING_PARAM', NULL, 'text', FALSE, FALSE);
        "#.to_string();

    Command::new("docker")
        .args(&[
            "compose", "exec", "-T", "database", "psql", db_url, "-c", &db_query,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("INSERT"));

    println!("bvup");
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";
    let registry = "http://localhost:50051";
    let mqtt = "mqtt://localhost:1883";

    Command::cargo_bin("bvup")
        .unwrap()
        .args([&provision_token, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--keys", url])
        .args(["--registry", registry])
        .args(["--mqtt", mqtt])
        .args(["--ip-gateway", "216.18.214.193"])
        .args(["--ip-range-from", "216.18.214.195"])
        .args(["--ip-range-to", "216.18.214.206"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Provision and init blockvisor configuration",
        ));

    println!("read host id");
    let config_path = "/etc/blockvisor.json";
    let config = fs::read_to_string(config_path).unwrap();
    let config: Config = serde_json::from_str(&config).unwrap();
    let host_id = config.id;
    println!("got host id: {host_id}");

    println!("restart blockvisor");
    test_env::bv_run(&["stop"], "blockvisor service stopped successfully", None);
    test_env::bv_run(&["start"], "blockvisor service started successfully", None);

    println!("get blockchain id");
    let mut client = pb::blockchain_service_client::BlockchainServiceClient::connect(url)
        .await
        .unwrap();

    let list_blockchains = pb::BlockchainServiceListRequest {};
    let list = client
        .list(with_auth(list_blockchains, &auth_token))
        .await
        .unwrap()
        .into_inner();
    let blockchain = list.blockchains.first().unwrap();
    println!("got blockchain: {:?}", blockchain);

    let mut node_client = pb::node_service_client::NodeServiceClient::connect(url)
        .await
        .unwrap();

    let node_create = pb::NodeServiceCreateRequest {
        org_id,
        blockchain_id: blockchain.id.to_string(),
        version: "0.0.1".to_string(),
        node_type: 3, // validator
        properties: vec![pb::NodeProperty {
            name: "TESTING_PARAM".to_string(),
            ui_type: pb::UiType::Text.into(),
            disabled: false,
            required: true,
            value: "I guess just some test value".to_string(),
        }],
        network: blockchain.networks[0].clone().name,
        placement: Some(pb::NodePlacement {
            placement: Some(pb::node_placement::Placement::Scheduler(
                pb::NodeScheduler {
                    similarity: None,
                    resource: pb::node_scheduler::ResourceAffinity::LeastResources.into(),
                },
            )),
        }),
        allow_ips: vec![],
        deny_ips: vec![],
    };
    let resp = node_client
        .create(with_auth(node_create, &auth_token))
        .await
        .unwrap()
        .into_inner();
    println!("created node: {resp:?}");
    let node_id = resp.node.unwrap().id;

    println!("list created node, should be auto-started");
    test_env::wait_for_node_status(&node_id, "Running", Duration::from_secs(60), None).await;

    let mut client = pb::command_service_client::CommandServiceClient::connect(url)
        .await
        .unwrap();

    println!("check node keys");
    test_env::bv_run(&["node", "keys", &node_id], "first", None);

    let node_stop = pb::CommandServiceCreateRequest {
        command: Some(pb::command_service_create_request::Command::StopNode(
            pb::StopNodeCommand {
                node_id: node_id.clone(),
            },
        )),
    };
    let resp = client
        .create(with_auth(node_stop, &auth_token))
        .await
        .unwrap()
        .into_inner();
    println!("executed stop node command: {resp:?}");

    println!("get node status");
    test_env::wait_for_node_status(&node_id, "Stopped", Duration::from_secs(60), None).await;

    let node_delete = pb::NodeServiceDeleteRequest {
        id: node_id.clone(),
    };
    node_client
        .delete(with_auth(node_delete, &auth_token))
        .await
        .unwrap()
        .into_inner();

    println!("check if node is deleted");
    let is_deleted = || {
        let mut cmd = Command::cargo_bin("bv").unwrap();
        cmd.args(["node", "status", &node_id])
            .env("NO_COLOR", "1")
            .assert()
            .try_failure()
            .is_ok()
    };
    let start = std::time::Instant::now();
    while !is_deleted() {
        if start.elapsed() < Duration::from_secs(30) {
            sleep(Duration::from_secs(1)).await;
        } else {
            panic!("timeout expired")
        }
    }
}
