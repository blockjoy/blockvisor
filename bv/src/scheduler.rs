use crate::node::Node;
use crate::node_state::NodeStatus;
use crate::pal::Pal;
use async_trait::async_trait;
use bv_utils::run_flag::RunFlag;
use chrono::DateTime;
use cron::Schedule;
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::min;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::str::FromStr;
use std::sync::Arc;
use tokio::select;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, warn};
use uuid::Uuid;

type TZ = chrono::Local;

#[async_trait]
pub trait TaskHandler {
    async fn handle(&self, task: &Scheduled);
}

pub struct NodeTaskHandler<P: Pal>(pub Arc<RwLock<HashMap<Uuid, RwLock<Node<P>>>>>);

#[async_trait]
impl<P> TaskHandler for NodeTaskHandler<P>
where
    P: Pal + Send + Sync + Debug + 'static,
    P::NodeConnection: Send + Sync,
    P::ApiServiceConnector: Send + Sync,
    P::VirtualMachine: Send + Sync,
    P::RecoveryBackoff: Send + Sync + 'static,
{
    async fn handle(&self, task: &Scheduled) {
        let nodes_lock = self.0.read().await;
        if let Some(node) = nodes_lock.get(&task.node_id) {
            let mut node_lock = node.write().await;
            if node_lock.status().await == NodeStatus::Running {
                match &task.task {
                    Task::PluginFnCall { name, param } => {
                        debug!("calling scheduled plugin function '{name}({param})'");
                        if let Err(err) = node_lock.babel_engine.call_method(name, param).await {
                            warn!("scheduled function '{name}({param}' failed with: {err:#}");
                        };
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Task {
    PluginFnCall { name: String, param: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scheduled {
    pub node_id: Uuid,
    pub name: String,
    #[serde(
        deserialize_with = "from_cron_string",
        serialize_with = "to_crone_string"
    )]
    pub schedule: Schedule,
    pub task: Task,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Add(Scheduled),
    Delete(String),
    DeleteNode(Uuid),
}

fn to_crone_string<S>(s: &Schedule, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&s.to_string())
}

fn from_cron_string<'de, D>(deserializer: D) -> Result<Schedule, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(deserializer)?;
    Schedule::from_str(s).map_err(Error::custom)
}

pub struct Scheduler {
    handle: tokio::task::JoinHandle<Vec<Scheduled>>,
    run: RunFlag,
    tx: mpsc::Sender<Action>,
}

impl Debug for Scheduler {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scheduler")
    }
}

async fn worker(
    mut run: RunFlag,
    mut rx: mpsc::Receiver<Action>,
    handler: impl TaskHandler,
    mut tasks: Vec<(DateTime<TZ>, Scheduled)>,
) -> Vec<Scheduled> {
    while run.load() {
        let now = TZ::now();
        let mut ttn = None; // TimeToNext
        let mut remaining_tasks = Vec::<(DateTime<TZ>, Scheduled)>::default();
        for (timestamp, task) in &mut tasks.into_iter() {
            for next in task.schedule.after(&timestamp) {
                if let Some(new_ttn) = (next - now).to_std().ok().and_then(|delta| {
                    if delta.is_zero() {
                        None
                    } else {
                        Some(delta)
                    }
                }) {
                    // next > now
                    if let Some(old_ttn) = ttn.take() {
                        ttn = Some(min(new_ttn, old_ttn));
                    } else {
                        ttn = Some(new_ttn);
                    }
                    remaining_tasks.push((now, task));
                    break;
                } else {
                    handler.handle(&task).await;
                }
            }
        }
        tasks = remaining_tasks;
        if let Some(ttn) = ttn {
            select!(
                action = rx.recv() => {
                    if let Some(action) = action {
                        handle_action(&mut tasks, action);
                    }
                }
                _ = tokio::time::sleep(ttn) => {}
                _ = run.wait() => {}
            );
        } else if let Some(action) = run.select(rx.recv()).await.flatten() {
            handle_action(&mut tasks, action);
        }
    }
    tasks.into_iter().map(|(_, task)| task).collect()
}

fn handle_action(tasks: &mut Vec<(DateTime<TZ>, Scheduled)>, action: Action) {
    match action {
        Action::Add(task) => tasks.push((TZ::now(), task)),
        Action::Delete(name) => tasks.retain(|(_, task)| task.name != name),
        Action::DeleteNode(id) => tasks.retain(|(_, task)| task.node_id != id),
    };
}

impl Scheduler {
    pub fn start(tasks: &[Scheduled], handler: impl TaskHandler + Sync + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel(16);
        let mut run = RunFlag::default();
        let handle = tokio::spawn(worker(
            run.clone(),
            rx,
            handler,
            tasks.iter().map(|task| (TZ::now(), task.clone())).collect(),
        ));
        Self { handle, run, tx }
    }

    pub async fn stop(mut self) -> eyre::Result<Vec<Scheduled>> {
        self.run.stop();
        Ok(self.handle.await?)
    }

    pub fn tx(&self) -> mpsc::Sender<Action> {
        self.tx.clone()
    }
}
