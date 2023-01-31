/// This module implements supervisor for node entry points. It spawn child processes as defined in
/// given config and watch them. Stopped child (whatever reason) is respawned with exponential backoff
/// timeout. Backoff timeout is reset after child stays alive for at least `backoff_timeout_ms`.
use crate::log_buffer::LogBuffer;
use crate::run_flag::RunFlag;
use async_trait::async_trait;
use babel_api::config::{Entrypoint, SupervisorConfig};
use eyre::bail;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;
use sysinfo::{Pid, Process, ProcessExt, System, SystemExt};
use tokio::process::Command;
use tokio::sync::{oneshot, watch};
use tokio::time::Duration;
use tracing::{debug, error, info, warn};

/// Time abstraction for better testing.
#[async_trait]
pub trait Timer {
    fn now(&self) -> Instant;
    async fn sleep(&self, duration: Duration);
}

pub fn load_config(json_str: &str) -> eyre::Result<SupervisorConfig> {
    let cfg: SupervisorConfig = serde_json::from_str(json_str)?;
    if cfg.entry_point.is_empty() {
        bail!("no entry point defined");
    }
    debug!("Loaded supervisor configuration: {:?}", &cfg);
    Ok(cfg)
}

pub struct SupervisorSetup {
    pub log_buffer: LogBuffer,
    pub config: SupervisorConfig,
}

impl SupervisorSetup {
    pub fn new(config: SupervisorConfig) -> Self {
        let log_buffer = LogBuffer::new(config.log_buffer_capacity_ln);
        Self { log_buffer, config }
    }
}

pub type BabelChangeTx = watch::Sender<Option<u32>>;
pub type BabelChangeRx = watch::Receiver<Option<u32>>;
pub type SupervisorSetupRx = oneshot::Receiver<SupervisorSetup>;
pub type SupervisorSetupTx = oneshot::Sender<SupervisorSetup>;

pub async fn run<T: Timer>(
    timer: T,
    run: RunFlag,
    babel_path: PathBuf,
    sup_setup_rx: SupervisorSetupRx,
    babel_change_rx: BabelChangeRx,
) {
    let babel_change_rx = wait_for_babel_bin(run.clone(), babel_change_rx).await;
    if let Some(supervisor) = wait_for_setup(timer, run, babel_path, sup_setup_rx).await {
        supervisor.kill_all_remnants();

        let mut futures = FuturesUnordered::new();
        for entry_point in &supervisor.config.entry_point {
            futures.push(supervisor.run_entrypoint(entry_point));
        }
        let entry_futures = async { while (futures.next().await).is_some() {} };
        tokio::join!(entry_futures, supervisor.run_babel(babel_change_rx));
    }
}

async fn wait_for_babel_bin(mut run: RunFlag, mut babel_change_rx: BabelChangeRx) -> BabelChangeRx {
    // if there is no babel binary yet, then just wait for babel start signal from blockvisord
    if babel_change_rx.borrow_and_update().is_none() {
        tokio::select!(
            _ = babel_change_rx.changed() => {},
            _ = run.wait() => {},
        );
    }
    babel_change_rx
}

async fn wait_for_setup<T: Timer>(
    timer: T,
    mut run: RunFlag,
    babel_path: PathBuf,
    sup_setup_rx: SupervisorSetupRx,
) -> Option<Supervisor<T>> {
    tokio::select!(
        setup = sup_setup_rx => {
            Some(Supervisor::new(timer, run, babel_path, setup.ok()?))
        },
        _ = run.wait() => None, // return anything
    )
}

struct Supervisor<T: Timer> {
    run: RunFlag,
    babel_path: PathBuf,
    log_buffer: LogBuffer,
    config: SupervisorConfig,
    timer: T,
}

impl<T: Timer> Supervisor<T> {
    fn new(timer: T, run: RunFlag, babel_path: PathBuf, setup: SupervisorSetup) -> Self {
        Supervisor {
            run,
            babel_path,
            log_buffer: setup.log_buffer,
            config: setup.config,
            timer,
        }
    }

    /// Check if there are no remnant child processes after previous run.
    /// If so, just kill them all.
    fn kill_all_remnants(&self) {
        let mut sys = System::new();
        sys.refresh_processes();
        let ps = sys.processes();
        kill_remnants(&self.babel_path.to_string_lossy(), &[], ps);
        for entry_point in &self.config.entry_point {
            kill_remnants("sh", &["-c", &entry_point.body], ps);
        }
    }

