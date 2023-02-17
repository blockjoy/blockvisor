use assert_cmd::Command;
use assert_fs::TempDir;
use predicates::prelude::predicate;
use std::time::Duration;
use sysinfo::{Pid, PidExt, ProcessExt, ProcessRefreshKind, System, SystemExt};
use tokio::time::sleep;

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

#[test]
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
fn test_bv_host_metrics() {
    bv_run(&["host", "metrics"], "Used cpu:");
}

#[test]
fn test_bv_cmd_delete_all() {
    bv_run(&["node", "rm", "--all", "--yes"], "");
}

#[test]
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
