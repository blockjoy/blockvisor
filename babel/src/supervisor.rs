/// This module implements supervisor for node entry points. It spawn child processes as defined in
/// given config and watch them. Stopped child (whatever reason) is respawned with exponential backoff
/// timeout. Backoff timeout is reset after child stays alive for at least `backoff_timeout_ms`.
use crate::utils::Backoff;
use babel_api::babelsup::SupervisorConfig;
use bv_utils::{
    system::{find_processes, is_process_running},
    {run_flag::RunFlag, timer::AsyncTimer},
};
use std::path::{Path, PathBuf};
use std::time::Instant;
use sysinfo::{ProcessExt, Signal, System, SystemExt};
use tokio::{
    process::Command,
    sync::{oneshot, watch},
    time::Duration,
};
use tracing::{debug, error, info};

pub fn load_config(json_str: &str) -> eyre::Result<SupervisorConfig> {
    let cfg: SupervisorConfig = serde_json::from_str(json_str)?;
    debug!("Loaded supervisor configuration: {:?}", &cfg);
    Ok(cfg)
}

pub type BabelChangeTx = watch::Sender<Option<u32>>;
pub type BabelChangeRx = watch::Receiver<Option<u32>>;
pub type SupervisorConfigRx = oneshot::Receiver<SupervisorConfig>;
pub type SupervisorConfigTx = oneshot::Sender<SupervisorConfig>;

pub async fn run<T: AsyncTimer>(
    timer: T,
    mut run: RunFlag,
    babel_path: PathBuf,
    sup_config_rx: SupervisorConfigRx,
    babel_change_rx: BabelChangeRx,
) {
    let babel_change_rx = wait_for_babel_bin(run.clone(), babel_change_rx).await;
    if let Some(supervisor) = wait_for_setup(timer, run.clone(), babel_path, sup_config_rx).await {
        // Check if there are no babel after previous run.
        // If so, just kill it.
        kill_babel(&supervisor.babel_path);

        supervisor.run_babel(run, babel_change_rx).await;
    }
}

async fn wait_for_babel_bin(mut run: RunFlag, mut babel_change_rx: BabelChangeRx) -> BabelChangeRx {
    // if there is no babel binary yet, then just wait for babel start signal from blockvisord
    if babel_change_rx.borrow_and_update().is_none() {
        run.select(babel_change_rx.changed()).await;
    }
    babel_change_rx
}

async fn wait_for_setup<T: AsyncTimer>(
    timer: T,
    mut run: RunFlag,
    babel_path: PathBuf,
    sup_config_rx: SupervisorConfigRx,
) -> Option<Supervisor<T>> {
    if let Some(setup) = run.select(sup_config_rx).await {
        Some(Supervisor::new(timer, babel_path, setup.ok()?))
    } else {
        None
    }
}

fn kill_babel(path: &Path) {
    let mut sys = System::new();
    sys.refresh_processes();
    let ps = sys.processes();
    if let Some((_, proc)) = find_processes(&path.to_string_lossy(), &[], ps).next() {
        let now = Instant::now();
        proc.kill_with(Signal::Kill);
        while is_process_running(proc.pid()) {
            if now.elapsed() > Duration::from_secs(5) {
                proc.kill();
                proc.wait();
                break;
            }
            std::thread::sleep(Duration::from_secs(1))
        }
    };
}

struct Supervisor<T> {
    babel_path: PathBuf,
    config: SupervisorConfig,
    timer: T,
}

impl<T: AsyncTimer> Supervisor<T> {
    fn new(timer: T, babel_path: PathBuf, config: SupervisorConfig) -> Self {
        Supervisor {
            babel_path,
            config,
            timer,
        }
    }

