use std::path::Path;
use tokio::task::JoinHandle;
use tonic::transport::Channel;

/// Helper struct to gracefully shutdown and join the test server,
/// to make sure all mock asserts are checked.
pub struct TestServer {
    pub handle: JoinHandle<()>,
    pub tx: tokio::sync::oneshot::Sender<()>,
}

impl TestServer {
    pub async fn assert(self) {
        let _ = self.tx.send(());
        let _ = self.handle.await;
    }
}

#[macro_export]
macro_rules! start_test_server {
    ($tmp_root:expr, $($mock:expr), +) => {{
        let socket_path = $tmp_root.join("test_socket");
        let uds_stream =
            tokio_stream::wrappers::UnixListenerStream::new(tokio::net::UnixListener::bind(socket_path).unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        $crate::rpc::TestServer {
            tx,
            handle: tokio::spawn(async move {
                tonic::transport::server::Server::builder()
                    .max_concurrent_streams(1)
                    .$(add_service($mock)). +
                    .serve_with_incoming_shutdown(uds_stream, async {
                        rx.await.ok();
                    })
                    .await
                    .unwrap();
            }),
        }
    }};
}

pub fn test_channel(tmp_root: &Path) -> Channel {
    bv_utils::rpc::build_socket_channel(tmp_root.join("test_socket"))
}
