use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::SendError;
use tokio::task::JoinHandle;
use tracing::warn;

/// This struct implements logs buffer that gather `stdout` and `stderr` from entry_points
/// and store them in circular buffer. Internally `tokio::broadcast` is used as a circular buffer
/// since it has all required properties out of the box.  
/// See tokio::broadcast for more details.
pub struct LogBuffer {
    tx: broadcast::Sender<String>,
    rx: broadcast::Receiver<String>,
}

impl LogBuffer {
    /// Create new LogBuffer with given capacity.
    /// NOTE: According to `tokio::broadcast` implementation capacity is rounded up to next power of 2.
    pub fn new(mut capacity: usize) -> Self {
        if capacity == 0 {
            capacity = 1; // tokio panic if it's 0, but we don't
        }
        let (tx, rx) = broadcast::channel(capacity);
        Self { tx, rx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.rx.resubscribe()
    }

    pub fn send(&self, msg: String) -> Result<usize, SendError<String>> {
        self.tx.send(msg)
    }

    pub fn attach<T, U>(
        &self,
        entry_name: &str,
        stdout: Option<T>,
        stderr: Option<U>,
    ) -> JoinHandle<()>
    where
        T: AsyncRead + Send + Unpin + 'static,
        U: AsyncRead + Send + Unpin + 'static,
    {
        let stdout_task = self.attach_stream(stdout);
        let stderr_task = self.attach_stream(stderr);
        let entry_name = entry_name.to_string();
        tokio::spawn(async move {
            match (stdout_task, stderr_task) {
                (None, Some(err)) => {
                    warn!("Missing stdout for '{entry_name}'");
                    let _ = err.await;
                }
                (Some(out), None) => {
                    warn!("Missing stderr for '{entry_name}'");
                    let _ = out.await;
                }
                (Some(out), Some(err)) => {
                    let _ = tokio::join!(out, err);
                }
                (None, None) => {
                    warn!("Missing stdout and stderr for '{entry_name}'");
                }
            }
        })
    }

    fn attach_stream<T: AsyncRead + Send + Unpin + 'static>(
        &self,
        stream: Option<T>,
    ) -> Option<JoinHandle<()>> {
        stream.map(|stream| {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                loop {
                    let mut line = String::default();
                    match reader.read_line(&mut line).await {
                        Err(err) => {
                            warn!("Invalid stream {err}");
                            break;
                        }
                        Ok(0) => break,
                        Ok(_) => {
                            let _ = tx.send(line);
                        }
                    }
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error;
    use tokio_util::io::StreamReader;

    #[tokio::test]
    async fn test_no_logs() {
        let log_buffer = LogBuffer::new(5);
        let mut rx = log_buffer.subscribe();
        log_buffer
            .attach::<tokio::io::Empty, tokio::io::Empty>("name1", None, None)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_all_logs() {
        let log_buffer = LogBuffer::new(5);

        let stdout_stream = tokio_stream::iter(vec![
            tokio::io::Result::Ok("one\n".as_bytes()),
            tokio::io::Result::Ok("two\n".as_bytes()),
            tokio::io::Result::Ok("three\n".as_bytes()),
        ]);
        let stdout = StreamReader::new(stdout_stream);
        let stderr_stream = tokio_stream::iter(vec![
            tokio::io::Result::Ok("err1\n".as_bytes()),
            tokio::io::Result::Ok("err2\n".as_bytes()),
            tokio::io::Result::Ok("err3\n".as_bytes()),
        ]);
        let stderr = StreamReader::new(stderr_stream);
        let mut rx = log_buffer.subscribe();
        log_buffer
            .attach("name1", Some(stdout), Some(stderr))
            .await
            .unwrap();
        let mut lines = Vec::default();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        assert_eq!(
            vec!["one\n", "two\n", "three\n", "err1\n", "err2\n", "err3\n"],
            lines
        );
    }

    #[tokio::test]
    async fn test_logs_overflow() {
        let log_buffer = LogBuffer::new(3);

        let stdout_stream = tokio_stream::iter(vec![
            tokio::io::Result::Ok("one\n".as_bytes()),
            tokio::io::Result::Ok("two\n".as_bytes()),
            tokio::io::Result::Ok("three\n".as_bytes()),
            tokio::io::Result::Ok("four\n".as_bytes()),
            tokio::io::Result::Ok("five\n".as_bytes()),
        ]);
        let stdout = StreamReader::new(stdout_stream);
        let mut rx = log_buffer.subscribe();
        log_buffer
            .attach::<_, tokio::io::Empty>("name1", Some(stdout), None)
            .await
            .unwrap();
        let mut lines = Vec::default();
        loop {
            match rx.try_recv() {
                Ok(line) => lines.push(line),
                Err(error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        // four items expected since capacity is rounded up to next power of 2
        assert_eq!(vec!["two\n", "three\n", "four\n", "five\n"], lines);
    }
}