    async fn run_babel(self, mut run: RunFlag, mut babel_change_rx: BabelChangeRx) {
        let mut cmd = Command::new(&self.babel_path);
        let mut backoff = Backoff::new(
            self.timer,
            run.clone(),
            self.config.backoff_base_ms,
            Duration::from_millis(self.config.backoff_timeout_ms),
        );
        while run.load() {
            backoff.start();
            match cmd.spawn() {
                Ok(mut child) => {
                    info!("Spawned Babel");
                    tokio::select!(
                        _ = child.wait() => {
                            error!("Babel stopped unexpected");
                            backoff.wait().await;
                        },
                        _ = babel_change_rx.changed() => {
                            info!("Babel changed - restart service");
                            kill_babel(&self.babel_path);
                        },
                        _ = run.wait() => {
                            info!("Supervisor stopped, killing babel");
                            kill_babel(&self.babel_path);
                        },
                    );
                }
                Err(err) => {
                    error!("Failed to spawn babel: {err:#}");
                    backoff.wait().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;
    use assert_fs::TempDir;
    use bv_utils::exp_backoff_timeout;
    use bv_utils::timer::MockAsyncTimer;
    use eyre::Result;
    use mockall::*;
    use std::fs;
    use std::ops::Add;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::time::Duration;

    struct TestEnv {
        ctrl_file: PathBuf,
        babel_path: PathBuf,
        run: RunFlag,
        babel_change_tx: BabelChangeTx,
        babel_change_rx: BabelChangeRx,
        sup_config_tx: Option<SupervisorConfigTx>,
        sup_config_rx: SupervisorConfigRx,
    }

    impl TestEnv {
        fn setup(&mut self, config: SupervisorConfig) {
            let sup_config_tx = self.sup_config_tx.take().unwrap();
            sup_config_tx.send(config).ok();
        }
    }

    fn setup_test_env(failing_babel: bool) -> Result<TestEnv> {
        let tmp_root = TempDir::new()?.to_path_buf();
        let ctrl_file = tmp_root.join("babel_started");
        let babel_path = tmp_root.join("babel");
        let run = Default::default();

        // create dummy babel that will touch control file and sleep
        fs::create_dir_all(&tmp_root)?;
        utils::tests::create_dummy_bin(&babel_path, &ctrl_file, !failing_babel);
        let (babel_change_tx, babel_change_rx) = watch::channel(Some(0));
        let (sup_config_tx, sup_config_rx) = oneshot::channel();
        Ok(TestEnv {
            ctrl_file,
            babel_path,
            run,
            babel_change_tx,
            babel_change_rx,
            sup_config_tx: Some(sup_config_tx),
            sup_config_rx,
        })
    }

    async fn wait_for_babel(control_file: PathBuf) {
        // asynchronously wait for dummy babel to start
        tokio::time::timeout(Duration::from_secs(3), async {
            while !control_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    fn minimal_cfg() -> SupervisorConfig {
        SupervisorConfig {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
        }
    }

    #[tokio::test]
    async fn test_backoff_timeout_ms() -> Result<()> {
        let mut test_env = setup_test_env(true)?;
        let cfg = minimal_cfg();
        let now = Instant::now();

        let mut test_run = test_env.run.clone();
        let mut timer_mock = MockAsyncTimer::new();
        timer_mock.expect_now().times(1).returning(move || now);
        timer_mock
            .expect_now()
            .returning(move || now.add(Duration::from_millis(cfg.backoff_timeout_ms + 1)));
        timer_mock.expect_sleep().once().returning(move |_| {
            test_run.stop();
        });

        test_env.setup(cfg);
        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_config_rx,
            test_env.babel_change_rx,
        )
        .await;
        Ok(())
    }

    #[tokio::test]
    async fn test_exponential_backoff() -> Result<()> {
        let mut test_env = setup_test_env(true)?;
        let mut test_run = test_env.run.clone();

        let now = Instant::now();

        let mut timer_mock = MockAsyncTimer::new();
        timer_mock.expect_now().returning(move || now);
        const RANGE: u32 = 8;
        for n in 0..RANGE {
            timer_mock
                .expect_sleep()
                .with(predicate::eq(exp_backoff_timeout(10, n)))
                .returning(|_| ());
        }
        timer_mock
            .expect_sleep()
            .once()
            .with(predicate::eq(exp_backoff_timeout(10, RANGE)))
            .returning(move |_| {
                test_run.stop();
            });
        test_env.setup(minimal_cfg());
        run(
            timer_mock,
            test_env.run,
            test_env.babel_path,
            test_env.sup_config_rx,
            test_env.babel_change_rx,
        )
        .await;
        Ok(())
    }

    #[tokio::test]
    async fn test_babel_restart() -> Result<()> {
        let mut test_env = setup_test_env(false)?;
        let cfg = minimal_cfg();
        let mut test_run = test_env.run.clone();

        test_env.setup(cfg.clone());
        let babel_change_tx = Arc::new(test_env.babel_change_tx);

        let now = Instant::now();
        let mut timer_mock = MockAsyncTimer::new();
        // expect now from run_babel
        timer_mock.expect_now().times(2).returning(move || now);
        let control_file = test_env.ctrl_file.clone();
        let change_tx = babel_change_tx.clone();
        assert!(!control_file.exists());
        tokio::spawn(async move {
            wait_for_babel(control_file).await;
            // and send restart signal
            change_tx.send_modify(|value| {
                let _ = value.insert(1);
            });
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
            test_env.sup_config_rx,
            test_env.babel_change_rx,
        )
        .await;
        assert!(test_env.ctrl_file.exists());
        Ok(())
    }
}
