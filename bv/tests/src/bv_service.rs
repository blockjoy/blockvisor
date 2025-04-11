use crate::src::utils::{
    execute_sql_insert,
    test_env::{self, link_apptainer_config},
};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::linux_platform::bv_root;
use blockvisord::{
    bv_config::Config,
    node_state::NodeState,
    services,
    services::api::{common, pb},
};
use predicates::prelude::*;
use serial_test::serial;
use std::path::PathBuf;
use std::{path::Path, str};
use tokio::time::{sleep, Duration};

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
        VALUES ('1cff0487-412b-4ca4-a6cd-fdb9957d5d2f', 'tester@example.com', '57snVgOUjwtfOrMxLHez8KOQaTNaNnLXMkUpzaxoRDs', 'cM4OaOTJUottdF4i8unbuA', '2023-01-17 22:13:52.422342+00', 'Luuk', 'Wester', '2023-01-17 22:14:06.297602+00');
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

    let channel = tonic::transport::Endpoint::from_static(url)
        .connect()
        .await
        .unwrap();
    let mut client = pb::org_service_client::OrgServiceClient::with_interceptor(
        channel.clone(),
        services::AuthToken(auth_token.clone()),
    );

    let response = client
        .get_provision_token(get_token)
        .await
        .unwrap()
        .into_inner();

    let mut client = pb::host_service_client::HostServiceClient::with_interceptor(
        channel.clone(),
        services::AuthToken(auth_token.clone()),
    );
    client
        .create_region(pb::HostServiceCreateRegionRequest {
            region_key: "eu-1".to_string(),
            display_name: "Europe".to_string(),
            sku_code: Some("EU1".to_string()),
        })
        .await
        .unwrap();

    let tmp_dir = TempDir::new().unwrap();
    link_apptainer_config(&tmp_dir).unwrap();

    let provision_token = "NOT_FOUND";
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";

    // make sure blockvisord is not running
    test_env::bv_run(&["stop"], "", None);
    let mut cmd = Command::cargo_bin("bvup").unwrap();
    cmd.args([provision_token, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--gateway-ip", "216.18.214.193"])
        .args(["--host-ip", "216.18.214.194"])
        .args(["--use-host-network"])
        .args(["--region", "eu-1"])
        .args(["--yes"])
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid token"));

    let provision_token = response.token;
    println!("host provision token: {provision_token}");

    let mut client = pb::api_key_service_client::ApiKeyServiceClient::with_interceptor(
        channel.clone(),
        services::AuthToken(auth_token.clone()),
    );
    let response = client
        .create(pb::ApiKeyServiceCreateRequest {
            label: "token".to_string(),
            resource: Some(common::Resource {
                resource_type: common::ResourceType::User.into(),
                resource_id: user_id.to_string(),
            }),
            permissions: vec![
                "protocol-admin-update-protocol".to_string(),
                "protocol-admin-update-version".to_string(),
                "protocol-admin-add-protocol".to_string(),
                "protocol-admin-add-version".to_string(),
                "protocol-admin-view-private".to_string(),
                "protocol-view-development".to_string(),
                "protocol-view-public".to_string(),
                "protocol-get-protocol".to_string(),
                "protocol-get-latest".to_string(),
                "protocol-list-protocols".to_string(),
                "protocol-list-variants".to_string(),
                "protocol-list-versions".to_string(),
                "image-admin-get".to_string(),
                "image-admin-add".to_string(),
                "image-admin-list-archives".to_string(),
                "image-admin-update-archive".to_string(),
                "image-admin-update-image".to_string(),
            ],
        })
        .await
        .unwrap()
        .into_inner();
    let api_key = response.api_key;
    println!("nib api_key: {api_key}");

    println!("stop blockvisor");
    test_env::bv_run(&["stop"], "blockvisor service stopped successfully", None);

    println!("bvup");
    let url = "http://localhost:8080";
    Command::cargo_bin("bvup")
        .unwrap()
        .args(["--available-ips", "10.0.2.32,10.0.2.33"]) // this region will be auto-created in API
        .args([&provision_token, "--skip-download"])
        .args(["--region", "eu-1"]) // this region will be auto-created in API
        .args(["--api", url])
        .args(["--yes", "--use-host-network", "--private"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Provision and init blockvisor configuration",
        ));

    println!("read host id");
    let config = Config::load(&bv_root()).await.unwrap();
    let host_id = config.id.clone();
    println!("got host id: {host_id}");

    blockvisord::nib_config::Config {
        token: api_key,
        blockjoy_api_url: url.to_string(),
    }
    .save()
    .await
    .unwrap();

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
            "--force-cleanup",
            "--props",
            r#"{"arbitrary-text-property":"testing value"}"#,
            "--path",
            &test_dir
                .join("image_v1")
                .join("babel.yaml")
                .to_string_lossy(),
            "plugin",
            "jobs-status",
            "jobs-restarts",
            "protocol-status",
        ],
        "All checks passed!",
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
        r#"{"arbitrary-text-property":"I guess just some test value"}"#,
    ]);
    let first_node_id = parse_out_node_id(stdout);
    println!("created first node: {first_node_id}");

    let stdout = bv_run(&[
        "node",
        "create",
        "testing",
        "test",
        "--props",
        r#"{"arbitrary-text-property":"I guess just some test value"}"#,
    ]);
    let second_node_id = parse_out_node_id(stdout);
    println!("created second node: {second_node_id}");

    println!("list created node, should be auto-started");
    test_env::wait_for_node_status(&first_node_id, "Running", Duration::from_secs(300), None).await;
    test_env::wait_for_node_status(&second_node_id, "Running", Duration::from_secs(300), None)
        .await;

    test_env::wait_for_job_status(
        &second_node_id,
        "init_job",
        "Finished with exit code 0",
        Duration::from_secs(10),
        None,
    )
    .await;

    let mut client = pb::node_service_client::NodeServiceClient::with_interceptor(
        channel.clone(),
        services::AuthToken(auth_token.clone()),
    );
    client
        .update_config(pb::NodeServiceUpdateConfigRequest {
            node_id: first_node_id.clone(),
            auto_upgrade: Some(false),
            new_org_id: None,
            new_display_name: None,
            new_note: None,
            new_values: vec![],
            new_firewall: None,
            update_tags: None,
            cost: None,
        })
        .await
        .unwrap();

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

    println!("wait for second node to be auto upgraded");
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

    check_upload_and_download(&first_node_id).await;

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

async fn check_upload_and_download(node_id: &str) {
    println!("cleanup previous download if any");
    sh_inside(
        node_id,
        "rm -rf /blockjoy/.protocol_data.lock; rm -rf /blockjoy/.babel_jobs; rm -rf /blockjoy/protocol_data/*",
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
    test_env::wait_for_job_status(
        node_id,
        "upload",
        "Finished with exit code 0\nprogress:         100.00% (9/9 chunks)\nrestart_count:    0\nupgrade_blocking: true\nlogs:             <empty>",
        Duration::from_secs(120),
        None,
    ).await;

    println!("cleanup protocol data");
    sh_inside(
        node_id,
        "rm -rf /blockjoy/protocol_data/* /blockjoy/protocol_data/.gitignore /blockjoy/.protocol_data.lock",
    );

    println!("start download job");
    test_env::bv_run(
        &["node", "run", "download", node_id],
        "Download started!",
        None,
    );

    println!("wait for download finished");
    test_env::wait_for_job_status(
        node_id,
        "download",
        "Finished with exit code 0\nprogress:         100.00% (9/9 chunks)\nrestart_count:    0\nupgrade_blocking: true\nlogs:             <empty>",
        Duration::from_secs(120),
        None,
    ).await;

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
