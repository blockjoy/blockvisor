use anyhow::{anyhow, bail, ensure, Context, Result};
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

use crate::node_connection::babelsup_pb::babel_sup_client::BabelSupClient;
use crate::{env::*, node::VSOCK_PATH};

pub mod babelsup_pb {
    tonic::include_proto!("blockjoy.babelsup.v1");
}

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
    Babel { stream: UnixStream },
    BabelSup { client: BabelSupClient<Channel> },
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
        let mut client = connect_babelsup(node_id, max_delay).await?;
        let (babel_bin, checksum) = load_babel_bin().await?;
        let babel_status = client
            .check_babel(babelsup_pb::CheckBabelRequest { checksum })
            .await?
            .into_inner()
            .babel_status;
        if babel_status != babelsup_pb::check_babel_response::BabelStatus::Ok as i32 {
            info!("Invalid or missing Babel service on VM, installing new one");
            client
                .start_new_babel(tokio_stream::iter(babel_bin))
                .await?;
        }
        Ok(Self {
            node_id,
            state: NodeConnectionState::BabelSup { client },
        })
    }

    /// This function combines the capabilities from `write_data` and `read_data` to allow you to
    /// send some request and then obtain a response back.
    /// It reconnect to babel if necessary.
    pub async fn babel_rpc<S: serde::ser::Serialize, D: serde::de::DeserializeOwned>(
        &mut self,
        request: S,
    ) -> Result<D> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("Cannot change port to babel: node connection is closed")
            }
            NodeConnectionState::Babel { .. } => {}
            NodeConnectionState::BabelSup { .. } => {
                debug!("Reconnecting to babel");
                self.state = NodeConnectionState::Babel {
                    stream: open_stream(self.node_id, BABEL_VSOCK_PORT, CONNECTION_SWITCH_TIMEOUT)
                        .await?,
                }
            }
        }
        let stream = self.try_babel_stream_mut()?;
        write_data(stream, request).await?;
        read_data(stream).await
    }

    /// This function gets gRPC client connected to babelsup. It reconnect to babelsup if necessary.
    pub async fn babelsup_client(&mut self) -> Result<&mut BabelSupClient<Channel>> {
        match &mut self.state {
            NodeConnectionState::Closed => {
                bail!("Cannot change port to babelsup: node connection is closed")
            }
            NodeConnectionState::Babel { stream } => {
                stream.shutdown().await?;
                debug!("Reconnecting to babelsup");
                self.state = NodeConnectionState::BabelSup {
                    client: connect_babelsup(self.node_id, CONNECTION_SWITCH_TIMEOUT).await?,
                };
            }
            NodeConnectionState::BabelSup { .. } => {}
        };
        self.try_babelsup_client_mut()
    }

    /// Tries to return the babel unix stream, and if there isn't one, returns an error message.
    fn try_babel_stream_mut(&mut self) -> Result<&mut UnixStream> {
        match &mut self.state {
            NodeConnectionState::Babel { stream } => Ok(stream),
            _ => Err(anyhow!(
                "Tried to get babel connection stream while there isn't one"
            )),
        }
    }

    /// Tries to return the babelsup client, and if there isn't one, returns an error message.
    fn try_babelsup_client_mut(&mut self) -> Result<&mut BabelSupClient<Channel>> {
        match &mut self.state {
            NodeConnectionState::BabelSup { client } => Ok(client),
            _ => Err(anyhow!(
                "Tried to get babelsup client while there isn't one"
            )),
        }
    }
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

/// Waits for the socket to become writable, then writes the data as json to the socket. The max
/// time that is allowed to elapse  is `SOCKET_TIMEOUT`.
async fn write_data(unix_stream: &mut UnixStream, data: impl serde::Serialize) -> Result<()> {
    use std::io::ErrorKind::WouldBlock;

    let data = serde_json::to_string(&data)?;
    let write_data = async {
        loop {
            // Wait for the socket to become ready to write to.
            unix_stream.writable().await?;
            // Try to write data, this may still fail with `WouldBlock` if the readiness event
            // is a false positive.
            match unix_stream.try_write(data.as_bytes()) {
                Ok(_) => break,
                Err(e) if e.kind() == WouldBlock => continue,
                Err(e) => bail!("Writing socket failed with `{e}`"),
            }
        }
        Ok(())
    };
    timeout(SOCKET_TIMEOUT, write_data).await?
}

/// Waits for the socket to become readable, then reads data from it. The max time that is
/// allowed to elapse (per read) is `SOCKET_TIMEOUT`. When data is sent over the vsock, this
/// data is parsed as json and returned as the requested type.
async fn read_data<D: serde::de::DeserializeOwned>(unix_stream: &mut UnixStream) -> Result<D> {
    use std::io::ErrorKind::WouldBlock;
    let read_data = async {
        loop {
            // Wait for the socket to become ready to read from.
            unix_stream.readable().await?;
            let mut data = vec![0; 16384];

            // Try to read data, this may still fail with `WouldBlock`
            // if the readiness event is a false positive.
            match unix_stream.try_read(&mut data) {
                Ok(n) => {
                    data.resize(n, 0);
                    let s = std::str::from_utf8(&data)?;
                    return Ok(serde_json::from_str(s)?);
                }
                Err(e) if e.kind() == WouldBlock => continue,
                Err(e) => bail!("Writing socket failed with `{e}`"),
            }
        }
    };
    timeout(SOCKET_TIMEOUT, read_data).await?
}

async fn connect_babelsup(
    node_id: uuid::Uuid,
    max_delay: Duration,
) -> Result<BabelSupClient<Channel>> {
    let channel = Endpoint::try_from("http://[::]:50052")
        .unwrap()
        .timeout(GRPC_REQUEST_TIMEOUT)
        .connect_timeout(GRPC_CONNECT_TIMEOUT)
        .connect_with_connector_lazy(tower::service_fn(move |_: Uri| async move {
            open_stream(node_id, BABEL_SUP_VSOCK_PORT, max_delay).await
        }));
    Ok(BabelSupClient::new(channel))
}

pub async fn load_babel_bin() -> Result<(Vec<babelsup_pb::StartNewBabelRequest>, u32)> {
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
    let mut babel_bin = Vec::<babelsup_pb::StartNewBabelRequest>::default();
    while let Ok(size) = reader.read(&mut buf[..]).await {
        if size == 0 {
            break;
        }
        digest.update(&buf[0..size]);
        babel_bin.push(babelsup_pb::StartNewBabelRequest {
            babel_bin: Some(babelsup_pb::start_new_babel_request::BabelBin::Bin(
                buf[0..size].to_vec(),
            )),
        });
    }
    let checksum = digest.finalize();
    babel_bin.push(babelsup_pb::StartNewBabelRequest {
        babel_bin: Some(babelsup_pb::start_new_babel_request::BabelBin::Checksum(
            checksum,
        )),
    });
    Ok((babel_bin, checksum))
}
