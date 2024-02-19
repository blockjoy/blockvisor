use crate::{firecracker_machine::VSOCK_PATH, pal};
use async_trait::async_trait;
use bv_utils::with_retry;
use eyre::{anyhow, bail, ensure, Context, Result};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    time::{sleep, timeout},
};
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{debug, info};

pub const BABEL_SUP_VSOCK_PORT: u32 = 41;
pub const BABEL_VSOCK_PORT: u32 = 42;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECTION_SWITCH_TIMEOUT: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum NodeConnectionState {
    Closed,
    Broken,
    Babel(pal::BabelClient),
    BabelSup(pal::BabelSupClient),
}

#[derive(Debug)]
pub struct NodeConnection {
    socket_path: PathBuf,
    state: NodeConnectionState,
}

/// Creates new closed connection instance.
pub fn new(vm_data_path: &Path) -> NodeConnection {
    NodeConnection {
        socket_path: vm_data_path.join(VSOCK_PATH),
        state: NodeConnectionState::Closed,
    }
}

#[async_trait]
impl pal::NodeConnection for NodeConnection {
    /// Tries to open a connection to the VM. Note that this fails if the VM hasn't started yet.
    /// It also initializes that connection by sending the opening message. Therefore, if this
    /// function succeeds the connection is guaranteed to be writeable at the moment of returning.
    async fn open(&mut self, max_delay: Duration) -> Result<()> {
        let mut client = connect_babelsup(&self.socket_path, max_delay).await?;
        let babelsup_version = with_retry!(client.get_version(()))?.into_inner();
        info!("Connected to babelsup {babelsup_version}");
        self.state = NodeConnectionState::BabelSup(client);
        Ok(())
    }

    fn close(&mut self) {
        self.state = NodeConnectionState::Closed;
    }

    fn is_closed(&self) -> bool {
        matches!(self.state, NodeConnectionState::Closed)
    }

    /// Mark connection as broken, which meant it will be reestablished on next `*_client` call.
    fn mark_broken(&mut self) {
        self.state = NodeConnectionState::Broken;
    }

    fn is_broken(&self) -> bool {
        matches!(self.state, NodeConnectionState::Broken)
    }

    async fn test(&mut self) -> Result<()> {
        let mut client = connect_babelsup(&self.socket_path, CONNECTION_SWITCH_TIMEOUT).await?;
        with_retry!(client.get_version(()))?;
        // update connection state (otherwise it still may be seen as broken)
        self.state = NodeConnectionState::BabelSup(client);
        Ok(())
    }

    /// This function gets gRPC client connected to babelsup. It reconnects to babelsup if necessary.
    async fn babelsup_client(&mut self) -> Result<&mut pal::BabelSupClient> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("Cannot change port to babelsup: node connection is closed")
            }
            NodeConnectionState::Babel { .. } | NodeConnectionState::Broken => {
                debug!("Reconnecting to babelsup");
                self.state = NodeConnectionState::BabelSup(
                    connect_babelsup(&self.socket_path, CONNECTION_SWITCH_TIMEOUT).await?,
                );
            }
            NodeConnectionState::BabelSup { .. } => {}
        };
        if let NodeConnectionState::BabelSup(client) = &mut self.state {
            Ok(client)
        } else {
            unreachable!()
        }
    }

    /// This function gets RPC client connected to babel. It reconnects to babel if necessary.
    async fn babel_client(&mut self) -> Result<&mut pal::BabelClient> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("Cannot change port to babel: node connection is closed")
            }
            NodeConnectionState::Babel { .. } => {}
            NodeConnectionState::BabelSup { .. } | NodeConnectionState::Broken => {
                debug!("Reconnecting to babel");
                self.state = NodeConnectionState::Babel(
                    babel_api::babel::babel_client::BabelClient::with_interceptor(
                        create_channel(
                            &self.socket_path,
                            BABEL_VSOCK_PORT,
                            CONNECTION_SWITCH_TIMEOUT,
                        )
                        .await?,
                        bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
                    ),
                );
            }
        };
        if let NodeConnectionState::Babel(client) = &mut self.state {
            Ok(client)
        } else {
            unreachable!()
        }
    }
}

