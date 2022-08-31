#[cfg(target_os = "linux")]
use assert_cmd::Command;
#[cfg(target_os = "linux")]
use assert_fs::TempDir;
#[cfg(target_os = "linux")]
use blockvisord::grpc::pb;
#[cfg(target_os = "linux")]
use futures_util::FutureExt;
#[cfg(target_os = "linux")]
use predicates::prelude::*;
#[cfg(target_os = "linux")]
use serde_json::{json, Value};
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
use tonic::transport::Server;

#[cfg(target_os = "linux")]
mod stub_server;

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
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("stop")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "blockvisor service stopped successfully",
        ));

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "blockvisor service started successfully",
        ));

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .assert()
        .success()
        .stdout(predicate::str::contains("Service already running"));
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_node_lifecycle() {
    use std::str;
    use uuid::Uuid;

    let chain_id = Uuid::new_v4().to_string();

    println!("create a node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    let cmd = cmd.args(&["node", "create", &chain_id]);
    let output = cmd.output().unwrap();
    let stdout = str::from_utf8(&output.stdout).unwrap();
    let stderr = str::from_utf8(&output.stderr).unwrap();
    println!("create stdout: {stdout}");
    println!("create stderr: {stderr}");
    let vm_id = stdout
        .trim_start_matches(&format!("Created new node for `{chain_id}` chain with ID "))
        .split('`')
        .nth(1)
        .unwrap();
    println!("create vm_id: {vm_id}");

    println!("stop stopped node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "stop", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stopped node"));

    println!("start stopped node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "start", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Started node"));

    println!("stop started node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "stop", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stopped node"));

    println!("restart stopped node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "start", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Started node"));

    println!("delete started node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "delete", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted node"));
}

