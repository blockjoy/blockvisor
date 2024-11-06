use blockvisord::services::api::pb;
use tonic::{Request, Response, Status};

type Result<T> = std::result::Result<Response<T>, Status>;

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
