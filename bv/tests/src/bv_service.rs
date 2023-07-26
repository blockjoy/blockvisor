use crate::src::utils::{stub_server::StubHostsServer, test_env};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::{
    config::Config, node_data::NodeImage, services::api::pb, services::cookbook::CookbookService,
};
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
            .args(["--mqtt", mqtt])
            .args(["--ip-gateway", "216.18.214.193"])
            .args(["--ip-range-from", "216.18.214.195"])
            .args(["--ip-range-to", "216.18.214.206"])
            .args(["--yes"])
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
    let url = "http://localhost:8080";
    let email = "tester@blockjoy.com";
    let password = "ilovemytests";
    let user_id = "1cff0487-412b-4ca4-a6cd-fdb9957d5d2f";
    let org_id = "53b28794-fb68-4cd1-8165-b98a51a19c46";
    let db_url = "postgres://blockvisor:password@database:5432/blockvisor_db";

    println!("create user");
    let user_query = r#"INSERT INTO users
        VALUES ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', 'tester@blockjoy.com', '57snVgOUjwtfOrMxLHez8KOQaTNaNnLXMkUpzaxoRDs', 'cM4OaOTJUottdF4i8unbuA', '2023-01-17 22:13:52.422342+00', 'Luuk', 'Wester', '2023-01-17 22:14:06.297602+00');
        "#;
    execute_sql(db_url, user_query);

    println!("login user");
    let mut client = pb::auth_service_client::AuthServiceClient::connect(url)
        .await
        .unwrap();

    let login_user = pb::AuthServiceLoginRequest {
        email: email.to_string(),
        password: password.to_string(),
    };
    let login = client.login(login_user).await.unwrap().into_inner();
    println!("user login: {login:?}");

    println!("get user org and token");
    let org_query = r#"INSERT INTO orgs VALUES ('53b28794-fb68-4cd1-8165-b98a51a19c46', 'Personal', TRUE, now(), now(), NULL);
        INSERT INTO orgs_users VALUES ('53b28794-fb68-4cd1-8165-b98a51a19c46', '1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', 'owner', now(), now(), 'rgfr4YJZ8dIA');
        "#;
    execute_sql(db_url, org_query);

    let auth_token = login.token;

    let get_token = pb::OrgServiceGetProvisionTokenRequest {
        user_id: user_id.to_string(),
        org_id: org_id.to_string(),
    };

    let mut client = pb::org_service_client::OrgServiceClient::connect(url)
        .await
        .unwrap();

    let response = client
        .get_provision_token(with_auth(get_token, &auth_token))
        .await
        .unwrap()
        .into_inner();
    let provision_token = response.token;
    println!("host provision token: {provision_token}");

    println!("add blockchain");
    let blockchain_query = r#"INSERT INTO blockchains (id, name) values ('ab5d8cfc-77b1-4265-9fee-ba71ba9de092', 'Testing');
        INSERT INTO blockchain_properties VALUES ('5972a35a-333c-421f-ab64-a77f4ae17533', 'ab5d8cfc-77b1-4265-9fee-ba71ba9de092', '0.0.3', 'validator', 'keystore-file', NULL, 'file_upload', FALSE, FALSE);
        INSERT INTO blockchain_properties VALUES ('a989ad08-b455-4a57-9fe0-696405947e48', 'ab5d8cfc-77b1-4265-9fee-ba71ba9de092', '0.0.3', 'validator', 'TESTING_PARAM', NULL, 'text', FALSE, FALSE);
        "#;
    execute_sql(db_url, blockchain_query);

    println!("bvup");
    let url = "http://localhost:8080";
    let mqtt = "mqtt://localhost:1883";

    Command::cargo_bin("bvup")
        .unwrap()
        .args([&provision_token, "--skip-download"])
        .args(["--region", "europe-bosnia-number-1"]) // this region will be auto-created in API
        .args(["--api", url])
        .args(["--mqtt", mqtt])
        .args(["--yes"])
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

    println!("test host info update");
    test_env::bv_run(&["host", "update"], "Host info update sent", None);

    println!("test chain list query");
    test_env::bv_run(&["chain", "list", "testing", "validator"], "0.0.3", None);

    println!("removing 0.0.3 image from cache to download it again");
    let folder = CookbookService::get_image_download_folder_path(
        Path::new("/"),
        &NodeImage {
            protocol: "testing".to_string(),
            node_type: "validator".to_string(),
            node_version: "0.0.3".to_string(),
        },
    );
    let _ = tokio::fs::remove_dir_all(&folder).await;

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
        org_id: org_id.to_string(),
        blockchain_id: blockchain.id.to_string(),
        version: "0.0.3".to_string(),
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
                    region: "europe-bosnia-number-1".to_string(),
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
    test_env::wait_for_node_status(&node_id, "Running", Duration::from_secs(120), None).await;

    println!("check node keys");
    test_env::bv_run(&["node", "keys", &node_id], "first", None);

    println!("start download job");
    test_env::bv_run(
        &["node", "run", "download", &node_id],
        "Download started!",
        None,
    );
    println!("wait for download finished");
    let start = std::time::Instant::now();
    while let Err(err) = test_env::try_bv_run(
        &["node", "run", "download_status", &node_id],
        r#"#{"finished": #{"exit_code": 0, "message": ""}}"#,
        None,
    ) {
        if start.elapsed() < Duration::from_secs(15) {
            sleep(Duration::from_secs(1)).await;
        } else {
            panic!("timeout expired: {err:#}")
        }
    }
    println!("verify downloaded data");
    test_env::bv_run(
        &["node", "run", "--param=file_a", "data_file_sha1", &node_id],
        "87661bf203551efa7fa6a938a372bbba1eb36a1b",
        None,
    );
    test_env::bv_run(
        &["node", "run", "--param=file_b", "data_file_sha1", &node_id],
        "516e8ffce053defa048255e19b2abf3ec7f44f3d",
        None,
    );
    test_env::bv_run(
        &["node", "run", "--param=file_c", "data_file_sha1", &node_id],
        "f8f81579034e0dd70e42d4a72f760923c2b22dd5",
        None,
    );
    test_env::bv_run(
        &[
            "node",
            "run",
            "--param='sub/file_d'",
            "data_file_sha1",
            &node_id,
        ],
        "c15618007493a7a2eaff43cd38b3fbb98ddddd24",
        None,
    );
    test_env::bv_run(
        &[
            "node",
            "run",
            "--param='sub/file_e'",
            "data_file_sha1",
            &node_id,
        ],
        "024262e7a10be0426cda667a29266b46d04a11fc",
        None,
    );

    let node_stop = pb::NodeServiceStopRequest {
        id: node_id.clone(),
    };
    let resp = node_client
        .stop(with_auth(node_stop, &auth_token))
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

fn execute_sql(connection_str: &str, query: &str) {
    Command::new("docker")
        .args(&[
            "compose",
            "exec",
            "-T",
            "database",
            "psql",
            connection_str,
            "-c",
            query,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("INSERT"));
}
