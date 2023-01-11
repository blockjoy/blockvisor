use crate::log_buffer::LogBuffer;
use crate::run_flag::RunFlag;
use crate::utils;
use async_trait::async_trait;
use babel_api::config::{Entrypoint, SupervisorConfig as Config};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;
use sysinfo::{Pid, Process, ProcessExt, System, SystemExt};
use tokio::process::Command;
use tokio::sync::{broadcast, watch};
use tokio::time::Duration;
use tracing::{error, info, warn};

/// Time abstraction for better testing.
#[async_trait]
pub trait Timer {
    fn now() -> Instant;
    async fn sleep(duration: Duration);
}

/// This module implements supervisor for node entry points. It spawn child processes as defined in
/// given config and watch them. Stopped child (whatever reason) is respawned with exponential backoff
/// timeout. Backoff timeout is reset after child stays alive for at least `backoff_timeout_ms`.
pub struct Supervisor<T: Timer> {
    run: RunFlag,
    config: Config,
    babel_path: PathBuf,
    log_buffer: LogBuffer,
    babel_change_rx: watch::Receiver<Option<u32>>,
    phantom: PhantomData<T>,
}

impl<T: Timer> Supervisor<T> {
    pub fn new(
        run: RunFlag,
        kernel_cmdline: &str,
        mut config: Config,
        babel_path: PathBuf,
        babel_change_rx: watch::Receiver<Option<u32>>,
    ) -> eyre::Result<Self> {
        if let Some(entrypoint_params) = parse_entrypoint_params(kernel_cmdline)? {
            // we don't need to sanitize entrypoint_params since they already passed kernel cmdline args gateway
            config.entry_point.iter_mut().for_each(|item| {
                item.args
                    .iter_mut()
                    .for_each(|arg| *arg = utils::render(arg.as_str(), &entrypoint_params))
            })
        }
        let log_buffer = LogBuffer::new(config.log_buffer_capacity_ln);
        Ok(Self {
            run,
            config,
            babel_path,
            log_buffer,
            babel_change_rx,
            phantom: Default::default(),
        })
    }

    pub fn get_logs_rx(&self) -> broadcast::Receiver<String> {
        self.log_buffer.subscribe()
    }

    pub async fn run(self) {
        let mut babel_change_rx = self.babel_change_rx.clone();
        let mut run = self.run.clone();
        // if there is no babel binary yet, then just wait for babel start signal from blockvisord
        if babel_change_rx.borrow_and_update().is_none() {
            tokio::select!(
                _ = babel_change_rx.changed() => {},
                _ = run.wait() => {},
            );
        }

        self.kill_all_remnants();

        let mut futures = FuturesUnordered::new();
        for entry_point in &self.config.entry_point {
            futures.push(self.run_entrypoint(entry_point));
        }
        let entry_futures = async { while (futures.next().await).is_some() {} };
        tokio::join!(entry_futures, self.run_babel(babel_change_rx));
    }

    /// Check if there are no remnant child processes after previous run.
    /// If so, just kill them all.
    fn kill_all_remnants(&self) {
        let mut sys = System::new();
        sys.refresh_processes();
        let ps = sys.processes();
        kill_remnants(
            &self.babel_path.to_string_lossy().to_string(),
            &Default::default(),
            ps,
        );
        for entry_point in &self.config.entry_point {
            kill_remnants(&entry_point.command, &entry_point.args, ps);
        }
    }

