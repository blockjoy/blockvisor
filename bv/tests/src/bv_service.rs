use crate::src::utils::{
    execute_sql_insert, rbac,
    stub_server::StubHostsServer,
    test_env::{self, link_apptainer_config},
};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::linux_platform::bv_root;
use blockvisord::{
    bv_config::Config,
    node_state::NodeState,
    services::api::{common, pb},
};
use predicates::prelude::*;
use std::path::PathBuf;
use std::{net::ToSocketAddrs, path::Path, str};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bvup() {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
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
        VALUES ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', 'tester@blockjoy.com', '57snVgOUjwtfOrMxLHez8KOQaTNaNnLXMkUpzaxoRDs', 'cM4OaOTJUottdF4i8unbuA', '2023-01-17 22:13:52.422342+00', 'Luuk', 'Wester', '2023-01-17 22:14:06.297602+00');
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
        INSERT INTO tokens (token_type, token, created_by_type, created_by_id, org_id, created_at) VALUES ('host_provision', 'rgfr4YJZ8dIA', 'user', '1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', now());
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'org-personal');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'grpc-login');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'grpc-new-host');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'view-developer-preview');
        INSERT INTO user_roles (user_id, org_id, role) values ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', '53b28794-fb68-4cd1-8165-b98a51a19c46', 'blockjoy-admin');
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

    let mut client = pb::api_key_service_client::ApiKeyServiceClient::connect(url)
        .await
        .unwrap();
    let response = client
        .create(with_auth(
            pb::ApiKeyServiceCreateRequest {
                label: "token".to_string(),
                resource: Some(common::Resource {
                    resource_type: common::ResourceType::User.into(),
                    resource_id: user_id.to_string(),
                }),
            },
            &auth_token,
        ))
        .await
        .unwrap()
        .into_inner();
    let api_key = response.api_key.unwrap();
    println!("nib api_key: {api_key}");

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
    let config = Config::load(&bv_root()).await.unwrap();
    let host_id = config.id.clone();
    println!("got host id: {host_id}");

    test_env::nib_run(&["config", &api_key, "--api", url], "", None);

    println!("start blockvisor");
    test_env::bv_run(&["start"], "blockvisor service started successfully", None);

    println!("test host info update");
    test_env::bv_run(&["host", "update"], "Host info update sent", None);

    let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    println!("push testing protocol");
    test_env::nib_run(
        &[
            "protocol",
            "push",
            "--path",
            &test_dir.join("protocols.yaml").to_string_lossy(),
        ],
        "Protocol 'testing' added",
        None,
    );

    println!("check test image");
    test_env::nib_run(
        &[
            "image",
            "check",
            "--props",
            r#"{"TESTING_PARAM":"testing value"}"#,
            "--path",
            &test_dir
                .join("image_v1")
                .join("babel.yaml")
                .to_string_lossy(),
        ],
        "Plugin linter: Ok(())",
        None,
    );

    println!("push test image v1");
    test_env::nib_run(
        &[
            "image",
            "push",
            "--path",
            &test_dir
                .join("image_v1")
                .join("babel.yaml")
                .to_string_lossy(),
        ],
        "Image 'testing/test/0.0.1/1' added",
        None,
    );

    println!("test chain list query");
    test_env::bv_run(
        &["protocol", "list", "--name", "Testing protocol"],
        "* test/0.0.1",
        None,
    );

    let stdout = bv_run(&[
        "node",
        "create",
        "testing",
        "test",
        "--props",
        r#"{"TESTING_PARAM":"I guess just some test value"}"#,
    ]);
    let first_node_id = parse_out_node_id(stdout);
    println!("created first node: {first_node_id}");

    let stdout = bv_run(&[
        "node",
        "create",
        "testing",
        "test",
        "--props",
        r#"{"TESTING_PARAM":"I guess just some test value"}"#,
    ]);
    let second_node_id = parse_out_node_id(stdout);
    println!("created second node: {second_node_id}");

    println!("list created node, should be auto-started");
    test_env::wait_for_node_status(&first_node_id, "Running", Duration::from_secs(300), None).await;
    test_env::wait_for_node_status(&second_node_id, "Running", Duration::from_secs(300), None)
        .await;

    let start = std::time::Instant::now();
    while let Err(err) = test_env::try_bv_run(
        &["node", "job", &second_node_id, "info", "init_job"],
        "status:           Finished with exit code 0",
        None,
    ) {
        if start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_secs(1));
        } else {
            panic!("timeout expired: {err:#}")
        }
    }

    println!("push test image v2");
    test_env::nib_run(
        &[
            "image",
            "push",
            "--path",
            &test_dir
                .join("image_v2")
                .join("babel.yaml")
                .to_string_lossy(),
        ],
        "Image 'testing/test/0.0.2/1' added",
        None,
    );

    println!("trigger second node upgrade");
    bv_run(&["node", "upgrade", &second_node_id]);

    println!("wait for second node to be upgraded");
    let start = std::time::Instant::now();
    while node_version(&second_node_id).await != "0.0.2" {
        if start.elapsed() < Duration::from_secs(300) {
            sleep(Duration::from_secs(1)).await;
        } else {
            panic!("timeout expired")
        }
    }

    println!("check crypt service");
    test_env::bv_run(
        &["node", "run", "secret_check", &second_node_id],
        "ok",
        None,
    );

    println!("check file system access");
    test_env::bv_run(
        &["node", "run", "file_access_check", &first_node_id],
        "ok",
        None,
    );

    check_upload_and_download(&first_node_id);

    assert_eq!("0.0.1", node_version(&first_node_id).await);

    let stdout = bv_run(&["node", "stop", &first_node_id]);
    println!("executed stop node command: {stdout:?}");

    println!("get node status");
    test_env::wait_for_node_status(&first_node_id, "Stopped", Duration::from_secs(60), None).await;

    bv_run(&["node", "delete", "--yes", &first_node_id]);

    println!("check if node is deleted");
    let is_deleted = || {
        let stdout = bv_run(&["node", "list"]);
        !stdout.contains(&first_node_id)
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

fn parse_out_node_id(std_out: String) -> String {
    std_out
        .trim_start_matches("Created new node with ID `")
        .split('`')
        .next()
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
