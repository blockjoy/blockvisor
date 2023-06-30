use anyhow::Result;
use assert_cmd::Command;
use async_trait::async_trait;
use blockvisord::{
    blockvisord::BlockvisorD,
    config::{Config, SharedConfig},
    firecracker_machine,
    node::REGISTRY_CONFIG_DIR,
    node_data::{NodeData, NodeStatus},
    pal::{CommandsStream, NetInterface, Pal, ServiceConnector},
    services::cookbook::IMAGES_DIR,
    BV_VAR_PATH,
};
use bv_utils::{cmd::run_cmd, run_flag::RunFlag};
use predicates::prelude::predicate;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    str,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::{task::JoinHandle, time::sleep};
use uuid::Uuid;

/// Global integration tests token. All tests (that may run in parallel) share common FS and net
/// devices space (tap devices which are created by Firecracker during tests). Each test shall pick
/// its own token that can be then used to create unique bv_root and net device names, so tests
/// won't collide.
pub static TEST_TOKEN: AtomicU32 = AtomicU32::new(0);

/// Common staff to setup for all tests like sut (blockvisord in that case),
/// path to root dir used in test, instance of DummyPlatform.
pub struct TestEnv {
    pub bv_root: PathBuf,
    pub api_config: Config,
    pub token: u32,
}

impl TestEnv {
    pub async fn new_with_api_config(mut api_config: Config) -> Result<Self> {
        // pick unique test token
        let token = TEST_TOKEN.fetch_add(1, Ordering::Relaxed);
        // make sure temp directories names are sort - socket file path has 108 char len limit
        // see `man 7 unix` - "On  Linux, sun_path is 108 bytes in size"
        let bv_root = if let Ok(bv_temp) = std::env::var("BV_TEMP") {
            PathBuf::from(bv_temp)
        } else {
            std::env::temp_dir()
        }
        .join(token.to_string());
        let _ = fs::remove_dir_all(&bv_root); // remove remnants if any
        let vars_path = bv_root.join(BV_VAR_PATH);
        fs::create_dir_all(&vars_path)?;
        // link to pre downloaded images in /var/lib/blockvisord/images
        std::os::unix::fs::symlink(
            Path::new("/").join(BV_VAR_PATH).join(IMAGES_DIR),
            vars_path.join(IMAGES_DIR),
        )?;
        fs::create_dir_all(bv_root.join("usr"))?;
        // link to /usr/bin where firecracker and jailer is expected
        std::os::unix::fs::symlink(
            Path::new("/").join("usr").join("bin"),
            bv_root.join("usr").join("bin"),
        )?;
        api_config.blockvisor_port = 0;
        api_config.save(&bv_root).await?;
        Ok(Self {
            bv_root,
            api_config,
            token,
        })
    }

    pub async fn new() -> Result<Self> {
        let api_config = Config {
            id: "host_id".to_owned(),
            token: "token".to_owned(),
            refresh_token: "fresh boii".to_owned(),
            blockjoy_api_url: "http://localhost:8070".to_owned(),
            blockjoy_mqtt_url: Some("mqtt://localhost:1873".to_string()),
            update_check_interval_secs: None,
            blockvisor_port: 0, // 0 has special meaning - pick first free port
        };
        Self::new_with_api_config(api_config).await
    }

    pub fn build_dummy_platform(&self) -> DummyPlatform {
        let babel_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target")
            .join("x86_64-unknown-linux-musl")
            .join("release");
        DummyPlatform {
            bv_root: self.bv_root.clone(),
            babel_path: babel_dir.join("babel"),
            job_runner_path: babel_dir.join("babel_job_runner"),
            token: self.token,
        }
    }

    pub async fn run_blockvisord(&mut self, run: RunFlag) -> Result<JoinHandle<Result<()>>> {
        let blockvisord = BlockvisorD::new(self.build_dummy_platform()).await?;
        self.api_config.blockvisor_port = blockvisord.local_addr()?.port();
        self.api_config.save(&self.bv_root).await?;
        Ok(tokio::spawn(blockvisord.run(run)))
    }

    pub fn bv_run(&self, commands: &[&str], stdout_pattern: &str) {
        bv_run(commands, stdout_pattern, Some(&self.bv_root));
    }

    pub fn try_bv_run(&self, commands: &[&str], stdout_pattern: &str) -> bool {
        try_bv_run(commands, stdout_pattern, Some(&self.bv_root))
    }

    pub async fn wait_for_running_node(&self, vm_id: &str, timeout: Duration) {
        wait_for_node_status(vm_id, "Running", timeout, Some(&self.bv_root)).await
    }

    pub async fn wait_for_node_fail(&self, vm_id: &str, timeout: Duration) {
        println!("wait for node to permanently fail");
        let node_path = self
            .bv_root
            .join(BV_VAR_PATH)
            .join(REGISTRY_CONFIG_DIR)
            .join(format!("{vm_id}.json"));
        let start = std::time::Instant::now();
        loop {
            if NodeData::<DummyNet>::load(&node_path)
                .await
                .unwrap()
                .expected_status
                == NodeStatus::Failed
            {
                break;
            } else if start.elapsed() < timeout {
                sleep(Duration::from_secs(5)).await;
            } else {
                panic!("timeout expired")
            }
        }
    }