    async fn run_babel(&self, mut babel_change_rx: BabelChangeRx) {
        let mut cmd = Command::new(&self.babel_path);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut run = self.run.clone();
        let mut backoff = Backoff::new(&self.timer, run.clone(), &self.config);
        while run.load() {
            backoff.start();
            if let Ok(mut child) = cmd.spawn() {
                info!("Spawned Babel");
                self.log_buffer
                    .attach("babel", child.stdout.take(), child.stderr.take());
                tokio::select!(
                    _ = child.wait() => {
                        error!("Babel stopped unexpected");
                        backoff.wait().await;
                    },
                    _ = babel_change_rx.changed() => {
                        info!("Babel changed - restart service");
                        let _ = child.kill().await;
                    },
                    _ = run.wait() => {
                        info!("Supervisor stopped, killing babel");
                        let _ = child.kill().await;
                    },
                );
            } else {
                error!("Failed to spawn babel");
                backoff.wait().await;
            }
        }
    }

    async fn run_entrypoint(&self, entrypoint: &Entrypoint) {
        let entry_name = &entrypoint.name;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &entrypoint.body])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut run = self.run.clone();
        let mut backoff = Backoff::new(&self.timer, run.clone(), &self.config);
        while run.load() {
            backoff.start();
            if let Ok(mut child) = cmd.spawn() {
                info!("Spawned entrypoint '{entry_name}'");
                self.log_buffer
                    .attach(entry_name, child.stdout.take(), child.stderr.take());
                tokio::select!(
                    _ = child.wait() => {
                        warn!("Entrypoint stopped unexpected '{entry_name}'");
                        backoff.wait().await;
                    },
                    _ = run.wait() => {
                        info!("Supervisor stopped, killing entrypoint '{entry_name}'");
                        let _ = child.kill().await;
                    },
                );
            } else {
                warn!("Failed to spawn entrypoint '{entry_name}'");
                backoff.wait().await;
            }
        }
    }
}

fn kill_remnants(cmd: &str, args: &[&str], ps: &HashMap<Pid, Process>) {
    let remnants: Vec<_> = ps
        .iter()
        .filter(|(_, process)| {
            let proc_call = process.cmd();
            if let Some(proc_cmd) = proc_call.first() {
                if proc_cmd == "/bin/sh" {
                    // if first element is shell call, just ignore it and treat second as cmd, rest are arguments
                    proc_call.len() > 1 && cmd == proc_call[1] && args == proc_call[2..].to_vec()
                } else {
                    // first element is cmd, rest are arguments
                    cmd == proc_cmd && args == proc_call[1..].to_vec()
                }
            } else {
                false
            }
        })
        .collect();

    for (_, proc) in remnants {
        proc.kill();
        proc.wait();
    }
}

struct Backoff<'a, T: Timer> {
    counter: u32,
    timestamp: Instant,
    backoff_base_ms: u64,
    reset_timeout: Duration,
    run: RunFlag,
    timer: &'a T,
}

impl<'a, T: Timer> Backoff<'a, T> {
    fn new(timer: &'a T, run: RunFlag, config: &SupervisorConfig) -> Self {
        Self {
            counter: 0,
            timestamp: Instant::now(),
            backoff_base_ms: config.backoff_base_ms,
            reset_timeout: Duration::from_millis(config.backoff_timeout_ms),
            run,
            timer,
        }
    }

    fn start(&mut self) {
        self.timestamp = self.timer.now();
    }

