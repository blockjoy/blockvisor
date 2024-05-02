use blockvisord::services::api::pb;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

type Result<T> = std::result::Result<Response<T>, Status>;

pub struct StubHostsServer {}

#[allow(clippy::diverging_sub_expression)]
#[tonic::async_trait]
impl pb::host_service_server::HostService for StubHostsServer {
    async fn update(
        &self,
        _: Request<pb::HostServiceUpdateRequest>,
    ) -> Result<pb::HostServiceUpdateResponse> {
        let reply = pb::HostServiceUpdateResponse {};

        Ok(Response::new(reply))
    }

    async fn delete(
        &self,
        _: Request<pb::HostServiceDeleteRequest>,
    ) -> Result<pb::HostServiceDeleteResponse> {
        let reply = pb::HostServiceDeleteResponse {};

        Ok(Response::new(reply))
    }

    async fn get(
        &self,
        _: Request<pb::HostServiceGetRequest>,
    ) -> Result<pb::HostServiceGetResponse> {
        unimplemented!("Sod off I'm just a test server");
    }

    async fn list(
        &self,
        _: Request<pb::HostServiceListRequest>,
    ) -> Result<pb::HostServiceListResponse> {
        unimplemented!("Sod off I'm just a test server");
    }

    async fn create(
        &self,
        _: Request<pb::HostServiceCreateRequest>,
    ) -> Result<pb::HostServiceCreateResponse> {
        let reply = pb::HostServiceCreateResponse {
            host: Some(pb::Host {
                id: "497d13b1-ddbe-4ee7-bfc7-752c7b710afe".to_string(),
                name: "hostname".to_string(),
                version: "1.0".to_string(),
                cpu_count: 1,
                mem_size_bytes: 1,
                disk_size_bytes: 1,
                os: "os".to_string(),
                os_version: "20.0".to_string(),
                ip: "1.1.1.1".to_string(),
                created_at: None,
                ip_gateway: "1.1.1.2".to_string(),
                org_id: "org".to_string(),
                org_name: "ORG".to_string(),
                node_count: 1,
                region: Some("europe-bosnia-number-1".to_string()),
                billing_amount: None,
                vmm_mountpoint: Some("/var/lib/blockvisor".to_string()),
                ip_addresses: vec![
                    pb::HostIpAddress {
                        ip: "1.1.1.3".to_string(),
                        assigned: false,
                    },
                    pb::HostIpAddress {
                        ip: "1.1.1.4".to_string(),
                        assigned: false,
                    },
                ],
                managed_by: pb::ManagedBy::Automatic.into(),
            }),
            token: "awesome-token".to_string(),
            refresh: "even-more-awesomer-token".to_string(),
        };

        Ok(Response::new(reply))
    }

    async fn start(
        &self,
        _request: Request<pb::HostServiceStartRequest>,
    ) -> Result<pb::HostServiceStartResponse> {
        unimplemented!()
    }

    async fn stop(
        &self,
        _request: Request<pb::HostServiceStopRequest>,
    ) -> Result<pb::HostServiceStopResponse> {
        unimplemented!()
    }

    async fn restart(
        &self,
        _request: Request<pb::HostServiceRestartRequest>,
    ) -> Result<pb::HostServiceRestartResponse> {
        unimplemented!()
    }

    async fn regions(
        &self,
        _request: Request<pb::HostServiceRegionsRequest>,
    ) -> Result<pb::HostServiceRegionsResponse> {
        unimplemented!()
    }
}

pub struct StubCommandsServer {
    pub commands: Arc<Mutex<Vec<Vec<pb::Command>>>>,
    pub updates: Arc<Mutex<Vec<pb::CommandServiceUpdateRequest>>>,
}

#[tonic::async_trait]
impl pb::command_service_server::CommandService for StubCommandsServer {
    async fn list(
        &self,
        _request: Request<pb::CommandServiceListRequest>,
    ) -> Result<pb::CommandServiceListResponse> {
        Ok(Response::new(pb::CommandServiceListResponse {
            commands: vec![],
        }))
    }

    async fn update(
        &self,
        request: Request<pb::CommandServiceUpdateRequest>,
    ) -> Result<pb::CommandServiceUpdateResponse> {
        let req = request.into_inner();
        self.updates.lock().await.push(req);
        let resp = pb::CommandServiceUpdateResponse { command: None }; // tests lol
        Ok(Response::new(resp))
    }

    async fn ack(
        &self,
        _request: Request<pb::CommandServiceAckRequest>,
    ) -> Result<pb::CommandServiceAckResponse> {
        Ok(Response::new(pb::CommandServiceAckResponse {}))
    }

    async fn pending(
        &self,
        _: Request<pb::CommandServicePendingRequest>,
    ) -> Result<pb::CommandServicePendingResponse> {
        let reply = pb::CommandServicePendingResponse {
            commands: self.commands.lock().await.pop().unwrap_or_default(),
        };

        Ok(Response::new(reply))
    }
}

pub struct StubDiscoveryService;

#[tonic::async_trait]
impl pb::discovery_service_server::DiscoveryService for StubDiscoveryService {
    async fn services(
        &self,
        _: Request<pb::DiscoveryServiceServicesRequest>,
    ) -> Result<pb::DiscoveryServiceServicesResponse> {
        Ok(Response::new(pb::DiscoveryServiceServicesResponse {
            notification_url: "notification_url".to_string(),
        }))
    }
}
