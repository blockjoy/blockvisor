use assert_cmd::{assert::AssertResult, Command};
use async_trait::async_trait;

use blockvisord::api_config::ApiConfig;
use blockvisord::{
    apptainer_machine,
    apptainer_platform::BareNodeConnection,
    blockvisord::BlockvisorD,
    bv_config::{ApptainerConfig, Config, SharedConfig},
    bv_context::BvContext,
    node_context,
    node_context::NODES_DIR,
    node_env,
    node_state::{NodeState, VmStatus},
    nodes_manager::NodesDataCache,
    pal::{AvailableResources, NodeFirewallConfig, RecoverBackoff},
    pal::{CommandsStream, Pal, ServiceConnector},
    services::{self, ApiInterceptor, AuthToken},
    BV_VAR_PATH,
};
use bv_utils::{logging::setup_logging, rpc::DefaultTimeout, run_flag::RunFlag};
use eyre::Result;
use predicates::prelude::predicate;
use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    str,
    str::FromStr,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::{task::JoinHandle, time::sleep};
use tonic::transport::Channel;
use uuid::Uuid;

/// Global integration tests token. All tests (that may run in parallel) share common FS. Each test shall pick
/// its own token that can be then used to create unique bv_root and net device names, so tests
/// won't collide.
pub static TEST_TOKEN: AtomicU32 = AtomicU32::new(0);

/// Common staff to setup for all tests like sut (blockvisord in that case),
/// path to root dir used in test, instance of DummyPlatform.
pub struct TestEnv {
    pub bv_root: PathBuf,
    pub api_config: Config,
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
        fs::create_dir_all(bv_root.join("usr"))?;
        // link to /usr/bin where apptainer is expected
        std::os::unix::fs::symlink(
            Path::new("/").join("usr").join("bin"),
            bv_root.join("usr").join("bin"),
        )?;
        link_apptainer_config(&bv_root)?;
        api_config.blockvisor_port = 0;
        api_config.save(&bv_root).await?;
        Ok(Self {
            bv_root,
            api_config,
        })
    }

    pub async fn new() -> Result<Self> {
        setup_logging();
        let api_config = Config {
            id: "host_id".to_owned(),
            name: "host_name".to_string(),
            api_config: ApiConfig {
                token: "token".to_owned(),
                refresh_token: "fresh boii".to_owned(),
                blockjoy_api_url: "http://localhost:8070".to_owned(),
            },
            blockjoy_mqtt_url: Some("mqtt://localhost:1873".to_string()),
            blockvisor_port: 0, // 0 has special meaning - pick first free port
            iface: "bvbr0".to_string(),
            ..Default::default()
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
        }
    }

    pub async fn run_blockvisord(&mut self, run: RunFlag) -> Result<JoinHandle<Result<()>>> {
        self.run_blockvisord_with_pal(run, self.build_dummy_platform())
            .await
    }

    pub async fn run_blockvisord_with_pal(
        &mut self,
        run: RunFlag,
        pal: DummyPlatform,
    ) -> Result<JoinHandle<Result<()>>> {
        let blockvisord = BlockvisorD::new(pal, Config::load(&self.bv_root).await?).await?;
        self.api_config.blockvisor_port = blockvisord.local_addr()?.port();
        self.api_config.save(&self.bv_root).await?;
        Ok(tokio::spawn(blockvisord.run(run)))
    }

    pub fn bv_run(&self, commands: &[&str], stdout_pattern: &str) {
        bv_run(commands, stdout_pattern, Some(&self.bv_root));
    }

    pub fn try_bv_run(&self, commands: &[&str], stdout_pattern: &str) -> AssertResult {
        try_bv_run(commands, stdout_pattern, Some(&self.bv_root))
    }

    pub async fn wait_for_running_node(&self, vm_id: &str, timeout: Duration) {
        wait_for_node_status(vm_id, "Running", timeout, Some(&self.bv_root)).await
    }

    pub async fn wait_for_job_status(
        &self,
        vm_id: &str,
        job: &str,
        status: &str,
        timeout: Duration,
    ) {
        wait_for_job_status(vm_id, job, status, timeout, Some(&self.bv_root)).await
    }

    pub async fn wait_for_node_fail(&self, vm_id: &str, timeout: Duration) {
        println!("wait for node to permanently fail");
        let node_path = self
            .bv_root
            .join(BV_VAR_PATH)
            .join(NODES_DIR)
            .join(format!("{vm_id}/state.json"));
        let start = std::time::Instant::now();
        loop {
            if NodeState::load(&node_path).await.unwrap().expected_status == VmStatus::Failed {
                break;
            } else if start.elapsed() < timeout {
                sleep(Duration::from_secs(5)).await;
            } else {
                panic!("timeout expired")
            }
        }
    }

    pub fn create_node(&self, image: &str, ip: &str) -> (String, String) {
        let mut cmd = Command::cargo_bin("bib").unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(image)
            .join("babel.yaml");
        let cmd = cmd
            .args([
                "image",
                "play",
                "--props",
                r#"{"network": "test", "TESTING_PARAM":"anything"}"#,
                "--gateway",
                "216.18.214.193",
                "--ip",
                ip,
                &path.to_string_lossy(),
            ])
            .env("BV_ROOT", &self.bv_root);
        let output = cmd.output().unwrap();
        let stdout = str::from_utf8(&output.stdout).unwrap();
        let stderr = str::from_utf8(&output.stderr).unwrap();
        println!("create stdout: {stdout}");
        println!("create stderr: {stderr}");
        let vm_id = stdout
            .trim_start_matches(&format!(
                "Created new dev_node from `{image}` image with ID "
            ))
            .split('`')
            .nth(1)
            .unwrap()
            .to_string();
        let vm_name = stdout.split('`').rev().nth(1).unwrap().to_string();
        (vm_id, vm_name)
    }

    pub fn sh_inside(&self, node_id: &str, sh_script: &str) -> String {
        String::from_utf8(
            Command::cargo_bin("bv")
                .unwrap()
                .args(["node", "run", "--param", sh_script, "sh_inside", node_id])
                .env("BV_ROOT", &self.bv_root)
                .assert()
                .success()
                .get_output()
                .stdout
                .to_owned(),
        )
        .unwrap()
    }
}