    pub fn create_node(&self, image: &str, ip: &str) -> (String, String) {
        let mut cmd = Command::cargo_bin("bv").unwrap();
        let cmd = cmd
            .args([
                "node",
                "create",
                image,
                "--props",
                r#"{"network": "test", "TESTING_PARAM":"anything"}"#,
                "--gateway",
                "216.18.214.193",
                "--ip",
                ip,
            ])
            .env("BV_ROOT", &self.bv_root);
        let output = cmd.output().unwrap();
        let stdout = str::from_utf8(&output.stdout).unwrap();
        let stderr = str::from_utf8(&output.stderr).unwrap();
        println!("create stdout: {stdout}");
        println!("create stderr: {stderr}");
        let vm_id = stdout
            .trim_start_matches(&format!("Created new node from `{image}` image with ID "))
            .split('`')
            .nth(1)
            .unwrap()
            .to_string();
        let vm_name = stdout.split('`').rev().nth(1).unwrap().to_string();
        (vm_id, vm_name)
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.bv_root);
    }
}

pub fn bv_run(commands: &[&str], stdout_pattern: &str, bv_root: Option<&Path>) {
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(commands).env("NO_COLOR", "1");
    if let Some(bv_root) = bv_root {
        cmd.env("BV_ROOT", bv_root);
    }
    cmd.assert()
        .success()
        .stdout(predicate::str::contains(stdout_pattern));
}

pub fn try_bv_run(commands: &[&str], stdout_pattern: &str, bv_root: Option<&Path>) -> bool {
    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.args(commands).env("NO_COLOR", "1");
    if let Some(bv_root) = bv_root {
        cmd.env("BV_ROOT", bv_root);
    }
    cmd.assert()
        .try_stdout(predicate::str::contains(stdout_pattern))
        .is_ok()
}

pub async fn wait_for_node_status(
    vm_id: &str,
    status: &str,
    timeout: Duration,
    bv_root: Option<&Path>,
) {
    println!("wait for {status} node");
    let start = std::time::Instant::now();
    while !try_bv_run(&["node", "status", vm_id], status, bv_root) {
        if start.elapsed() < timeout {
            sleep(Duration::from_secs(1)).await;
        } else {
            panic!("timeout expired")
        }
    }
}

#[derive(Debug)]
pub struct DummyPlatform {
    pub(crate) bv_root: PathBuf,
    pub(crate) babel_path: PathBuf,
    pub(crate) job_runner_path: PathBuf,
    pub(crate) token: u32,
}

#[async_trait]
impl Pal for DummyPlatform {
    fn bv_root(&self) -> &Path {
        &self.bv_root
    }

    fn babel_path(&self) -> &Path {
        &self.babel_path
    }

    fn job_runner_path(&self) -> &Path {
        self.job_runner_path.as_path()
    }

    type NetInterface = DummyNet;

    async fn create_net_interface(
        &self,
        index: u32,
        ip: IpAddr,
        gateway: IpAddr,
    ) -> Result<Self::NetInterface> {
        let name = format!("bv{}t{}", index, self.token);
        // remove remnants after failed tests if any
        let _ = run_cmd("ip", ["link", "delete", &name, "type", "tuntap"]).await;
        Ok(DummyNet { name, ip, gateway })
    }

    type CommandsStream = EmptyStream;
    type CommandsStreamConnector = EmptyStreamConnector;

    fn create_commands_stream_connector(
        &self,
        _config: &SharedConfig,
    ) -> Self::CommandsStreamConnector {
        EmptyStreamConnector
    }

    type NodeConnection = blockvisord::node_connection::NodeConnection;

    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection {
        blockvisord::node_connection::new(&self.bv_root.join(BV_VAR_PATH), node_id)
    }

    type VirtualMachine = firecracker_machine::FirecrackerMachine;

    async fn create_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        firecracker_machine::create(&self.bv_root, node_data).await
    }

    async fn attach_vm(
        &self,
        node_data: &NodeData<Self::NetInterface>,
    ) -> Result<Self::VirtualMachine> {
        firecracker_machine::attach(&self.bv_root, node_data).await
    }

    fn build_vm_data_path(&self, id: Uuid) -> PathBuf {
        firecracker_machine::build_vm_data_path(&self.bv_root, id)
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DummyNet {
    pub name: String,
    pub ip: IpAddr,
    pub gateway: IpAddr,
}

#[async_trait]
impl NetInterface for DummyNet {
    fn name(&self) -> &String {
        &self.name
    }
    fn ip(&self) -> &IpAddr {
        &self.ip
    }
    fn gateway(&self) -> &IpAddr {
        &self.gateway
    }
    async fn remaster(&self) -> Result<()> {
        Ok(())
    }
    async fn delete(self) -> Result<()> {
        Ok(())
    }
}

pub struct EmptyStreamConnector;
pub struct EmptyStream;

#[async_trait]
impl ServiceConnector<EmptyStream> for EmptyStreamConnector {
    async fn connect(&self) -> Result<EmptyStream> {
        Ok(EmptyStream)
    }
}

#[async_trait]
impl CommandsStream for EmptyStream {
    async fn wait_for_pending_commands(&mut self) -> Result<Option<Vec<u8>>> {
        sleep(Duration::from_millis(100)).await;
        Ok(None)
    }
}
