use blockvisord::services::api::pb;
use blockvisord::services::api::pb::ServicesResponse;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

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
}

pub struct StubNodesServer {
    pub updates: Arc<Mutex<Vec<pb::node_info::ContainerStatus>>>,
}

#[tonic::async_trait]
impl pb::nodes_server::Nodes for StubNodesServer {
    async fn info_update(
        &self,
        request: Request<pb::NodeInfoUpdateRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let status = req.info.unwrap().container_status.unwrap();
        self.updates
            .lock()
            .await
            .push(pb::node_info::ContainerStatus::from_i32(status).unwrap());
        Ok(Response::new(()))
    }
}

pub struct StubCommandsServer {
    pub commands: Arc<Mutex<Vec<pb::Command>>>,
    pub updates: Arc<Mutex<Vec<pb::CommandInfo>>>,
}

#[tonic::async_trait]
impl pb::commands_server::Commands for StubCommandsServer {
    async fn pending(
        &self,
        _request: Request<pb::PendingCommandsRequest>,
    ) -> Result<Response<pb::CommandResponse>, Status> {
        let reply = pb::CommandResponse {
            commands: self.commands.lock().await.to_vec(),
        };
        self.commands.lock().await.clear();

        Ok(Response::new(reply))
    }

    async fn get(
        &self,
        _request: Request<pb::CommandInfo>,
    ) -> Result<Response<pb::Command>, Status> {
        unimplemented!()
    }

    async fn update(&self, request: Request<pb::CommandInfo>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.updates.lock().await.push(req);

        Ok(Response::new(()))
    }
}

pub struct StubDiscoveryService;

#[tonic::async_trait]
impl pb::discovery_server::Discovery for StubDiscoveryService {
    async fn services(&self, _: Request<()>) -> Result<Response<ServicesResponse>, Status> {
        Ok(Response::new(ServicesResponse {
            key_service_url: "key_service_url".to_string(),
            registry_url: "registry_url".to_string(),
            notification_url: "notification_url".to_string(),
        }))
    }
}
