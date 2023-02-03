use crate::{env::*, node::VSOCK_PATH, with_retry};
use anyhow::{anyhow, bail, ensure, Context, Result};
use babel_api::babel_client::BabelClient;
use babel_api::babel_sup_client::BabelSupClient;
use std::env;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::BufReader;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::{sleep, timeout},
};
use tokio_stream;
use tonic::transport::{Channel, Endpoint, Uri};
use tracing::{debug, error, info};
use uuid::Uuid;

pub const BABEL_SUP_VSOCK_PORT: u32 = 41;
pub const BABEL_VSOCK_PORT: u32 = 42;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const GRPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const GRPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECTION_SWITCH_TIMEOUT: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum NodeConnectionState {
    Closed,
    Babel(BabelClient<Channel>),
    BabelSup(BabelSupClient<Channel>),
}

#[derive(Debug)]
pub struct NodeConnection {
    node_id: uuid::Uuid,
    state: NodeConnectionState,
}

impl NodeConnection {
    pub fn closed(node_id: uuid::Uuid) -> Self {
        Self {
            node_id,
            state: NodeConnectionState::Closed,
        }
    }

    /// Tries to open a new connection to the VM. Note that this fails if the VM hasn't started yet.
    /// It also initializes that connection by sending the opening message. Therefore, if this
    /// function succeeds the connection is guaranteed to be writeable at the moment of returning.
    pub async fn try_open(node_id: uuid::Uuid, max_delay: Duration) -> Result<Self> {
        let mut client = connect_babelsup(node_id, max_delay).await;
        let babelsup_version = with_retry!(client.get_version(()))?.into_inner();
        info!("Connected to babelsup {babelsup_version}");
        let (babel_bin, checksum) = load_babel_bin().await?;
        let babel_status = with_retry!(client.check_babel(checksum))?.into_inner();
        if babel_status != babel_api::BabelStatus::Ok {
            info!("Invalid or missing Babel service on VM, installing new one");
            with_retry!(client.start_new_babel(tokio_stream::iter(babel_bin.clone())))?;
        }
        Ok(Self {
            node_id,
            state: NodeConnectionState::BabelSup(client),
        })
    }

    /// This function gets gRPC client connected to babel. It reconnect to babelsup if necessary.
    pub async fn babel_client(&mut self) -> Result<&mut BabelClient<Channel>> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("Cannot change port to babel: node connection is closed")
            }
            NodeConnectionState::Babel { .. } => {}
            NodeConnectionState::BabelSup { .. } => {
                debug!("Reconnecting to babel");
                self.state = NodeConnectionState::Babel(BabelClient::new(
                    create_channel(self.node_id, BABEL_VSOCK_PORT, CONNECTION_SWITCH_TIMEOUT).await,
                ));
            }
        };
        if let NodeConnectionState::Babel(client) = &mut self.state {
            Ok(client)
        } else {
            unreachable!()
        }
    }

    /// This function gets gRPC client connected to babelsup. It reconnect to babelsup if necessary.
    pub async fn babelsup_client(&mut self) -> Result<&mut BabelSupClient<Channel>> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("Cannot change port to babelsup: node connection is closed")
            }
            NodeConnectionState::Babel { .. } => {
                debug!("Reconnecting to babelsup");
                self.state = NodeConnectionState::BabelSup(
                    connect_babelsup(self.node_id, CONNECTION_SWITCH_TIMEOUT).await,
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

async fn connect_babelsup(node_id: Uuid, max_delay: Duration) -> BabelSupClient<Channel> {
    BabelSupClient::new(create_channel(node_id, BABEL_SUP_VSOCK_PORT, max_delay).await)
}

async fn open_stream(node_id: uuid::Uuid, port: u32, max_delay: Duration) -> Result<UnixStream> {
    // We are going to connect to the central socket for this VM. Later we will specify which
    // port we want to talk to.
    let socket = format!(
        "{}/firecracker/{node_id}/root{VSOCK_PATH}",
        CHROOT_PATH.to_string_lossy()
    );
    debug!("Connecting to node at `{socket}`");

    // We need to implement retrying when reading from the socket, as it may take a little bit
    // of time for the socket file to get created on disk, because this is done asynchronously
    // by Firecracker.
    let start = std::time::Instant::now();
    let elapsed = || std::time::Instant::now() - start;
    let stream = loop {
        match UnixStream::connect(&socket).await {
            // Also it's possible that we will not manage to
            // initiate connection from the first attempt
            Ok(mut stream) => match handshake(&mut stream, port).await {
                Ok(_) => break Ok(stream),
                Err(e) if elapsed() < max_delay => {
                    debug!(
                        "Handshake error, retrying in {} seconds: {}",
                        RETRY_INTERVAL.as_secs(),
                        e
                    );
                    sleep(RETRY_INTERVAL).await;
                }
                Err(e) => break Err(anyhow!("handshake error {e}")),
            },
            Err(e) if elapsed() < max_delay => {
                debug!(
                    "No socket file yet, retrying in {} seconds: {}",
                    RETRY_INTERVAL.as_secs(),
                    e
                );
                sleep(RETRY_INTERVAL).await;
            }
            Err(e) => break Err(anyhow!("uds connect error {e}")),
        };
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
                error!("Socket responded to open message with empty message :(");
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

async fn create_channel(node_id: uuid::Uuid, vsock_port: u32, max_delay: Duration) -> Channel {
    Endpoint::from_static("http://[::]:50052")
        .timeout(GRPC_REQUEST_TIMEOUT)
        .connect_timeout(GRPC_CONNECT_TIMEOUT)
        .connect_with_connector_lazy(tower::service_fn(move |_: Uri| async move {
            open_stream(node_id, vsock_port, max_delay).await
        }))
}

pub async fn load_babel_bin() -> Result<(Vec<babel_api::BabelBin>, u32)> {
    let babel_path =
        fs::canonicalize(env::current_exe().with_context(|| "failed to get current binary path")?)
            .await
            .with_context(|| "non canonical current binary path")?
            .parent()
            .with_context(|| "invalid current binary dir - has no parent")?
            .join("../../babel/bin/babel");
    let file = File::open(&babel_path).await.with_context(|| {
        format!(
            "failed to load babel binary {}",
            babel_path.to_string_lossy()
        )
    })?;
    let mut reader = BufReader::new(file);
    let mut buf = [0; 16384];
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);
    let mut digest = crc.digest();
    let mut babel_bin = Vec::<babel_api::BabelBin>::default();
    while let Ok(size) = reader.read(&mut buf[..]).await {
        if size == 0 {
            break;
        }
        digest.update(&buf[0..size]);
        babel_bin.push(babel_api::BabelBin::Bin(buf[0..size].to_vec()));
    }
    let checksum = digest.finalize();
    babel_bin.push(babel_api::BabelBin::Checksum(checksum));
    Ok((babel_bin, checksum))
}
