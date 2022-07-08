#[cfg(target_os = "linux")]
use assert_cmd::Command;
#[cfg(target_os = "linux")]
use assert_fs::TempDir;
#[cfg(target_os = "linux")]
use predicates::prelude::*;
#[cfg(target_os = "linux")]
use serde_json::{json, Value};
#[cfg(target_os = "linux")]
use serial_test::serial;
#[cfg(target_os = "linux")]
use tokio::time::Duration;

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
    // FIXME: investigate why test is not stable without sleeps
    use std::{thread::sleep, time::Duration};

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("stop")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "blockvisor service stopped successfully",
        ));
    sleep(Duration::from_secs(1));

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "blockvisor service started successfully",
        ));
    sleep(Duration::from_secs(1));
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn test_bv_cmd_node_lifecycle() {
    use std::str;
    // FIXME: investigate why test is not stable without sleeps
    use std::{thread::sleep, time::Duration};
    use uuid::Uuid;

    let chain_id = Uuid::new_v4().to_string();

    println!("start service");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "blockvisor service started successfully",
        ));
    sleep(Duration::from_secs(1));

    println!("create a node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    let cmd = cmd.args(&["node", "create", &chain_id]);
    let output = cmd.output().unwrap();
    let stdout = str::from_utf8(&output.stdout).unwrap();
    println!("create output: {stdout}");
    let vm_id = stdout
        .trim_start_matches(&format!("Created new node for `{chain_id}` chain with ID "))
        .split('`')
        .nth(1)
        .unwrap();
    println!("create vm_id: {vm_id}");
    sleep(Duration::from_secs(1));

    println!("stop stopped node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "stop", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stopped node"));
    sleep(Duration::from_secs(1));

    println!("start stopped node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "start", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Started node"));
    sleep(Duration::from_secs(1));

    println!("stop started node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "stop", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stopped node"));
    sleep(Duration::from_secs(1));

    println!("restart stopped node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "start", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Started node"));
    sleep(Duration::from_secs(1));

    println!("delete started node");
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(&["node", "delete", vm_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted node"));
}

#[test]
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