async fn connect_babelsup(socket_path: &Path, max_delay: Duration) -> Result<pal::BabelSupClient> {
    Ok(
        babel_api::babelsup::babel_sup_client::BabelSupClient::with_interceptor(
            create_channel(socket_path, BABEL_SUP_VSOCK_PORT, max_delay).await?,
            bv_utils::rpc::DefaultTimeout(RPC_REQUEST_TIMEOUT),
        ),
    )
}

async fn open_stream(socket_path: PathBuf, port: u32, max_delay: Duration) -> Result<UnixStream> {
    // We are going to connect to the central socket for this VM. Later we will specify which
    // port we want to talk to.
    debug!("Connecting to node at `{}`", socket_path.display());

    // We need to implement retrying when reading from the socket, as it may take a little bit
    // of time for the socket file to get created on disk, because this is done asynchronously
    // by Firecracker.
    let start = std::time::Instant::now();
    let stream = loop {
        match UnixStream::connect(&socket_path).await {
            // Also it's possible that we will not manage to
            // initiate connection from the first attempt
            Ok(mut stream) => match handshake(&mut stream, port).await {
                Ok(_) => break Ok(stream),
                Err(e) if start.elapsed() < max_delay => {
                    debug!(
                        "Handshake error, retrying in {} seconds: {:#}",
                        RETRY_INTERVAL.as_secs(),
                        e
                    );
                }
                Err(e) => break Err(anyhow!("handshake error: {e:#}")),
            },
            Err(e) if start.elapsed() < max_delay => {
                debug!(
                    "No socket file yet, retrying in {} seconds: {:#}",
                    RETRY_INTERVAL.as_secs(),
                    e
                );
            }
            Err(e) => break Err(anyhow!("uds connect error {e:#}")),
        };
        sleep(Ord::min(RETRY_INTERVAL, max_delay - start.elapsed())).await;
    }
    .with_context(|| "Failed to connect to node bus")?;

    Ok(stream)
}

/// Execute connection handshake procedure
///
/// For host initiated connections we have to send a message to the guess socket that
/// contains the port number. As a response we expect a message of the format `Ok <num>\n`.
/// We check this by asserting that the first message received starts with `OK`. Popping
/// this message here prevents us from having to check for the opening message elsewhere
/// were we expect the response to be valid json.
async fn handshake(stream: &mut UnixStream, port: u32) -> Result<()> {
    use std::io::ErrorKind::WouldBlock;

    let open_message = format!("CONNECT {port}\n");
    debug!("Sending open message: `{open_message:?}`");
    timeout(SOCKET_TIMEOUT, stream.write(open_message.as_bytes())).await??;
    debug!("Sent open message");
    let mut sock_opened_buf = [0; 20];
    loop {
        stream.readable().await?;
        match stream.try_read(&mut sock_opened_buf) {
            Ok(0) => {
                debug!("Socket responded to open message with empty message");
                bail!("Socket responded to open message with empty message");
            }
            Ok(n) => {
                let sock_opened_msg = std::str::from_utf8(&sock_opened_buf[..n]).unwrap();
                let msg_valid = sock_opened_msg.starts_with("OK ");
                ensure!(msg_valid, "Invalid opening message for new socket");
                break;
            }
            // Ignore false-positive readable events
            Err(e) if e.kind() == WouldBlock => continue,
            Err(e) => bail!("Establishing socket failed with `{e:#}`"),
        }
    }
    Ok(())
}

async fn create_channel(
    socket_path: &Path,
    vsock_port: u32,
    max_delay: Duration,
) -> Result<Channel> {
    let socket_path = socket_path.to_owned();
    Ok(Endpoint::from_static("http://[::]:50052")
        .connect_timeout(RPC_CONNECT_TIMEOUT)
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let socket_path = socket_path.clone();
            async move { open_stream(socket_path, vsock_port, max_delay).await }
        }))
        .await?)
}
