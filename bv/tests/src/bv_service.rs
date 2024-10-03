use crate::src::utils::test_env::link_apptainer_config;
use crate::src::utils::{
    execute_sql, execute_sql_insert, rbac, stub_server::StubHostsServer, test_env,
};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::bv_config::ApptainerConfig;
use blockvisord::{bv_config::Config, node_state::NodeState, services::api::pb};
use predicates::prelude::*;
use serial_test::serial;
use std::{fs, net::ToSocketAddrs, path::Path, str};
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

#[test]
#[serial]
fn test_bvup_unknown_provision_token() {
    let tmp_dir = TempDir::new().unwrap();
    link_apptainer_config(&tmp_dir).unwrap();

    let provision_token = "NOT_FOUND";
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";
    let mqtt = "mqtt://localhost:1883";

    // make sure blockvisord is not running
    test_env::bv_run(&["stop"], "", None);
    let mut cmd = Command::cargo_bin("bvup").unwrap();
    cmd.args([provision_token, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--mqtt", mqtt])
        .args(["--ip-gateway", "216.18.214.193"])
        .args(["--ip-range-from", "216.18.214.195"])
        .args(["--ip-range-to", "216.18.214.206"])
        .args(["--yes"])
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid token"));
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

        // make sure blockvisord is running
        test_env::bv_run(&["start"], "", None);

        let mut cmd = Command::cargo_bin("bvup").unwrap();
        cmd.args([provision_token, "--skip-download"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("Can't provision and init blockvisor configuration, while it is running, `bv stop` first."));

        test_env::bv_run(&["stop"], "", None);
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
            .args(["--yes", "--use-host-network"])
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

    rbac::setup_rbac(db_url).await;
    println!("create user");
    let user_query = r#"INSERT INTO users
        VALUES ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', 'tester@blockjoy.com', '57snVgOUjwtfOrMxLHez8KOQaTNaNnLXMkUpzaxoRDs', 'cM4OaOTJUottdF4i8unbuA', '2023-01-17 22:13:52.422342+00', 'Luuk', 'Wester', '2023-01-17 22:14:06.297602+00', NULL, NULL);
        "#;
    execute_sql_insert(db_url, user_query);

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
        INSERT INTO tokens (token_type, token, created_by_resource, created_by, org_id, created_at) VALUES ('host_provision', 'rgfr4YJZ8dIA', 'user', '1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', now());
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'org-admin');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'org-member');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'api-key-host');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'api-key-node');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'grpc-login');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'grpc-new-host');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'view-developer-preview');
        "#;
    execute_sql_insert(db_url, org_query);

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

    const OLD_IMAGE_VERSION: &str = "0.0.2";
    const OLD_IMAGE: &str = "testing/validator/0.0.2";
    // const NEW_IMAGE_VERSION: &str = "0.0.3";
    println!("add protocol");
    let protocol_query = r#"INSERT INTO protocols (id, name, display_name, visibility, ticker) values ('ab5d8cfc-77b1-4265-9fee-ba71ba9de092', 'Testing', 'Testing', 'public', 'TEST');
        INSERT INTO protocol_node_types (id, protocol_id, node_type, visibility) VALUES ('206fae73-0ea5-4b3c-9b76-f8ea2b9b5f45','ab5d8cfc-77b1-4265-9fee-ba71ba9de092', 'validator', 'public');
        INSERT INTO protocol_versions (id, protocol_id, protocol_node_type_id, version) VALUES ('78d4c409-401d-491f-8c87-df7f35971bb7','ab5d8cfc-77b1-4265-9fee-ba71ba9de092', '206fae73-0ea5-4b3c-9b76-f8ea2b9b5f45', '0.0.2');
        INSERT INTO protocol_properties VALUES ('5972a35a-333c-421f-ab64-a77f4ae17533', 'ab5d8cfc-77b1-4265-9fee-ba71ba9de092', 'keystore-file', NULL, 'file_upload', FALSE, FALSE, '206fae73-0ea5-4b3c-9b76-f8ea2b9b5f45', '78d4c409-401d-491f-8c87-df7f35971bb7', 'Wow nice property');
        INSERT INTO protocol_properties VALUES ('a989ad08-b455-4a57-9fe0-696405947e48', 'ab5d8cfc-77b1-4265-9fee-ba71ba9de092', 'TESTING_PARAM', NULL, 'text',        FALSE, FALSE, '206fae73-0ea5-4b3c-9b76-f8ea2b9b5f45', '78d4c409-401d-491f-8c87-df7f35971bb7', 'Wow nice property');
        "#;
    execute_sql_insert(db_url, protocol_query);

    println!("stop blockvisor");
    test_env::bv_run(&["stop"], "blockvisor service stopped successfully", None);

    println!("bvup");
    let url = "http://localhost:8080";
    let mqtt = "mqtt://localhost:1883";
    Command::cargo_bin("bvup")
        .unwrap()
        .args(["--ip-range-from", "10.0.2.32", "--ip-range-to", "10.0.2.33"]) // this region will be auto-created in API
        .args([&provision_token, "--skip-download"])
        .args(["--region", "EU1"]) // this region will be auto-created in API
        .args(["--api", url])
        .args(["--mqtt", mqtt])
        .args(["--yes", "--use-host-network"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Provision and init blockvisor configuration",
        ));

    println!("read host id");
    let config_path = "/etc/blockvisor.json";
    let config = fs::read_to_string(config_path).unwrap();
    let mut config: Config = serde_json::from_str(&config).unwrap();
    let host_id = config.id.clone();
    config.apptainer = ApptainerConfig {
        host_network: true,
        extra_args: Some(vec!["--dns".to_owned(), "1.1.1.1,8.8.8.8".to_owned()]),
        ..Default::default()
    };
    let config = serde_json::to_string(&config).unwrap();
    tokio::fs::write(config_path, config).await.unwrap();

    println!("got host id: {host_id}");

    println!("start blockvisor");
    test_env::bv_run(&["start"], "blockvisor service started successfully", None);

    println!("test host info update");
    test_env::bv_run(&["host", "update"], "Host info update sent", None);

    println!("test chain list query");
    test_env::bv_run(
        &["chain", "list", "testing", "validator"],
        OLD_IMAGE_VERSION,
        None,
    );

    let stdout = bv_run(&[
        "node",
        "create",
        OLD_IMAGE,
        "--props",
        r#"{"TESTING_PARAM":"I guess just some test value"}"#,
        "--network",
        "test",
    ]);
    println!("created first node: {stdout}");
    let not_updated_node_id = parse_out_node_id(OLD_IMAGE, stdout);
    let self_update_query = r#"UPDATE nodes SET self_update = false;"#;
    execute_sql(db_url, self_update_query, "UPDATE 1");

    let stdout = bv_run(&[
        "node",
        "create",
        OLD_IMAGE,
        "--props",
        r#"{"TESTING_PARAM":"I guess just some test value"}"#,
        "--network",
        "test",
    ]);
    println!("created second node: {stdout}");
    let auto_updated_node_id = parse_out_node_id(OLD_IMAGE, stdout);

    println!("list created node, should be auto-started");
    test_env::wait_for_node_status(
        &not_updated_node_id,
        "Running",
        Duration::from_secs(300),
        None,
    )
    .await;
    test_env::wait_for_node_status(
        &auto_updated_node_id,
        "Running",
        Duration::from_secs(300),
        None,
    )
    .await;

    // println!("give user 'blockjoy-admin' so ity can add new protocol version");
    // let org_query = r#"INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'blockjoy-admin');"#;
    // println!("add new image version {NEW_IMAGE_VERSION} - trigger auto upgrade");
    // execute_sql_insert(db_url, org_query);
    // client
    //     .add_version(with_auth(
    //         pb::protocolServiceAddVersionRequest {
    //             protocol_id: protocol.id.clone(),
    //             version: NEW_IMAGE_VERSION.to_string(),
    //             description: None,
    //             node_type: common::NodeType::Validator.into(),
    //             properties: vec![],
    //         },
    //         &auth_token,
    //     ))
    //     .await
    //     .unwrap();

    // Note(luuk): nodes no longer auto upgrade by default when a new version
    // is created. One day we will add a endpoint to do this and we can
    // reintroduce this test.
    // println!("list node, should be auto-upgraded");
    // let start = std::time::Instant::now();
    // while node_version(&auto_updated_node_id).await != NEW_IMAGE_VERSION {
    //     if start.elapsed() < Duration::from_secs(300) {
    //         sleep(Duration::from_secs(1)).await;
    //     } else {
    //         panic!("timeout expired")
    //     }
    // }

    // TODO uncomment when API part is ready
    // test_env::bv_run(
    //     &["node", "run", "config_check", &auto_updated_node_id],
    //     "ok",
    //     None,
    // );

    test_env::bv_run(
        &["node", "run", "file_access_check", &auto_updated_node_id],
        "ok",
        None,
    );

    check_upload_and_download(&auto_updated_node_id);

    assert_eq!(OLD_IMAGE_VERSION, node_version(&not_updated_node_id).await);

    let stdout = bv_run(&["node", "stop", &auto_updated_node_id]);
    println!("executed stop node command: {stdout:?}");

    println!("get node status");
    test_env::wait_for_node_status(
        &auto_updated_node_id,
        "Stopped",
        Duration::from_secs(60),
        None,
    )
    .await;

    bv_run(&["node", "delete", "--yes", &auto_updated_node_id]);

    println!("check if node is deleted");
    let is_deleted = || {
        let stdout = bv_run(&["node", "list"]);
        !stdout.contains(&auto_updated_node_id)
    };
    let start = std::time::Instant::now();
    while !is_deleted() {
        if start.elapsed() < Duration::from_secs(60) {
            sleep(Duration::from_secs(1)).await;
        } else {
            panic!("timeout expired")
        }
    }
}

