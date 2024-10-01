use crate::{
    bib_cli::{ImageCommand, ProtocolCommand},
    services::{self, ApiServiceConnector},
};
use std::time::Duration;

const BV_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BV_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn process_image_command(
    connector: impl ApiServiceConnector + Clone,
    command: ImageCommand,
) -> eyre::Result<()> {
    match command {
        ImageCommand::Create { .. } => {}
        ImageCommand::Push { .. } => {}
    }
    Ok(())
}

pub async fn process_protocol_command(
    connector: impl ApiServiceConnector + Clone,
    command: ProtocolCommand,
) -> eyre::Result<()> {
    match command {
        ProtocolCommand::List => {
            let mut client = services::protocol::ProtocolService::new(connector).await?;
            let protocols = client.list_protocols().await?;
            for protocol in protocols {
                println!(
                    "{} ({}) - {}",
                    protocol.name,
                    protocol.key,
                    protocol.description.unwrap_or_default()
                );
            }
        }
        ProtocolCommand::Push { .. } => {
            // TODO MJR
        }
    }
    Ok(())
}
