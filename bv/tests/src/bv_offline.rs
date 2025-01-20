use crate::src::utils::{
    stub_server::StubDiscoveryService, test_env::TestEnv, token::TokenGenerator,
};
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::api_config::ApiConfig;
use blockvisord::{
    apptainer_machine::build_rootfs_dir,
    bv_config::{Config, SharedConfig},
    node_context::build_node_dir,
    services,
    services::api::pb,
    utils,
};
use bv_utils::{cmd::run_cmd, run_flag::RunFlag, system::is_process_running};
use eyre::{bail, Result};
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use tokio::{
    fs,
    time::{sleep, Duration},
};
use tonic::transport::Server;
use uuid::Uuid;

#[test]
fn test_bv_cli_start_without_init() {
    let tmp_dir = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Error: Host is not registered, please run `bvup` first",
        ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_host_metrics_and_info() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    test_env.bv_run(&["host", "metrics"], "Used cpu:");
    test_env.bv_run(&["host", "info"], "Hostname:");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_delete_all() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    test_env.bv_run(&["node", "rm", "--all", "--yes"], "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_start_and_stop_all() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    const NODES_COUNT: usize = 2;
    println!("create {NODES_COUNT} nodes");
    let mut nodes: Vec<(String, String)> = Default::default();
    for i in 0..NODES_COUNT {
        nodes.push(test_env.create_node("image_v1", &format!("216.18.214.{i}")));
    }

    println!("start all created nodes");
    test_env.bv_run(&["node", "start"], "Started node");
    println!("check all nodes are running");
    for (vm_id, _) in &nodes {
        test_env.bv_run(&["node", "status", vm_id], "Running");
    }
    let (node_id, _) = nodes.first().unwrap();
    let node_dir = build_node_dir(&test_env.bv_root, Uuid::parse_str(node_id).unwrap());
    assert!(!test_env
        .sh_inside(node_id, "env")
        .trim()
        .contains("BV_ROOT"));
    assert_eq!("ok", test_env.sh_inside(node_id, "cat /root/test").trim());
    assert_eq!("ok", test_env.sh_inside(node_id, "cat /tmp/test").trim());
    fs::write(node_dir.join("data/test"), "ok").await.unwrap();
    assert_eq!(
        "ok",
        test_env.sh_inside(node_id, "cat /blockjoy/test").trim()
    );
    assert_eq!(
        "memory.limit = 1000000000",
        fs::read_to_string(node_dir.join("cgroups.toml"))
            .await
            .unwrap()
            .trim()
    );
    let pid = fs::read_to_string(node_dir.join("apptainer.pid"))
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(is_process_running(pid));
    println!("stop all nodes");
    test_env.bv_run(&["node", "stop"], "Stopped node");
    println!("check all nodes are stopped");
    for (vm_id, _) in &nodes {
        test_env.bv_run(&["node", "status", vm_id], "Stopped");
    }
    assert!(!is_process_running(pid));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_jobs() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;
    println!("create a node");
    let (vm_id, _) = &test_env.create_node("image_v1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("start node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("check jobs");
    test_env.bv_run(&["node", "job", vm_id, "ls"], "init_job");

    println!("stop job");
    test_env.bv_run(&["node", "job", vm_id, "stop", "init_job"], "");

    println!("job info");
    test_env.bv_run(
        &["node", "job", vm_id, "info", "init_job"],
        "status:           Stopped",
    );

    println!("start job");
    test_env.bv_run(&["node", "job", vm_id, "start", "init_job"], "");

    println!("wait for init_job finished");
    let start = std::time::Instant::now();
    while let Err(err) = test_env.try_bv_run(
        &["node", "job", vm_id, "info", "init_job"],
        "status:           Finished with exit code 0",
    ) {
        if start.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_secs(1));
        } else {
            panic!("timeout expired: {err:#}")
        }
    }

    assert!(fs::read_to_string(
        build_rootfs_dir(&build_node_dir(&test_env.bv_root, Uuid::parse_str(vm_id)?))
            .join("var/lib/babel/jobs/init_job/logs"),
    )
    .await?
    .contains("dummy_init"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_lifecycle() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    let mut run = RunFlag::default();
    let bv_handle = test_env.run_blockvisord(run.clone()).await?;
    println!("create a node");
    let (vm_id, vm_name) = &test_env.create_node("image_v1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("stop stopped node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("stop started node");
    test_env.bv_run(&["node", "stop", vm_id], "Stopped node");

    println!("restart stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");
    test_env
        .wait_for_job_status(vm_id, "echo", "Running", Duration::from_secs(5))
        .await;

    println!("check node");
    test_env.bv_run(&["node", "info", vm_id], "In consensus:   false");

    println!("list running node before service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("list running node using vm name");
    test_env.bv_run(&["node", "status", vm_name], "Running");

    println!("stop service");
    run.stop();
    bv_handle.await.ok();

    println!("start service again");
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("list running node after service restart");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("upgrade running node");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("image_v2")
        .join("babel.yaml");
    test_env.nib_run(
        &["image", "upgrade", "--path", &path.to_string_lossy(), vm_id],
        "Upgraded dev_node",
    );

    println!("list running node after node upgrade");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("check jobs after node upgrade");
    test_env
        .wait_for_job_status(vm_id, "echo2", "Running", Duration::from_secs(5))
        .await;

    println!("delete started node");
    test_env.bv_run(&["node", "delete", "--yes", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_recovery() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    test_env.run_blockvisord(RunFlag::default()).await?;

    println!("create a node");
    let (vm_id, _) = &test_env.create_node("image_v1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    let process_id = utils::get_process_pid(
        "babel",
        &blockvisord::apptainer_machine::build_rootfs_dir(
            &blockvisord::node_context::build_node_dir(
                &test_env.bv_root,
                Uuid::parse_str(vm_id).unwrap(),
            ),
        )
        .to_string_lossy(),
    )
    .unwrap();
    println!("kill babel - break node");
    run_cmd("kill", ["-9", &process_id.to_string()])
        .await
        .unwrap();
    // wait until process is actually killed
    while is_process_running(process_id) {
        sleep(Duration::from_millis(10)).await;
    }

    println!("list running node before recovery");
    test_env.bv_run(&["node", "status", vm_id], "Failed");
    test_env
        .wait_for_running_node(vm_id, Duration::from_secs(60))
        .await;

    println!("stop container - break node");
    run_cmd("apptainer", ["instance", "stop", vm_id])
        .await
        .unwrap();

    println!("list running node before recovery");
    test_env.bv_run(&["node", "status", vm_id], "Failed");
    test_env
        .wait_for_running_node(vm_id, Duration::from_secs(60))
        .await;

    println!("delete started node");
    test_env.bv_run(&["node", "delete", "--yes", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_node_recovery_fail() -> Result<()> {
    let mut test_env = TestEnv::new().await?;
    let mut pal = test_env.build_dummy_platform();
    let babel_link = test_env.bv_root.join("babel");
    fs::symlink(&pal.babel_path, &babel_link).await.unwrap();
    pal.babel_path.clone_from(&babel_link);
    test_env
        .run_blockvisord_with_pal(RunFlag::default(), pal)
        .await?;

    println!("create a node");
    let (vm_id, _) = &test_env.create_node("image_v1", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("start stopped node");
    test_env.bv_run(&["node", "start", vm_id], "Started node");

    println!("list running node");
    test_env.bv_run(&["node", "status", vm_id], "Running");

    println!("break babel - permanently break node");
    fs::remove_file(babel_link).await.unwrap();
    let vm_rootfs = blockvisord::apptainer_machine::build_rootfs_dir(
        &blockvisord::node_context::build_node_dir(
            &test_env.bv_root,
            Uuid::parse_str(vm_id).unwrap(),
        ),
    );
    let process_id = utils::get_process_pid("babel", &vm_rootfs.to_string_lossy()).unwrap();
    run_cmd("kill", ["-9", &process_id.to_string()])
        .await
        .unwrap();
    // wait until process is actually killed
    while is_process_running(process_id) {
        sleep(Duration::from_millis(10)).await;
    }

    println!("list running node before recovery");
    test_env.bv_run(&["node", "status", vm_id], "Failed");
    test_env
        .wait_for_node_fail(vm_id, Duration::from_secs(600))
        .await;

    println!("delete started node");
    test_env.bv_run(&["node", "delete", "--yes", vm_id], "Deleted node");
    Ok(())
}

#[tokio::test]
async fn test_discovery_on_connection_error() -> Result<()> {
    let discovery_service = StubDiscoveryService;
    let server_future = async {
        Server::builder()
            .max_concurrent_streams(1)
            .add_service(pb::discovery_service_server::DiscoveryServiceServer::new(
                discovery_service,
            ))
            .serve("0.0.0.0:8091".to_socket_addrs().unwrap().next().unwrap())
            .await
            .unwrap()
    };
    let id = Uuid::new_v4();
    let config = Config {
        id: id.to_string(),
        name: "host name".to_string(),
        api_config: ApiConfig {
            token: TokenGenerator::create_host(id, "1245456"),
            refresh_token: "any refresh token".to_string(),
            blockjoy_api_url: "http://localhost:8091".to_string(),
        },
        iface: "bvbr0".to_string(),
        ..Default::default()
    };
    let config = SharedConfig::new(config, "/some/dir/conf.json".into());
    let connect_future = services::connect_with_discovery(&config, |config| async {
        let config = config.read().await;
        if config.blockjoy_mqtt_url.is_none() {
            bail!("first try without urls")
        }
        Ok(config)
    });
    println!("run server");
    let final_cfg = tokio::select! {
        _ = server_future => {unreachable!()},
        res = connect_future => {res},
    }?;
    assert_eq!("notification_url", &final_cfg.blockjoy_mqtt_url.unwrap());
    Ok(())
}
