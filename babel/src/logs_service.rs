use tokio::sync::{broadcast, oneshot};
use tonic::{Request, Response, Status};

pub struct LogsService {
    logs_tx: broadcast::Sender<String>,
}

impl LogsService {
    pub async fn wait_for_logs_tx(
        logs_rx: oneshot::Receiver<broadcast::Sender<String>>,
    ) -> Option<Self> {
        if let Ok(logs_tx) = logs_rx.await {
            Some(Self { logs_tx })
        } else {
            None
        }
    }
}

#[tonic::async_trait]
impl babel_api::logs_collector_server::LogsCollector for LogsService {
    async fn send_log(&self, request: Request<String>) -> Result<Response<()>, Status> {
        let log = request.into_inner();
        let _ = self.logs_tx.send(log);
        Ok(Response::new(()))
    }
}