#[test]
#[ignore] // FIXME: switch from rest API client to gRPC one
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_init_unknown_otp() {
    let tmp_dir = TempDir::new().unwrap();

    let otp = "UNKNOWN";
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "https://api.blockvisor.dev";

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
#[ignore] // FIXME: wait for updated stakejoy api image
#[serial]
#[cfg(target_os = "linux")]
async fn test_bv_cmd_init_localhost() {
    let timeout = Duration::from_secs(2);
    let client = reqwest::Client::builder().timeout(timeout).build().unwrap();

    println!("create user");
    let create_user = json!({
        "email": "user1@example.com",
        "password": "user1pass",
        "password_confirm": "user1pass",
    });
    client
        .post("http://localhost:8080/users")
        .header("Content-Type", "application/json")
        .json(&create_user)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    println!("make admin");
    let db_url = "postgres://blockvisor:password@database:5432/blockvisor_db";
    let db_query = r#"update users set role='admin' where email='user1@example.com'"#;

    Command::new("docker")
        .args(&[
            "compose", "run", "-it", "database", "psql", db_url, "-c", db_query,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("UPDATE 1"));

    println!("login user");
    let login_user = json!({
        "email": "user1@example.com",
        "password": "user1pass",
    });
    let text = client
        .post("http://localhost:8080/login")
        .header("Content-Type", "application/json")
        .json(&login_user)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let login: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(login.get("role").unwrap(), "admin");

    println!("get user organization id");
    let user_id = login.get("id").unwrap().as_str().unwrap();
    let token = login.get("token").unwrap().as_str().unwrap();

    let text = client
        .get(format!("http://localhost:8080/users/{}/orgs", user_id))
        .header("Content-Type", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let orgs: Value = serde_json::from_str(&text).unwrap();
    let org = orgs
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap();
    let org_id = org.get("id").unwrap().as_str().unwrap();

    println!("get blockchain id");
    let text = client
        .get("http://localhost:8080/blockchains")
        .header("Content-Type", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let blockchains: Value = serde_json::from_str(&text).unwrap();
    let blockchain = blockchains
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap();
    let blockchain_id = blockchain.get("id").unwrap().as_str().unwrap();

    println!("create host provision");
    let host_provision = json!({
        "org_id": org_id,
        "nodes": [
            {"blockchain_id": blockchain_id, "node_type": "validator"}
        ],
    });
    let text = client
        .post("http://localhost:8080/host_provisions")
        .header("Content-Type", "application/json")
        .bearer_auth(token)
        .json(&host_provision)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let provision: Value = serde_json::from_str(&text).unwrap();
    let otp = provision.get("id").unwrap().as_str().unwrap();

    println!("bv init");
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";

    Command::cargo_bin("bv")
        .unwrap()
        .args(&["init", otp])
        .args(&["--ifa", ifa])
        .args(&["--url", url])
        .assert()
        .success()
        .stdout(predicate::str::contains("Configuring blockvisor"));
}

#[tokio::test]
#[serial]
#[cfg(target_os = "linux")]
async fn test_bv_cmd_grpc_commands() {
    use stub_server::StubServer;
    use uuid::Uuid;

    let node_name = "beautiful-node-name".to_string();
    let node_id = Uuid::new_v4().to_string();
    let id = pb::Uuid {
        value: node_id.clone(),
    };

    println!("delete existing node, if any");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "delete", &node_name]).assert();

    println!("preparing server");
    let commands = vec![
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: Some(id.clone()),
                meta: None,
                command: Some(pb::node_command::Command::Create(pb::NodeCreate {
                    name: node_name,
                    image: Some(pb::ContainerImage {
                        url: "helium/node/latest".to_string(),
                    }),
                    blockchain: "helium".to_string(),
                    r#type: pb::NodeType::Node.into(),
                })),
            })),
        },
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: Some(id.clone()),
                meta: None,
                command: Some(pb::node_command::Command::Start(pb::NodeStart {})),
            })),
        },
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: Some(id.clone()),
                meta: None,
                command: Some(pb::node_command::Command::Stop(pb::NodeStop {})),
            })),
        },
        pb::Command {
            r#type: Some(pb::command::Type::Node(pb::NodeCommand {
                id: Some(id.clone()),
                meta: None,
                command: Some(pb::node_command::Command::Restart(pb::NodeRestart {})),
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
                "0.0.0.0:8080".to_socket_addrs().unwrap().next().unwrap(),
                shutdown_rx.recv().map(drop),
            )
            .await
            .unwrap()
    };

    println!("run server");
    tokio::select! {
        _ = server_future => {},
        _ = sleep(Duration::from_secs(60)) => {},
    };

    println!("list created node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "list", "--all"])
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains(&node_id));

    println!("delete created node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "delete", &node_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted node"));

    println!("check received updates");
    let updates: Vec<_> = ReceiverStream::new(updates_rx)
        .timeout(Duration::from_secs(1))
        .take_while(Result::is_ok)
        .collect()
        .await;

    println!("got updates: {updates:?}");
    let expected_statuses = vec![
        pb::node_info::ContainerStatus::Creating,
        pb::node_info::ContainerStatus::Stopped,
        pb::node_info::ContainerStatus::Starting,
        pb::node_info::ContainerStatus::Running,
        pb::node_info::ContainerStatus::Stopping,
        pb::node_info::ContainerStatus::Stopped,
        pb::node_info::ContainerStatus::Stopping,
        pb::node_info::ContainerStatus::Stopped,
        pb::node_info::ContainerStatus::Starting,
        pb::node_info::ContainerStatus::Running,
        pb::node_info::ContainerStatus::Deleting,
        pb::node_info::ContainerStatus::Deleted,
    ];
    for (id, status) in expected_statuses.into_iter().enumerate() {
        assert_eq!(
            updates.get(id).unwrap().as_ref().unwrap(),
            &default_node_update_with_status(&node_id, status)
        );
    }

    assert_eq!(updates.len(), 12);
}

#[cfg(target_os = "linux")]
fn default_node_update_with_status(
    node_id: &str,
    status: pb::node_info::ContainerStatus,
) -> pb::InfoUpdate {
    pb::InfoUpdate {
        info: Some(pb::info_update::Info::Node(pb::NodeInfo {
            id: Some(pb::Uuid {
                value: node_id.to_string(),
            }),
            container_status: Some(status.into()),
            ..Default::default()
        })),
    }
}
