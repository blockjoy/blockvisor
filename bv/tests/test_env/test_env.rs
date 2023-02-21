use anyhow::Result;
use assert_cmd::Command;
use async_trait::async_trait;
use blockvisord::blockvisord::BlockvisorD;
use blockvisord::config::Config;
use blockvisord::pal::{NetInterface, Pal};
use blockvisord::services::cookbook::IMAGES_DIR;
use blockvisord::utils::run_cmd;
use blockvisord::BV_VAR_PATH;
use bv_utils::run_flag::RunFlag;
use predicates::prelude::predicate;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::task::JoinHandle;

/// Global integration tests token. All tests (that may run in parallel) share common FS and net devices space
/// (tap devices which are created by Firecracker during tests).
/// Each test shall pick its own token that can be then used to create unique bv_root and net device names,
/// so tests won't collide.
pub static TEST_TOKEN: AtomicU32 = AtomicU32::new(0);

/// Common staff to setup for all tests like sut (blockvisord in that case),
/// path to root dir used in test, instance of DummyPlatform.
pub struct TestEnv {
    pub bv_root: PathBuf,
    api_config: Config,
    token: u32,
}

impl TestEnv {
    pub async fn new() -> Result<Self> {
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

        let api_config = Config {
            id: "host_id".to_owned(),
            token: "token".to_owned(),
            blockjoy_api_url: "http://localhost:8070".to_owned(),
            blockjoy_keys_url: "http://localhost:8070".to_owned(),
            blockjoy_registry_url: "http://localhost:50041".to_owned(),
            blockjoy_mqtt_url: "mqtt://localhost:1873".to_string(),
            update_check_interval_secs: None,
            blockvisor_port: 0, // 0 has special meaning - pick first free port
        };
        api_config.save(&bv_root).await?;
        Ok(Self {
            bv_root,
            api_config,
            token,
        })
    }

    pub async fn run_blockvisord(&mut self, run: RunFlag) -> Result<JoinHandle<Result<()>>> {
        let blockvisord = BlockvisorD::new(DummyPlatform {
            bv_root: self.bv_root.clone(),
            babel_path: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../target")
                .join("x86_64-unknown-linux-musl")
                .join("release")
                .join("babel"),
            token: self.token,
        })
        .await?;
        self.api_config.blockvisor_port = blockvisord.local_addr()?.port();
        self.api_config.save(&self.bv_root).await?;
        Ok(tokio::spawn(blockvisord.run(run)))
    }

    pub fn bv_run(&self, commands: &[&str], stdout_pattern: &str) {
        let mut cmd = Command::cargo_bin("bv").unwrap();
        cmd.args(commands)
            .env("BV_ROOT", &self.bv_root)
            .env("NO_COLOR", "1")
            .assert()
            .success()
            .stdout(predicate::str::contains(stdout_pattern));
    }

    pub fn try_bv_run(&self, commands: &[&str], stdout_pattern: &str) -> bool {
        let mut cmd = Command::cargo_bin("bv").unwrap();
        cmd.args(commands)
            .env("BV_ROOT", &self.bv_root)
            .env("NO_COLOR", "1")
            .assert()
            .success()
            .try_stdout(predicate::str::contains(stdout_pattern))
            .is_ok()
    }

    pub fn create_node(&self, image: &str) -> String {
        use std::str;

        let mut cmd = Command::cargo_bin("bv").unwrap();
        let cmd = cmd
            .args([
                "node",
                "create",
                image,
                "--props",
                r#"{"TESTING_PARAM":"anything"}"#,
                "--gateway",
                "216.18.214.193",
                "--ip",
                "216.18.214.195",
            ])
            .env("BV_ROOT", &self.bv_root);
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
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.bv_root);
    }
}

#[derive(Debug)]
pub struct DummyPlatform {
    bv_root: PathBuf,
    babel_path: PathBuf,
    token: u32,
}

#[async_trait]
impl Pal for DummyPlatform {
    fn bv_root(&self) -> &Path {
        &self.bv_root
    }

    fn babel_path(&self) -> &Path {
        &self.babel_path
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
    async fn remaster(self) -> Result<()> {
        Ok(())
    }
    async fn delete(self) -> Result<()> {
        Ok(())
    }
}