    async fn wait(&mut self) {
        let now = self.timer.now();
        let duration = now.duration_since(self.timestamp);
        if duration > self.reset_timeout {
            self.counter = 0;
        } else {
            tokio::select!(
                _ = self.timer.sleep(Duration::from_millis(self.backoff_base_ms * 2u64.pow(self.counter))) => {},
                _ = self.run.wait() => {},
            );
            self.counter += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_flag::RunFlag;
    use assert_fs::TempDir;
    use async_trait::async_trait;
    use eyre::Result;
    use mockall::*;
    use std::fs;
    use std::io::Write;
    use std::ops::Add;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::broadcast;
    use tokio::time::Duration;

    mock! {
        pub TestTimer {}

        #[async_trait]
        impl Timer for TestTimer {
            fn now(&self) -> Instant;
            async fn sleep(&self, duration: Duration);
        }
    }

    struct TestEnv {
        tmp_root: PathBuf,
        ctrl_file: PathBuf,
        babel_path: PathBuf,
        run: RunFlag,
        babel_change_tx: BabelChangeTx,
        babel_change_rx: BabelChangeRx,
        sup_setup_tx: Option<SupervisorSetupTx>,
        sup_setup_rx: SupervisorSetupRx,
    }

    impl TestEnv {
        fn setup(&mut self, config: SupervisorConfig) -> Option<broadcast::Receiver<String>> {
            let sup_setup_tx = self.sup_setup_tx.take()?;
            let sup_setup = SupervisorSetup::new(config);
            let rx = sup_setup.log_buffer.subscribe();
            sup_setup_tx.send(sup_setup).ok();
            Some(rx)
        }
    }

    fn setup_test_env() -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let ctrl_file = tmp_root.join("babel_started");
        let babel_path = tmp_root.join("babel");
        let run = Default::default();

        // create dummy babel that will touch control file and sleep
        fs::create_dir_all(babel_path.parent().unwrap())?;
        let _ = fs::remove_file(&babel_path);
        let mut babel = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o770)
            .open(&babel_path)?;
        writeln!(babel, "#!/bin/sh")?;
        writeln!(babel, "echo \"babel log\"")?;
        writeln!(babel, "touch {}", ctrl_file.to_string_lossy())?;
        writeln!(babel, "sleep infinity")?;
        let (babel_change_tx, babel_change_rx) = watch::channel(Some(0));
        let (sup_setup_tx, sup_setup_rx) = oneshot::channel();
        Ok(TestEnv {
            tmp_root,
            ctrl_file,
            babel_path,
            run,
            babel_change_tx,
            babel_change_rx,
            sup_setup_tx: Some(sup_setup_tx),
            sup_setup_rx,
        })
    }

    fn wait_for_babel(mut test_run: RunFlag, control_file: PathBuf) {
        tokio::spawn(async move {
            // asynchronously wait for dummy babel to start
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                while !control_file.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            // and send stop signal
            test_run.stop();
        });
    }

