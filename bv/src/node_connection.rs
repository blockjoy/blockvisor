use crate::{node::VSOCK_PATH, with_retry};
use anyhow::{anyhow, bail, ensure, Context, Result};
use async_trait::async_trait;
use babel_api::babel::babel_client::BabelClient;
use babel_api::babelsup::babel_sup_client::BabelSupClient;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    time::{sleep, timeout},
};
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{debug, info};
use uuid::Uuid;

pub const BABEL_SUP_VSOCK_PORT: u32 = 41;
pub const BABEL_VSOCK_PORT: u32 = 42;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECTION_SWITCH_TIMEOUT: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Abstract Babel connection for better testing.
#[async_trait]
pub trait BabelConnection {
    /// This function gets RPC client connected to babel.
    async fn babel_client(&mut self) -> Result<&mut BabelClient<Channel>>;
    /// Mark connection as broken, which meant it will be reestablished on next `babel_client` call.
    fn mark_broken(&mut self);
}

#[derive(Debug)]
pub enum NodeConnectionState {
    Closed,
    Broken,
    Babel(BabelClient<Channel>),
    BabelSup(BabelSupClient<Channel>),
}

#[derive(Debug)]
pub struct NodeConnection {
    socket_path: PathBuf,
    state: NodeConnectionState,
}

impl NodeConnection {
    pub fn closed(chroot_path: &Path, node_id: Uuid) -> Self {
        Self {
            socket_path: build_socket_path(chroot_path, node_id),
            state: NodeConnectionState::Closed,
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, NodeConnectionState::Closed)
    }

    pub fn is_broken(&self) -> bool {
        matches!(self.state, NodeConnectionState::Broken)
    }

    /// Tries to open a new connection to the VM. Note that this fails if the VM hasn't started yet.
    /// It also initializes that connection by sending the opening message. Therefore, if this
    /// function succeeds the connection is guaranteed to be writeable at the moment of returning.
    pub async fn try_open(chroot_path: &Path, node_id: Uuid, max_delay: Duration) -> Result<Self> {
        let socket_path = build_socket_path(chroot_path, node_id);
        let mut client = connect_babelsup(&socket_path, max_delay).await?;
        let babelsup_version = with_retry!(client.get_version(()))?.into_inner();
        info!("Connected to babelsup {babelsup_version}");
        Ok(Self {
            socket_path,
            state: NodeConnectionState::BabelSup(client),
        })
    }

    pub async fn connection_test(&self) -> Result<()> {
        let mut client = connect_babelsup(&self.socket_path, CONNECTION_SWITCH_TIMEOUT).await?;
        with_retry!(client.get_version(()))?;
        Ok(())
    }

    /// This function gets gRPC client connected to babelsup. It reconnect to babelsup if necessary.
    pub async fn babelsup_client(&mut self) -> Result<&mut BabelSupClient<Channel>> {
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
}
#[async_trait]
impl BabelConnection for NodeConnection {
    /// This function gets RPC client connected to babel. It reconnect to babel if necessary.
    async fn babel_client(&mut self) -> Result<&mut BabelClient<Channel>> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("Cannot change port to babel: node connection is closed")
            }
            NodeConnectionState::Babel { .. } => {}
            NodeConnectionState::BabelSup { .. } | NodeConnectionState::Broken => {
                debug!("Reconnecting to babel");
                self.state = NodeConnectionState::Babel(BabelClient::new(
                    create_channel(
                        &self.socket_path,
                        BABEL_VSOCK_PORT,
                        CONNECTION_SWITCH_TIMEOUT,
                    )
                    .await?,
                ));
            }
        };
        if let NodeConnectionState::Babel(client) = &mut self.state {
            Ok(client)
        } else {
            unreachable!()
        }
    }

    /// Mark connection as broken, which meant it will be reestablished on next `*_client` call.
    fn mark_broken(&mut self) {
        self.state = NodeConnectionState::Broken;
    }
}

fn build_socket_path(chroot_path: &Path, node_id: Uuid) -> PathBuf {
    chroot_path
        .join("firecracker")
        .join(node_id.to_string())
        .join("root")
        .join(VSOCK_PATH)
}
async fn connect_babelsup(
    socket_path: &Path,
    max_delay: Duration,
) -> Result<BabelSupClient<Channel>> {
    Ok(BabelSupClient::new(
        create_channel(socket_path, BABEL_SUP_VSOCK_PORT, max_delay).await?,
    ))
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
                        "Handshake error, retrying in {} seconds: {}",
                        RETRY_INTERVAL.as_secs(),
                        e
                    );
                }
                Err(e) => break Err(anyhow!("handshake error {e}")),
            },
            Err(e) if start.elapsed() < max_delay => {
                debug!(
                    "No socket file yet, retrying in {} seconds: {}",
                    RETRY_INTERVAL.as_secs(),
                    e
                );
            }
            Err(e) => break Err(anyhow!("uds connect error {e}")),
        };
        sleep(Ord::min(RETRY_INTERVAL, max_delay - start.elapsed())).await;
    }
    .context("Failed to connect to node bus")?;

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
                info!("Socket responded to open message with empty message :(");
                bail!("Socket responded to open message with empty message :(");
            }
            Ok(n) => {
                let sock_opened_msg = std::str::from_utf8(&sock_opened_buf[..n]).unwrap();
                let msg_valid = sock_opened_msg.starts_with("OK ");
                ensure!(msg_valid, "Invalid opening message for new socket");
                break;
            }
            // Ignore false-positive readable events
            Err(e) if e.kind() == WouldBlock => continue,
            Err(e) => bail!("Establishing socket failed with `{e}`"),
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
        .timeout(RPC_REQUEST_TIMEOUT)
        .connect_timeout(RPC_CONNECT_TIMEOUT)
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let socket_path = socket_path.clone();
            async move { open_stream(socket_path, vsock_port, max_delay).await }
        }))
        .await?)
}
