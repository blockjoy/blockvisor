use blockvisord::services::api::pb;
use std::{pin::Pin, sync::Arc};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

type ResponseResult<T> = Result<Response<T>, Status>;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<pb::Command, Status>> + Send>>;

pub struct StubServer {
    pub commands: Arc<Mutex<Vec<pb::Command>>>,
    pub updates_tx: mpsc::Sender<pb::InfoUpdate>,
    pub shutdown_tx: mpsc::Sender<()>,
}

pub struct StubHostsServer {}

#[tonic::async_trait]
impl pb::hosts_server::Hosts for StubHostsServer {
    async fn provision(
        &self,
        request: Request<pb::ProvisionHostRequest>,
    ) -> Result<Response<pb::ProvisionHostResponse>, Status> {
        let host = request.into_inner();
        if host.otp != "UNKNOWN" {
            let host_id = "497d13b1-ddbe-4ee7-bfc7-752c7b710afe".to_string();

            let reply = pb::ProvisionHostResponse {
                host_id,
                token: "awesome-token".to_owned(),
                messages: vec![],
                origin_request_id: host.request_id,
            };

            Ok(Response::new(reply))
        } else {
            Err(Status::permission_denied("Invalid token"))
        }
    }

    async fn delete(
        &self,
        request: Request<pb::DeleteHostRequest>,
    ) -> Result<Response<pb::DeleteHostResponse>, Status> {
        let host = request.into_inner();

        let reply = pb::DeleteHostResponse {
            messages: vec![],
            origin_request_id: host.request_id,
        };

        Ok(Response::new(reply))
    }

    async fn info_update(
        &self,
        request: Request<pb::HostInfoUpdateRequest>,
    ) -> Result<Response<pb::HostInfoUpdateResponse>, Status> {
        let host = request.into_inner();

        let reply = pb::HostInfoUpdateResponse {
            messages: vec![],
            origin_request_id: host.request_id,
        };

        Ok(Response::new(reply))
    }
}

#[tonic::async_trait]
impl pb::command_flow_server::CommandFlow for StubServer {
    type CommandsStream = ResponseStream;

    async fn commands(
        &self,
        req: Request<Streaming<pb::InfoUpdate>>,
    ) -> ResponseResult<Self::CommandsStream> {
        let mut in_stream = req.into_inner();
        let (tx, rx) = mpsc::channel(128);

        for command in self.commands.lock().await.iter() {
            tx.send(Ok(command.clone())).await.unwrap();
        }
        self.commands.lock().await.clear();

        let shutdown_tx = self.shutdown_tx.clone();
        let updates_tx = self.updates_tx.clone();
        tokio::spawn(async move {
            while let Some(result) = in_stream.next().await {
                match result {
                    Ok(v) => {
                        println!("server: get update {:?}", &v);
                        updates_tx.send(v).await.unwrap();
                    }
                    Err(err) => {
                        eprintln!("server: error {:?}", err);
                        break;
                    }
                }
            }
            println!("server: stream ended");
            shutdown_tx.send(()).await.unwrap();
        });

        let out_stream = ReceiverStream::new(rx);

        Ok(Response::new(Box::pin(out_stream) as Self::CommandsStream))
    }
}
