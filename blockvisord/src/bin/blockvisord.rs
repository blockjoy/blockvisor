use anyhow::Result;
use blockvisord::{
    client::{APIClient, CommandStatusUpdate},
    config::Config,
    containers::Containers,
    hosts::dummy_apply_config,
    logging::setup_logging,
};
use tokio::time::{sleep, Duration};
use tracing::{info, Level};

#[allow(unreachable_code)]
#[tokio::main]
async fn main() -> Result<()> {
    setup_logging(Level::INFO)?;
    info!("Starting...");

    let config = Config::load().await?;
    loop {
        let containers = Containers::load().await?;

        let vmm = std::env::var("VMM").unwrap_or_else(|_| "dummy".into());
        if vmm == "dummy" {
            dummy_apply_config(&containers).await?;
        }

        process_pending_commands(&config).await?;

        sleep(Duration::from_secs(5)).await;
    }

    info!("Stopping...");
    Ok(())
}

async fn process_pending_commands(config: &Config) -> Result<()> {
    let timeout = Duration::from_secs(10);
    let client = APIClient::new(&config.blockjoy_api_url, timeout)?;

    info!("Getting pending commands for host: {}", &config.id);
    for command in client
        .get_pending_commands(&config.token, &config.id)
        .await?
    {
        info!("Processing command: {}", &command.cmd);

        let update = CommandStatusUpdate {
            response: "Done".to_string(),
            exit_status: 0,
        };

        client
            .update_command_status(&config.token, &command.id, &update)
            .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use blockvisord::client::{APIClient, CommandStatusUpdate};
    use chrono::{TimeZone, Utc};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn test_get_pending_commands() {
        let server = MockServer::start();

        let token = "TOKEN";
        let host_id = "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b";

        let m = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/hosts/{}/commands/pending", host_id))
                .header("Content-Type", "application/json")
                .header("authorization", format!("Bearer {}", token));
            then.status(200)
                .header("Content-Type", "application/json")
                .json_body(json!([
                  {
                    "id": "497f6eca-6276-4993-bfeb-53cbbbba6f08",
                    "host_id": host_id,
                    "cmd": "restart_miner",
                    "created_at": "2019-08-24T14:15:22Z",
                  }
                ]));
        });

        let client = APIClient::new(&server.base_url(), Duration::from_secs(10)).unwrap();
        let resp = client.get_pending_commands(token, host_id).await.unwrap();

        assert_eq!(resp.len(), 1);
        assert_eq!(resp[0].id, "497f6eca-6276-4993-bfeb-53cbbbba6f08");
        assert_eq!(resp[0].host_id, host_id);
        assert_eq!(resp[0].cmd, "restart_miner");
        assert_eq!(resp[0].sub_cmd, None);
        assert_eq!(resp[0].response, None);
        assert_eq!(resp[0].exit_status, None);
        assert_eq!(resp[0].created_at, Utc.ymd(2019, 8, 24).and_hms(14, 15, 22));
        assert_eq!(resp[0].completed_at, None);

        m.assert();
    }

    #[tokio::test]
    async fn test_update_command_status() {
        let server = MockServer::start();

        let token = "TOKEN";
        let command_id = "497f6eca-6276-4993-bfeb-53cbbbba6f08";

        let m = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/commands/{}/response", command_id))
                .header("Content-Type", "application/json")
                .header("authorization", format!("Bearer {}", token))
                .json_body(json!({
                    "response": "restarted",
                    "exit_status": 0_i32,
                }));
            then.status(200)
                .header("Content-Type", "application/json")
                .json_body(json!(
                  {
                    "id": command_id,
                    "host_id": "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b",
                    "cmd": "restart_miner",
                    "response": "restarted",
                    "exit_status": 0_i32,
                    "created_at": "2019-08-24T14:15:22Z",
                    "completed_at": "2020-08-24T14:15:22Z",
                  }
                ));
        });

        let client = APIClient::new(&server.base_url(), Duration::from_secs(10)).unwrap();
        let update = CommandStatusUpdate {
            response: "restarted".to_string(),
            exit_status: 0,
        };
        let resp = client
            .update_command_status(token, command_id, &update)
            .await
            .unwrap();

        assert_eq!(resp.id, command_id);
        assert_eq!(resp.host_id, "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b");
        assert_eq!(resp.cmd, "restart_miner");
        assert_eq!(resp.sub_cmd, None);
        assert_eq!(resp.response, Some("restarted".to_string()));
        assert_eq!(resp.exit_status, Some(0_i32));
        assert_eq!(resp.created_at, Utc.ymd(2019, 8, 24).and_hms(14, 15, 22));
        assert_eq!(
            resp.completed_at,
            Some(Utc.ymd(2020, 8, 24).and_hms(14, 15, 22))
        );

        m.assert();
    }
}
