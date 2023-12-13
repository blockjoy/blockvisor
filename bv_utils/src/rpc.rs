use eyre::{bail, Context, ContextCompat};
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Request;

pub const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Extract grpc request timeout value from "grpc-timeout" header. See [tonic::Request::set_timeout](https://docs.rs/tonic/latest/tonic/struct.Request.html#method.set_timeout).
/// Units are translated according to [gRPC Spec](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests).
pub fn extract_grpc_timeout<T>(req: &Request<T>) -> eyre::Result<Duration> {
    let timeout = req
        .metadata()
        .get("grpc-timeout")
        .with_context(|| "grpc-timeout header not set")?
        .to_str()?;
    if !timeout.is_empty() {
        let (value, unit) = timeout.split_at(timeout.len() - 1);
        let value = value
            .parse::<u64>()
            .with_context(|| format!("invalid timeout value: {value}"))?;
        match unit {
            "H" => Ok(Duration::from_secs(value * 3600)),
            "M" => Ok(Duration::from_secs(value * 60)),
            "S" => Ok(Duration::from_secs(value)),
            "m" => Ok(Duration::from_millis(value)),
            "u" => Ok(Duration::from_micros(value)),
            "n" => Ok(Duration::from_nanos(value)),
            _ => bail!("invalid unit: {unit}"),
        }
    } else {
        bail!("empty timeout value")
    }
}

pub fn build_socket_channel(socket_path: impl AsRef<Path>) -> Channel {
    let socket_path = socket_path.as_ref().to_path_buf();
    Endpoint::from_static("http://[::]:50052")
        .timeout(RPC_REQUEST_TIMEOUT)
        .connect_timeout(RPC_CONNECT_TIMEOUT)
        .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
            UnixStream::connect(socket_path.clone())
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_extract_timeout() -> eyre::Result<()> {
        let mut req = Request::new("text".to_string());
        assert_eq!(
            "grpc-timeout header not set",
            extract_grpc_timeout(&req).unwrap_err().to_string()
        );
        req.set_timeout(Duration::from_secs(7));
        assert_eq!(Duration::from_secs(7), extract_grpc_timeout(&req)?);
        req.set_timeout(Duration::from_nanos(77));
        assert_eq!(Duration::from_nanos(77), extract_grpc_timeout(&req)?);
        Ok(())
    }
}