pub fn link_apptainer_config(bv_root: &Path) -> Result<()> {
    fs::create_dir_all(bv_root.join("usr/local/etc"))?;
    // link apptainer config
    std::os::unix::fs::symlink(
        Path::new("/usr/local/etc/apptainer"),
        bv_root.join("usr/local/etc/apptainer"),
    )?;
    Ok(())
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.bv_root);
    }
}

pub fn bv_run(commands: &[&str], stdout_pattern: &str, bv_root: Option<&Path>) {
    bv_tool_run("bv", commands, stdout_pattern, bv_root)
}

pub fn try_bv_run(commands: &[&str], stdout_pattern: &str, bv_root: Option<&Path>) -> AssertResult {
    try_bv_tool_run("bv", commands, stdout_pattern, bv_root)
}

pub fn bib_run(commands: &[&str], stdout_pattern: &str, bv_root: Option<&Path>) {
    bv_tool_run("bib", commands, stdout_pattern, bv_root)
}

fn bv_tool_run(tool_name: &str, commands: &[&str], stdout_pattern: &str, bv_root: Option<&Path>) {
    let mut cmd = Command::cargo_bin(tool_name).unwrap();
    cmd.args(commands).env("NO_COLOR", "1");
    if let Some(bv_root) = bv_root {
        cmd.env("BV_ROOT", bv_root);
    }
    cmd.assert()
        .success()
        .stdout(predicate::str::contains(stdout_pattern));
}

fn try_bv_tool_run(
    tool_name: &str,
    commands: &[&str],
    stdout_pattern: &str,
    bv_root: Option<&Path>,
) -> AssertResult {
    let mut cmd = Command::cargo_bin(tool_name).unwrap();
    cmd.args(commands).env("NO_COLOR", "1");
    if let Some(bv_root) = bv_root {
        cmd.env("BV_ROOT", bv_root);
    }
    cmd.assert()
        .try_stdout(predicate::str::contains(stdout_pattern))
}

pub async fn wait_for_node_status(
    vm_id: &str,
    status: &str,
    timeout: Duration,
    bv_root: Option<&Path>,
) {
    println!("wait for {status} node");
    wait_for_expected(
        || try_bv_run(&["node", "status", vm_id], status, bv_root),
        timeout,
    )
    .await;
}

pub async fn wait_for_job_status(
    vm_id: &str,
    job: &str,
    status: &str,
    timeout: Duration,
    bv_root: Option<&Path>,
) {
    println!("wait for {status} '{job}' job");
    wait_for_expected(
        || try_bv_run(&["node", "job", vm_id, "info", job], status, bv_root),
        timeout,
    )
    .await;
}

