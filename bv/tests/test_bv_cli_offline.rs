pub mod test_env;

use crate::test_env::test_env::TestEnv;
use anyhow::Result;
use assert_cmd::Command;
use assert_fs::TempDir;
use bv_utils::run_flag::RunFlag;
use std::time::Duration;
use sysinfo::{Pid, PidExt, ProcessExt, ProcessRefreshKind, System, SystemExt};
use tokio::time::sleep;

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

#[tokio::test]
async fn test_bv_host_metrics() -> Result<()> {
    let test_env = TestEnv::new().await?;
    test_env.bv_run(&["host", "metrics"], "Used cpu:");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 7)]
async fn test_bv_cmd_delete_all() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    test_env.bv_run(&["node", "rm", "--all", "--yes"], "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 7)]
async fn test_bv_cmd_node_start_and_stop_all() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    const NODES_COUNT: usize = 2;
    println!("create {NODES_COUNT} nodes");
    let mut nodes: Vec<String> = Default::default();
    for _ in 0..NODES_COUNT {
        nodes.push(test_env.create_node("testing/validator/0.0.1"));
    }

    println!("start all created nodes");
    test_env.bv_run(&["node", "start"], "Started node");
    println!("check all nodes are running");
    for id in &nodes {
        test_env.bv_run(&["node", "status", id], "Running");
    }
    println!("stop all nodes");
    test_env.bv_run(&["node", "stop"], "Stopped node");
    println!("check all nodes are stopped");
    for id in &nodes {
        test_env.bv_run(&["node", "status", id], "Stopped");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 7)]
async fn test_bv_cmd_logs() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    println!("create a node");
    let vm_id = &test_env.create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("start node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("get logs");
    test_env.bv_run(
        &["node", "logs", vm_id],
        "Testing entry_point not configured, but parametrized with anything!",
    );

    println!("stop started node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 7)]
async fn test_bv_cmd_node_lifecycle() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    let mut run = RunFlag::default();
    let bv_handle = test_env.run_blockvisord(run.clone()).await?;
    println!("create a node");
    let vm_id = &test_env.create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("stop stopped node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("stop started node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("restart stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("query metrics");
    test_env.bv_run(&["node", "metrics", vm_id], "In consensus:        false");

    println!("list running node before service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("stop service");
    run.stop();
    bv_handle.await.ok();

    println!("start service again");
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("list running node after service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("upgrade running node");
    test_env.bv_run(
        &["node", "upgrade", vm_id, "testing/validator/0.0.2"],
        "Upgraded node",
    );

    println!("list running node after node upgrade");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("generate node keys");
    test_env.bv_run(&["node", "run", vm_id, "generate_keys"], "");

    println!("check node keys");
    test_env.bv_run(&["node", "keys", vm_id], "first");

    println!("delete started node");
    test_env.bv_run(&["node", "delete", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 7)]
async fn test_bv_cmd_node_recovery() -> Result<()> {
    use blockvisord::{node::FC_BIN_NAME, utils};

    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("create a node");
    let vm_id = &test_env.create_node("testing/validator/0.0.1");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    test_env.bv_run(&["node", "status", vm_id], "Running");

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
    test_env.bv_run(&["node", "status", vm_id], "Failed");

    println!("list running node after recovery");
    let start = std::time::Instant::now();
    let elapsed = || std::time::Instant::now() - start;
    while !test_env.try_bv_run(&["node", "status", vm_id], "Running")
        && elapsed() < Duration::from_secs(60)
    {
        sleep(Duration::from_secs(1)).await;
    }

    println!("delete started node");
    test_env.bv_run(&["node", "delete", vm_id], "Deleted node");
    Ok(())
}