    async fn run_babel(&self, mut babel_change_rx: watch::Receiver<Option<u32>>) {
        let mut cmd = Command::new(&self.babel_path);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut run = self.run.clone();
        let mut backoff = Backoff::<T>::new(run.clone(), &self.config);
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
        let entry_name = format!("{} {}", entrypoint.command, entrypoint.args.join(" "));
        let mut cmd = Command::new(&entrypoint.command);
        cmd.args(&entrypoint.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut run = self.run.clone();
        let mut backoff = Backoff::<T>::new(run.clone(), &self.config);
        while run.load() {
            backoff.start();
            if let Ok(mut child) = cmd.spawn() {
                info!("Spawned entrypoint '{entry_name}'");
                self.log_buffer
                    .attach(&entry_name, child.stdout.take(), child.stderr.take());
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

fn kill_remnants(cmd: &String, args: &Vec<String>, ps: &HashMap<Pid, Process>) {
    let remnants: Vec<_> = ps
        .iter()
        .filter(|(_, process)| {
            let proc_call = process.cmd();
            if let Some(proc_cmd) = proc_call.first() {
                if proc_cmd == "/bin/sh" {
                    // if first element is shell call, just ignore it and treat second as cmd, rest are arguments
                    proc_call.len() > 1 && cmd == &proc_call[1] && args == &proc_call[2..].to_vec()
                } else {
                    // first element is cmd, rest are arguments
                    cmd == proc_cmd && args == &proc_call[1..].to_vec()
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

fn parse_entrypoint_params(
    cmdline: &str,
) -> Result<Option<HashMap<String, String>>, serde_json::Error> {
    cmdline
        .split(' ')
        .find_map(|x| {
            x.strip_prefix(babel_api::BABELSUP_ENTRYPOINT_PARAMS)
                .map(serde_json::from_str::<HashMap<String, String>>)
        })
        .transpose()
}

struct Backoff<T: Timer> {
    counter: u32,
    timestamp: Instant,
    backoff_base_ms: u64,
    reset_timeout: Duration,
    run: RunFlag,
    phantom: PhantomData<T>,
}

impl<T: Timer> Backoff<T> {
    fn new(run: RunFlag, config: &Config) -> Self {
        Self {
            counter: 0,
            timestamp: Instant::now(),
            backoff_base_ms: config.backoff_base_ms,
            reset_timeout: Duration::from_millis(config.backoff_timeout_ms),
            run,
            phantom: Default::default(),
        }
    }

    fn start(&mut self) {
        self.timestamp = T::now();
    }

    async fn wait(&mut self) {
        let now = T::now();
        let duration = now.duration_since(self.timestamp);
        if duration > self.reset_timeout {
            self.counter = 0;
        } else {
            tokio::select!(
                _ = T::sleep(Duration::from_millis(self.backoff_base_ms * 2u64.pow(self.counter))) => {},
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
    use serial_test::serial;
    use std::fs;
    use std::io::Write;
    use std::ops::Add;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::time::Duration;

    mock! {
        pub TestTimer {}

        #[async_trait]
        impl Timer for TestTimer {
            fn now() -> Instant;
            async fn sleep(duration: Duration);
        }
    }

    struct TestEnv {
        tmp_root: PathBuf,
        ctrl_file: PathBuf,
        babel_path: PathBuf,
        run: RunFlag,
        babel_change_tx: watch::Sender<Option<u32>>,
        babel_change_rx: watch::Receiver<Option<u32>>,
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
        Ok(TestEnv {
            tmp_root,
            ctrl_file,
            babel_path,
            run,
            babel_change_tx,
            babel_change_rx,
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

    fn minimal_cfg() -> Config {
        Config {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            entry_point: vec![Entrypoint {
                command: "echo".to_owned(),
                args: vec!["test".to_owned()],
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_backoff_timeout_ms() -> Result<()> {
        let test_env = setup_test_env()?;
        let cfg = minimal_cfg();
        let now = Instant::now();

        let mut test_run = test_env.run.clone();
        let now_ctx = MockTestTimer::now_context();
        let sleep_ctx = MockTestTimer::sleep_context();
        now_ctx.expect().once().returning(move || now);
        now_ctx.expect().once().returning(move || now);
        now_ctx
            .expect()
            .times(3)
            .returning(move || now.add(Duration::from_millis(cfg.backoff_timeout_ms + 1)));
        sleep_ctx.expect().once().returning(move |_| {
            test_run.stop();
        });

        Supervisor::<MockTestTimer>::new(
            test_env.run,
            Default::default(),
            cfg,
            test_env.babel_path,
            test_env.babel_change_rx,
        )?
        .run()
        .await;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_exponential_backoff() -> Result<()> {
        let test_env = setup_test_env()?;
        let cfg = minimal_cfg();
        let mut test_run = test_env.run.clone();

        let now = Instant::now();

        let now_ctx = MockTestTimer::now_context();
        now_ctx.expect().once().returning(move || now);
        let sleep_ctx = MockTestTimer::sleep_context();
        const RANGE: u32 = 8;
        for n in 0..RANGE {
            now_ctx.expect().times(2).returning(move || now);
            sleep_ctx
                .expect()
                .with(predicate::eq(Duration::from_millis(10 * 2u64.pow(n))))
                .returning(|_| ());
        }
        now_ctx.expect().times(2).returning(move || now);
        sleep_ctx
            .expect()
            .once()
            .with(predicate::eq(Duration::from_millis(10 * 2u64.pow(RANGE))))
            .returning(move |_| {
                test_run.stop();
            });
        Supervisor::<MockTestTimer>::new(
            test_env.run,
            Default::default(),
            cfg,
            test_env.babel_path,
            test_env.babel_change_rx,
        )?
        .run()
        .await;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_multiple_entry_points() -> Result<()> {
        let test_env = setup_test_env()?;
        let file_path = test_env.tmp_root.join("test_multiple_entry_points");
        let cfg = Config {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            entry_point: vec![
                Entrypoint {
                    command: "sleep".to_owned(),
                    args: vec!["infinity".to_owned()],
                },
                Entrypoint {
                    command: "sleep".to_owned(),
                    args: vec!["infinity".to_owned()],
                },
                Entrypoint {
                    command: "touch".to_owned(),
                    args: vec![file_path.to_str().unwrap().to_string()],
                },
            ],
            ..Default::default()
        };
        let mut test_run = test_env.run.clone();

        let now = Instant::now();

        let now_ctx = MockTestTimer::now_context();
        now_ctx.expect().returning(move || now);
        let sleep_ctx = MockTestTimer::sleep_context();
        let first_file_path = file_path.clone();
        sleep_ctx.expect().once().returning(move |_| {
            assert!(first_file_path.exists());
            let _ = fs::remove_file(&first_file_path);
        });
        let second_file_path = file_path.clone();
        sleep_ctx.expect().once().returning(move |_| {
            assert!(second_file_path.exists());
            let _ = fs::remove_file(&second_file_path);
            test_run.stop();
        });
        let _ = fs::remove_file(&file_path);
        Supervisor::<MockTestTimer>::new(
            test_env.run,
            Default::default(),
            cfg,
            test_env.babel_path,
            test_env.babel_change_rx,
        )?
        .run()
        .await;
        assert!(!file_path.exists());
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_babel_restart() -> Result<()> {
        let test_env = setup_test_env()?;
        let cfg = Config {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            entry_point: vec![Entrypoint {
                command: "sleep".to_owned(),
                args: vec!["infinity".to_owned()],
            }],
            ..Default::default()
        };
        let mut test_run = test_env.run.clone();

        let babel_change_tx = Arc::new(test_env.babel_change_tx);
        let supervisor = Supervisor::<MockTestTimer>::new(
            test_env.run,
            Default::default(),
            cfg.clone(),
            test_env.babel_path.clone(),
            test_env.babel_change_rx,
        )?;

        let now = Instant::now();
        let now_ctx = MockTestTimer::now_context();
        let change_tx = babel_change_tx.clone();
        // expect now from run_babel
        now_ctx.expect().once().returning(move || now);
        // expect now from run_entry_point
        let control_file = test_env.ctrl_file.clone();
        now_ctx.expect().once().returning(move || {
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
        now_ctx.expect().once().returning(move || {
            test_run.stop();
            now.add(Duration::from_millis(cfg.backoff_timeout_ms + 1))
        });

        supervisor.run().await;
        assert!(test_env.ctrl_file.exists());
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn test_logs() -> Result<()> {
        let test_env = setup_test_env()?;
        let cfg = Config {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            log_buffer_capacity_ln: 10,
            entry_point: vec![Entrypoint {
                command: "echo".to_owned(),
                args: vec!["test".to_owned()],
            }],
        };
        let mut test_run = test_env.run.clone();

        let now = Instant::now();

        let now_ctx = MockTestTimer::now_context();
        now_ctx.expect().returning(move || now);
        now_ctx.expect().returning(move || now);
        let sleep_ctx = MockTestTimer::sleep_context();
        sleep_ctx.expect().times(3).returning(|_| ());
        let control_file = test_env.ctrl_file.clone();
        sleep_ctx.expect().returning(move |_| {
            test_run.stop();
            wait_for_babel(test_run.clone(), control_file.clone());
        });
        let supervisor = Supervisor::<MockTestTimer>::new(
            test_env.run,
            Default::default(),
            cfg,
            test_env.babel_path,
            test_env.babel_change_rx,
        )?;
        let mut rx = supervisor.get_logs_rx();
        supervisor.run().await;

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
    #[serial]
    async fn test_babel_only() -> Result<()> {
        let test_env = setup_test_env()?;
        let cfg = Config {
            backoff_timeout_ms: 600,
            backoff_base_ms: 10,
            ..Default::default()
        };
        let test_run = test_env.run.clone();

        let now = Instant::now();

        let supervisor = Supervisor::<MockTestTimer>::new(
            test_env.run,
            Default::default(),
            cfg,
            test_env.babel_path,
            test_env.babel_change_rx,
        )?;

        let now_ctx = MockTestTimer::now_context();
        now_ctx.expect().once().returning(move || now);
        wait_for_babel(test_run.clone(), test_env.ctrl_file.clone());
        let mut rx = supervisor.get_logs_rx();
        supervisor.run().await;

        let mut lines = Vec::default();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        assert_eq!(vec!["babel log\n"], lines);
        Ok(())
    }

    #[tokio::test]
    #[serial]
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
        kill_remnants(
            &test_env.babel_path.to_string_lossy().to_string(),
            &["a".to_string(), "b".to_string(), "c".to_string()].to_vec(),
            ps,
        );
        assert!(child.kill().await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_parse_entrypoint_params() -> Result<()> {
        assert_eq!(None, parse_entrypoint_params(r#"{"A":"valA","B":"valB"}"#)?);
        assert!(
            parse_entrypoint_params(r#"babelsup.entrypoint_params={"A":"valA","B":3}"#).is_err()
        );
        assert_eq!(
            Some(HashMap::from([
                ("A".to_string(), "valA".to_string()),
                ("B".to_string(), "valB".to_string()),
            ])),
            parse_entrypoint_params(r#"babelsup.entrypoint_params={"A":"valA","B":"valB"}"#)?
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_entrypoint_params_applied() -> Result<()> {
        let test_env = setup_test_env()?;
        let cfg = Config {
            entry_point: vec![
                Entrypoint {
                    command: "cmd1".to_owned(),
                    args: vec!["{{A}} and {{B}}".to_owned(), "only {{B}}".to_owned()],
                },
                Entrypoint {
                    command: "cmd2".to_owned(),
                    args: vec![
                        "{{B}} and {{A}} twice {{A}}".to_owned(),
                        "none{a}".to_owned(),
                    ],
                },
            ],
            ..Default::default()
        };

        assert!(Supervisor::<MockTestTimer>::new(
            test_env.run.clone(),
            r#"babelsup.entrypoint_params={InvalidJson}"#,
            cfg.clone(),
            test_env.babel_path.clone(),
            test_env.babel_change_rx.clone(),
        )
        .is_err());

        let supervisor = Supervisor::<MockTestTimer>::new(
            test_env.run,
            r#"babelsup.entrypoint_params={"A":"valA","B":"valB"}"#,
            cfg,
            test_env.babel_path,
            test_env.babel_change_rx,
        )?;
        assert_eq!(
            "valA and valB",
            supervisor
                .config
                .entry_point
                .first()
                .unwrap()
                .args
                .first()
                .unwrap()
        );
        assert_eq!(
            "only valB",
            supervisor
                .config
                .entry_point
                .first()
                .unwrap()
                .args
                .last()
                .unwrap()
        );
        assert_eq!(
            "valB and valA twice valA",
            supervisor
                .config
                .entry_point
                .last()
                .unwrap()
                .args
                .first()
                .unwrap()
        );
        assert_eq!(
            "none{a}",
            supervisor
                .config
                .entry_point
                .last()
                .unwrap()
                .args
                .last()
                .unwrap()
        );
        Ok(())
    }
}