pub async fn wait_for_expected(call: impl Fn() -> AssertResult, timeout: Duration) {
    let start = std::time::Instant::now();
    while let Err(err) = call() {
        if start.elapsed() < timeout {
            sleep(Duration::from_secs(1)).await;
        } else {
            panic!("timeout expired: {err:#}")
        }
    }
}

#[derive(Debug)]
pub struct DummyPlatform {
    pub(crate) bv_root: PathBuf,
    pub(crate) babel_path: PathBuf,
    pub(crate) job_runner_path: PathBuf,
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

    type CommandsStream = EmptyStream;
    type CommandsStreamConnector = EmptyStreamConnector;

    fn create_commands_stream_connector(
        &self,
        _config: &SharedConfig,
    ) -> Self::CommandsStreamConnector {
        EmptyStreamConnector
    }

    type ApiServiceConnector = DummyApiConnector;
    fn create_api_service_connector(&self, _config: &SharedConfig) -> Self::ApiServiceConnector {
        DummyApiConnector
    }

    type NodeConnection = BareNodeConnection;
    fn create_node_connection(&self, node_id: Uuid) -> Self::NodeConnection {
        BareNodeConnection::new(node_context::build_node_dir(&self.bv_root, node_id))
    }

    type VirtualMachine = apptainer_machine::ApptainerMachine;

    async fn create_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<Self::VirtualMachine> {
        let vm = apptainer_machine::new(
            &self.bv_root,
            IpAddr::from_str("216.18.214.90")?,
            24,
            node_env::new(bv_context, node_state),
            node_state,
            self.babel_path.clone(),
            ApptainerConfig {
                extra_args: None,
                host_network: true,
                cpu_limit: true,
                memory_limit: true,
            },
        )
        .await?;
        vm.create().await?;
        Ok(vm)
    }

    async fn attach_vm(
        &self,
        bv_context: &BvContext,
        node_state: &NodeState,
    ) -> Result<Self::VirtualMachine> {
        let mut vm = apptainer_machine::new(
            &self.bv_root,
            IpAddr::from_str("216.18.214.90")?,
            24,
            node_env::new(bv_context, node_state),
            node_state,
            self.babel_path.clone(),
            ApptainerConfig {
                extra_args: None,
                host_network: true,
                cpu_limit: true,
                memory_limit: true,
            },
        )
        .await?;
        vm.attach().await?;
        Ok(vm)
    }

    async fn available_cpus(&self) -> usize {
        4
    }

    async fn available_resources(
        &self,
        _nodes_data_cache: NodesDataCache,
    ) -> Result<blockvisord::pal::AvailableResources> {
        Ok(AvailableResources {
            vcpu_count: 4,
            mem_size_mb: 4096,
            disk_size_gb: 10,
        })
    }

    async fn used_disk_space_correction(&self, _nodes_data_cache: NodesDataCache) -> Result<u64> {
        Ok(0)
    }

    type RecoveryBackoff = DummyBackoff;

    fn create_recovery_backoff(&self) -> DummyBackoff {
        Default::default()
    }

    async fn apply_firewall_config(&self, _config: NodeFirewallConfig) -> Result<()> {
        Ok(())
    }
    async fn cleanup_firewall_config(&self, _id: Uuid) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct DummyApiConnector;

#[async_trait]
impl services::ApiServiceConnector for DummyApiConnector {
    async fn connect<T, I>(&self, with_interceptor: I) -> Result<T, tonic::Status>
    where
        I: Send + Sync + Fn(Channel, ApiInterceptor) -> T,
    {
        Ok(with_interceptor(
            Channel::from_static("http://dummy.url").connect_lazy(),
            ApiInterceptor(
                AuthToken("test_token".to_owned()),
                DefaultTimeout(Duration::from_secs(1)),
            ),
        ))
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
        sleep(Duration::from_secs(3)).await;
        Ok(None)
    }
}

#[derive(Debug, Default)]
pub struct DummyBackoff {
    reconnect: u32,
    stop: u32,
    start: u32,
    vm: u32,
}

impl RecoverBackoff for DummyBackoff {
    fn backoff(&self) -> bool {
        false
    }
    fn reset(&mut self) {}
    fn start_failed(&mut self) -> bool {
        self.start += 1;
        self.start >= 1
    }
    fn stop_failed(&mut self) -> bool {
        self.stop += 1;
        self.stop >= 1
    }
    fn reconnect_failed(&mut self) -> bool {
        self.reconnect += 1;
        self.reconnect >= 1
    }

    fn vm_recovery_failed(&mut self) -> bool {
        self.vm += 1;
        self.vm >= 1
    }
}