    fn minimal_cfg() -> SupervisorConfig {
        SupervisorConfig {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            log_buffer_capacity_ln: 10,
            entry_point: vec![Entrypoint {
                name: "echo".to_owned(),
                body: "echo test".to_owned(),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_backoff_timeout_ms() -> Result<()> {
        let mut test_env = setup_test_env()?;
        let cfg = minimal_cfg();
        let now = Instant::now();

        let mut test_run = test_env.run.clone();
        let mut timer_mock = MockTestTimer::new();
        timer_mock.expect_now().once().returning(move || now);
        timer_mock.expect_now().once().returning(move || now);
        timer_mock
            .expect_now()
            .times(3)
            .returning(move || now.add(Duration::from_millis(cfg.backoff_timeout_ms + 1)));
        timer_mock.expect_sleep().once().returning(move |_| {
            test_run.stop();
        });

        test_env.setup(cfg);
        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_setup_rx,
            test_env.babel_change_rx,
        )
        .await;
        Ok(())
    }

    #[tokio::test]
    async fn test_exponential_backoff() -> Result<()> {
        let mut test_env = setup_test_env()?;
        let mut test_run = test_env.run.clone();

        let now = Instant::now();

        let mut timer_mock = MockTestTimer::new();
        timer_mock.expect_now().once().returning(move || now);
        const RANGE: u32 = 8;
        for n in 0..RANGE {
            timer_mock.expect_now().times(2).returning(move || now);
            timer_mock
                .expect_sleep()
                .with(predicate::eq(Duration::from_millis(10 * 2u64.pow(n))))
                .returning(|_| ());
        }
        timer_mock.expect_now().times(2).returning(move || now);
        timer_mock
            .expect_sleep()
            .once()
            .with(predicate::eq(Duration::from_millis(10 * 2u64.pow(RANGE))))
            .returning(move |_| {
                test_run.stop();
            });
        test_env.setup(minimal_cfg());
        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_setup_rx,
            test_env.babel_change_rx,
        )
        .await;
        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_entry_points() -> Result<()> {
        let mut test_env = setup_test_env()?;
        let file_path = test_env.tmp_root.join("test_multiple_entry_points");
        let cfg = SupervisorConfig {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            entry_point: vec![
                Entrypoint {
                    name: "sleep".to_owned(),
                    body: "sleep infinity".to_owned(),
                },
                Entrypoint {
                    name: "sleep".to_owned(),
                    body: "sleep infinity".to_owned(),
                },
                Entrypoint {
                    name: "touch".to_owned(),
                    body: format!("touch {}", file_path.to_str().unwrap()),
                },
            ],
            ..Default::default()
        };
        let mut test_run = test_env.run.clone();

        let now = Instant::now();

        let mut timer_mock = MockTestTimer::new();
        timer_mock.expect_now().returning(move || now);
        let first_file_path = file_path.clone();
        timer_mock.expect_sleep().once().returning(move |_| {
            assert!(first_file_path.exists());
            let _ = fs::remove_file(&first_file_path);
        });
        let second_file_path = file_path.clone();
        timer_mock.expect_sleep().once().returning(move |_| {
            assert!(second_file_path.exists());
            let _ = fs::remove_file(&second_file_path);
            test_run.stop();
        });
        let _ = fs::remove_file(&file_path);
        test_env.setup(cfg);
        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_setup_rx,
            test_env.babel_change_rx,
        )
        .await;
        assert!(!file_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_babel_restart() -> Result<()> {
        let mut test_env = setup_test_env()?;
        let cfg = SupervisorConfig {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            entry_point: vec![Entrypoint {
                name: "sleep".to_owned(),
                body: "sleep infinity".to_owned(),
            }],
            ..Default::default()
        };
        let mut test_run = test_env.run.clone();

        test_env.setup(cfg.clone());
        let babel_change_tx = Arc::new(test_env.babel_change_tx);

        let now = Instant::now();
        let mut timer_mock = MockTestTimer::new();
        let change_tx = babel_change_tx.clone();
        // expect now from run_babel
        timer_mock.expect_now().once().returning(move || now);
        // expect now from run_entry_point
        let control_file = test_env.ctrl_file.clone();
        timer_mock.expect_now().once().returning(move || {
            let change_tx = change_tx.clone();
            assert!(!control_file.exists());
            let control_file = control_file.clone();
            tokio::spawn(async move {
                // asynchronously wait for dummy babel to start
                let _ = tokio::time::timeout(Duration::from_millis(500), async {
                    while !control_file.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await;
                // and send restart signal
                change_tx.send_modify(|value| {
                    let _ = value.insert(1);
                });
            });
            now
        });
        // expect now after babel restart
        timer_mock.expect_now().once().returning(move || {
            test_run.stop();
            now.add(Duration::from_millis(cfg.backoff_timeout_ms + 1))
        });

        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_setup_rx,
            test_env.babel_change_rx,
        )
        .await;
        assert!(test_env.ctrl_file.exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_logs() -> Result<()> {
        let mut test_env = setup_test_env()?;
        let mut test_run = test_env.run.clone();

        let now = Instant::now();

        let mut timer_mock = MockTestTimer::new();
        timer_mock.expect_now().returning(move || now);
        timer_mock.expect_now().returning(move || now);
        timer_mock.expect_sleep().times(3).returning(|_| ());
        let control_file = test_env.ctrl_file.clone();
        timer_mock.expect_sleep().returning(move |_| {
            test_run.stop();
            wait_for_babel(test_run.clone(), control_file.clone());
        });
        let mut rx = test_env.setup(minimal_cfg()).unwrap();
        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_setup_rx,
            test_env.babel_change_rx,
        )
        .await;

        let mut lines = Vec::default();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        lines.sort();
        assert_eq!(
            vec!["babel log\n", "test\n", "test\n", "test\n", "test\n"],
            lines
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_babel_only() -> Result<()> {
        let mut test_env = setup_test_env()?;
        let cfg = SupervisorConfig {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            log_buffer_capacity_ln: 10,
            ..Default::default()
        };
        let test_run = test_env.run.clone();

        let now = Instant::now();

        let mut timer_mock = MockTestTimer::new();
        timer_mock.expect_now().once().returning(move || now);
        wait_for_babel(test_run.clone(), test_env.ctrl_file.clone());
        let mut rx = test_env.setup(cfg).unwrap();
        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_setup_rx,
            test_env.babel_change_rx,
        )
        .await;

        let mut lines = Vec::default();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        assert_eq!(vec!["babel log\n"], lines);
        Ok(())
    }

    #[tokio::test]
    async fn test_kill_remnants() -> Result<()> {
        let test_env = setup_test_env()?;
        let mut cmd = Command::new(&test_env.babel_path);
        cmd.args(["a", "b", "c"]);
        let mut child = cmd.spawn()?;
        let test_run = test_env.run.clone();
        wait_for_babel(test_run.clone(), test_env.ctrl_file.clone());
        let mut sys = System::new();
        sys.refresh_processes();
        let ps = sys.processes();
        kill_remnants(&test_env.babel_path.to_string_lossy(), &["a", "b", "c"], ps);
        child.kill().await.unwrap_err();
        Ok(())
    }
}