async fn node_version(id: &str) -> String {
    if let Ok(node_state) = NodeState::load(Path::new(&format!(
        "/var/lib/blockvisor/nodes/{id}/state.json"
    )))
    .await
    {
        node_state.image.version
    } else {
        Default::default()
    }
}

fn parse_out_node_id(image: &str, std_out: String) -> String {
    std_out
        .trim_start_matches(&format!("Created new node from `{image}` image with ID "))
        .split('`')
        .nth(1)
        .unwrap()
        .to_string()
}

fn check_upload_and_download(node_id: &str) {
    println!("cleanup previous download if any");
    sh_inside(
        node_id,
        "rm -rf /blockjoy/.babel_jobs; rm -rf /blockjoy/protocol_data/*",
    );
    println!("create dummy protocol data");
    sh_inside(node_id,"mkdir -p /blockjoy/protocol_data/sub /blockjoy/protocol_data/some_subdir && touch /blockjoy/protocol_data/.gitignore /blockjoy/protocol_data/some_subdir/something_to_ignore.txt /blockjoy/protocol_data/empty_file");
    sh_inside(node_id,"head -c 43210 < /dev/urandom > /blockjoy/protocol_data/file_a && head -c 654321 < /dev/urandom > /blockjoy/protocol_data/file_b && head -c 432 < /dev/urandom > /blockjoy/protocol_data/file_c && head -c 257 < /dev/urandom > /blockjoy/protocol_data/sub/file_d && head -c 128 < /dev/urandom > /blockjoy/protocol_data/sub/file_e && head -c 43210 < /dev/urandom > /blockjoy/protocol_data/some_subdir/any.bak");
    let sha_a = sh_inside(node_id, "sha1sum /blockjoy/protocol_data/file_a");
    let sha_b = sh_inside(node_id, "sha1sum /blockjoy/protocol_data/file_b");
    let sha_c = sh_inside(node_id, "sha1sum /blockjoy/protocol_data/file_c");
    let sha_d = sh_inside(node_id, "sha1sum /blockjoy/protocol_data/sub/file_d");
    let sha_e = sh_inside(node_id, "sha1sum /blockjoy/protocol_data/sub/file_e");

    println!("start upload job");
    test_env::bv_run(&["node", "run", "upload", node_id], "", None);

    println!("wait for upload finished");
    let start = std::time::Instant::now();
    while let Err(err) = test_env::try_bv_run(
        &["node", "job", node_id, "info", "upload"],
        "status:           Finished with exit code 0\nprogress:         100.00% (9/9 chunks)\nrestart_count:    0\nupgrade_blocking: true\nlogs:             <empty>",
        None,
    ) {
        if start.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_secs(1));
        } else {
            panic!("timeout expired: {err:#}")
        }
    }

    println!("cleanup protocol data");
    sh_inside(
        node_id,
        "rm -rf /blockjoy/protocol_data/* /blockjoy/protocol_data/.gitignore",
    );

    println!("start download job");
    test_env::bv_run(
        &["node", "run", "download", node_id],
        "Download started!",
        None,
    );

    println!("wait for download finished");
    let start = std::time::Instant::now();
    while let Err(err) = test_env::try_bv_run(
        &["node", "job", node_id, "info", "download"],
        "status:           Finished with exit code 0\nprogress:         100.00% (9/9 chunks)\nrestart_count:    0\nupgrade_blocking: true\nlogs:             <empty>",
        None,
    ) {
        if start.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_secs(1));
        } else {
            panic!("timeout expired: {err:#}")
        }
    }

    println!("check download progress");
    test_env::bv_run(
        &["node", "job", node_id, "info", "download"],
        "progress:         100.00% (9/9 chunks)",
        None,
    );

    println!("verify downloaded data");
    assert_eq!(
        sha_a.trim(),
        sh_inside(node_id, "sha1sum /blockjoy/protocol_data/file_a").trim()
    );
    assert_eq!(
        sha_b.trim(),
        sh_inside(node_id, "sha1sum /blockjoy/protocol_data/file_b").trim()
    );
    assert_eq!(
        sha_c.trim(),
        sh_inside(node_id, "sha1sum /blockjoy/protocol_data/file_c").trim()
    );
    assert_eq!(
        sha_d.trim(),
        sh_inside(node_id, "sha1sum /blockjoy/protocol_data/sub/file_d").trim()
    );
    assert_eq!(
        sha_e.trim(),
        sh_inside(node_id, "sha1sum /blockjoy/protocol_data/sub/file_e").trim()
    );
    sh_inside(
        node_id,
        "if [ -f /blockjoy/protocol_data/.gitignore ]; then exit 1; fi",
    );
    sh_inside(
        node_id,
        "if [ -f /blockjoy/protocol_data/some_subdir/something_to_ignore.txt ]; then exit 1; fi",
    );
    sh_inside(
        node_id,
        "if [ ! -f /blockjoy/protocol_data/empty_file ]; then exit 1; fi",
    );
}

fn sh_inside(node_id: &str, sh_script: &str) -> String {
    String::from_utf8(
        Command::cargo_bin("bv")
            .unwrap()
            .args([
                "node",
                "run",
                &format!("--param={sh_script}"),
                "sh_inside",
                node_id,
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .to_owned(),
    )
    .unwrap()
}

fn bv_run(commands: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("bv").unwrap();
    let cmd = cmd.args(commands).env("NO_COLOR", "1");
    let output = cmd.output().unwrap();
    str::from_utf8(&[output.stdout, output.stderr].concat())
        .unwrap()
        .to_string()
}
