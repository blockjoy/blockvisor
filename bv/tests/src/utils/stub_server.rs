use blockvisord::services::api::pb::ServicesResponse;
use blockvisord::services::api::pb::{self, UpdateNodeResponse};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

type Result<T> = std::result::Result<Response<T>, Status>;

pub struct StubHostsServer {}

#[tonic::async_trait]
impl pb::hosts_server::Hosts for StubHostsServer {
    async fn provision(
        &self,
        request: Request<pb::ProvisionHostRequest>,
    ) -> Result<pb::ProvisionHostResponse> {
        let host = request.into_inner();
        if host.otp != "UNKNOWN" {
            let host_id = "497d13b1-ddbe-4ee7-bfc7-752c7b710afe".to_string();

            let reply = pb::ProvisionHostResponse {
                host_id,
                token: "awesome-token".to_owned(),
            };

            Ok(Response::new(reply))
        } else {
            Err(Status::permission_denied("Invalid token"))
        }
    }

    async fn update(&self, _: Request<pb::UpdateHostRequest>) -> Result<pb::UpdateHostResponse> {
        let reply = pb::UpdateHostResponse {};

        Ok(Response::new(reply))
    }

    async fn delete(&self, _: Request<pb::DeleteHostRequest>) -> Result<pb::DeleteHostResponse> {
        let reply = pb::DeleteHostResponse {};

        Ok(Response::new(reply))
    }

    async fn get(&self, _: Request<pb::GetHostRequest>) -> Result<pb::GetHostResponse> {
        unimplemented!("Sod off I'm just a test server")
    }

    async fn list(&self, _: Request<pb::ListHostsRequest>) -> Result<pb::ListHostsResponse> {
        unimplemented!("Sod off I'm just a test server")
    }

    async fn create(&self, _: Request<pb::CreateHostRequest>) -> Result<pb::CreateHostResponse> {
        unimplemented!("Sod off I'm just a test server")
    }
}

pub struct StubNodesServer {
    pub updates: Arc<Mutex<Vec<pb::node::ContainerStatus>>>,
}

#[tonic::async_trait]
impl pb::nodes_server::Nodes for StubNodesServer {
    async fn update(
        &self,
        request: Request<pb::UpdateNodeRequest>,
    ) -> Result<pb::UpdateNodeResponse> {
        let req = request.into_inner();
        let status = req.container_status.unwrap();
        self.updates
            .lock()
            .await
            .push(pb::node::ContainerStatus::from_i32(status).unwrap());
        Ok(Response::new(UpdateNodeResponse {}))
    }

    async fn get(&self, _: Request<pb::GetNodeRequest>) -> Result<pb::GetNodeResponse> {
        unimplemented!("Sod off I'm just a test server")
    }

    async fn list(&self, _: Request<pb::ListNodesRequest>) -> Result<pb::ListNodesResponse> {
        unimplemented!("Sod off I'm just a test server")
    }

    async fn create(&self, _: Request<pb::CreateNodeRequest>) -> Result<pb::CreateNodeResponse> {
        unimplemented!("Sod off I'm just a test server")
    }

    async fn delete(&self, _: Request<pb::DeleteNodeRequest>) -> Result<pb::DeleteNodeResponse> {
        unimplemented!("Sod off I'm just a test server")
    }
}

pub struct StubCommandsServer {
    pub commands: Arc<Mutex<Vec<pb::Command>>>,
    pub updates: Arc<Mutex<Vec<pb::UpdateCommandRequest>>>,
}

#[tonic::async_trait]
impl pb::commands_server::Commands for StubCommandsServer {
    async fn pending(
        &self,
        _: Request<pb::PendingCommandsRequest>,
    ) -> Result<pb::PendingCommandsResponse> {
        let reply = pb::PendingCommandsResponse {
            commands: std::mem::take(&mut *self.commands.lock().await),
        };

        Ok(Response::new(reply))
    }

    async fn get(&self, _: Request<pb::GetCommandRequest>) -> Result<pb::GetCommandResponse> {
        unimplemented!()
    }

    async fn create(
        &self,
        _: Request<pb::CreateCommandRequest>,
    ) -> Result<pb::CreateCommandResponse> {
        unimplemented!()
    }

    async fn update(
        &self,
        request: Request<pb::UpdateCommandRequest>,
    ) -> Result<pb::UpdateCommandResponse> {
        let req = request.into_inner();
        self.updates.lock().await.push(req);
        let resp = pb::UpdateCommandResponse { command: None }; // tests lol
        Ok(Response::new(resp))
    }
}

pub struct StubDiscoveryService;

#[tonic::async_trait]
impl pb::discovery_server::Discovery for StubDiscoveryService {
    async fn services(&self, _: Request<pb::ServicesRequest>) -> Result<pb::ServicesResponse> {
        Ok(Response::new(ServicesResponse {
            key_service_url: "key_service_url".to_string(),
            registry_url: "registry_url".to_string(),
            notification_url: "notification_url".to_string(),
        }))
    }
}
