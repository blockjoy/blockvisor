use crate::{config::SharedConfig, pal, services};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use metrics::{register_counter, Counter};
use reqwest::Url;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, LastWill, MqttOptions, QoS};
use std::time::Duration;
use tracing::info;

const ONLINE: &[u8] = "online".as_bytes();
const OFFLINE: &[u8] = "offline".as_bytes();

lazy_static::lazy_static! {
    pub static ref MQTT_POLL_COUNTER: Counter = register_counter!("mqtt.connection.poll");
    pub static ref MQTT_NOTIFY_COUNTER: Counter = register_counter!("mqtt.connection.notification");
    pub static ref MQTT_ERROR_COUNTER: Counter = register_counter!("mqtt.connection.error");
}

pub struct MqttConnector {
    pub config: SharedConfig,
}

pub struct MqttStream {
    event_loop: EventLoop,
}

#[async_trait]
impl pal::ServiceConnector<MqttStream> for MqttConnector {
    async fn connect(&self) -> Result<MqttStream> {
        services::connect(self.config.clone(), |config| async {
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
            options.set_keep_alive(Duration::from_secs(30));
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

            Ok(MqttStream {
                event_loop: eventloop,
            })
        })
        .await
    }
}

#[async_trait]
impl pal::CommandsStream for MqttStream {
    async fn wait_for_pending_commands(&mut self) -> Result<Option<Vec<u8>>> {
        MQTT_POLL_COUNTER.increment(1);
        match self.event_loop.poll().await {
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
