use crate::{config::SharedConfig, services};
use anyhow::{anyhow, bail, Result};
use reqwest::Url;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, LastWill, MqttOptions, QoS};
use std::time::Duration;
use tracing::info;

const ONLINE: &[u8] = "online".as_bytes();
const OFFLINE: &[u8] = "offline".as_bytes();

pub struct CommandsStream {
    eventloop: EventLoop,
}

impl CommandsStream {
    pub async fn connect(config: &SharedConfig) -> Result<Self> {
        services::connect(config.clone(), |config| async {
            let url = config
                .blockjoy_mqtt_url
                .ok_or_else(|| anyhow!("missing blockjoy_mqtt_url"))?
                .clone();
            let host_id = config.id;
            let token = config.token;
            // parse url into host and port
            let url = Url::parse(&url)?;
            let host = url
                .host()
                .ok_or_else(|| anyhow!("Cannot parse host from: `{url}`"))?
                .to_string();
            let port = url
                .port()
                .ok_or_else(|| anyhow!("Cannot parse port from: `{url}`"))?;
            // use host id as client id
            let mut options = MqttOptions::new(&host_id, host, port);
            options.set_keep_alive(Duration::from_secs(10));
            // use last will to notify about host going offline
            let status_topic = format!("/bv/hosts/{host_id}/status");
            options.set_last_will(LastWill {
                topic: status_topic.clone(),
                message: OFFLINE.into(),
                qos: QoS::AtLeastOnce,
                retain: true,
            });
            // use jwt as username, set empty password
            options.set_credentials(token, "");

            let client_channel_capacity = 100;
            let (client, eventloop) = AsyncClient::new(options, client_channel_capacity);

            // set status to online
            client
                .publish(status_topic, QoS::AtLeastOnce, true, ONLINE)
                .await?;

            // subscribe to host topic
            let host_topic = format!("/bv/hosts/{host_id}/#");
            client.subscribe(host_topic, QoS::AtMostOnce).await?;

            Ok(Self { eventloop })
        })
        .await
    }

    pub async fn wait_for_pending_commands(&mut self) -> Result<Option<Vec<u8>>> {
        match self.eventloop.poll().await {
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                info!("MQTT incoming topic: {}, payload: {:?}", p.topic, p.payload);
                Ok(Some(p.payload.to_vec()))
            }
            Ok(Event::Incoming(i)) => {
                info!("MQTT incoming = {i:?}");
                Ok(None)
            }
            Ok(Event::Outgoing(o)) => {
                info!("MQTT outgoing = {o:?}");
                Ok(None)
            }
            Err(e) => {
                bail!("MQTT error = {e:?}")
            }
        }
    }
}
