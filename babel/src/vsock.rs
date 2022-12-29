use crate::run_flag::RunFlag;
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_vsock::VsockStream;

#[async_trait]
pub trait Handler {
    async fn handle(&self, message: &str) -> eyre::Result<Vec<u8>>;
}

/// This function tries to read messages from the vsocket and keeps responding to those messages.
/// Each opened connection gets handled separately by a tokio task and then the listener starts
/// listening for new messages. This means that we do not need to care if blockvisor shuts down or
/// restarts.
pub async fn serve(
    mut run: RunFlag,
    cid: u32,
    port: u32,
    handler: impl Handler + Send + Sync + 'static,
) -> eyre::Result<()> {
    let handler = Arc::new(handler);
    tracing::debug!("Binding to virtual socket...");
    let listener = tokio_vsock::VsockListener::bind(cid, port)?;
    tracing::debug!("Bound");
    let mut incoming = listener.incoming();
    tracing::debug!("Receiving incoming messages");
    while run.load() {
        tokio::select!(
            res = incoming.next() => {
                if let Some(res) = res {
                    match res {
                        Ok(stream) => {
                            tracing::debug!("Stream opened, delegating to handler");
                            tokio::spawn(serve_stream(stream, handler.clone()));
                        }
                        Err(_) => {
                            tracing::warn!("Receiving streams failed. Aborting server");
                            run.stop();
                        }
                    }
                }
            },
            _ = run.wait() => {},
        )
    }
    Ok(())
}

async fn serve_stream(mut stream: VsockStream, handler: Arc<impl Handler>) {
    loop {
        let mut buf = vec![0u8; 16384];
        let len = match stream.read(&mut buf).await {
            Ok(len) => len,
            // If we cannot await new data from the stream anymore we end the task.
            Err(_) => break,
        };
        if len == 0 {
            tracing::info!("Vsock stream closed. Shutting down connection handler");
            break;
        }
        buf.resize(len, 0);
        let msg = match std::str::from_utf8(&buf) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("Ingoring non-utf8 message: {e}");
                continue;
            }
        };
        if let Err(e) = handle_message(msg, &mut stream, &handler).await {
            tracing::warn!("Failed to handle message: {e}");
        }
    }
}

async fn handle_message(
    message: &str,
    stream: &mut tokio_vsock::VsockStream,
    handler: &Arc<impl Handler>,
) -> eyre::Result<()> {
    tracing::debug!("Received message: `{message}`");
    let response = handler.handle(message).await?;
    tracing::debug!("Sending response: {response:?}");
    stream.write_all(&response).await?;
    Ok(())
}
