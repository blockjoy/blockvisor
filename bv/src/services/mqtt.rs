use crate::{bv_config::SharedConfig, pal, services, services::api::common};
use async_trait::async_trait;
use eyre::{anyhow, bail, Result};
use metrics::{counter, Counter};
use prost::Message;
use reqwest::Url;
use rumqttc::{
    v5::{
        mqttbytes::v5::LastWill,
        mqttbytes::QoS,
        {AsyncClient, Event, EventLoop, Incoming, MqttOptions},
    },
    Transport,
};
use std::time::Duration;
use tracing::{debug, info};

lazy_static::lazy_static! {
    pub static ref MQTT_POLL_COUNTER: Counter = counter!("mqtt.connection.poll");
    pub static ref MQTT_NOTIFY_COUNTER: Counter = counter!("mqtt.connection.notification");
    pub static ref MQTT_ERROR_COUNTER: Counter = counter!("mqtt.connection.error");
}

pub struct MqttConnector {
    pub config: SharedConfig,
}

pub struct MqttStream {
    event_loop: EventLoop,
    config: SharedConfig,
}

#[async_trait]
impl pal::ServiceConnector<MqttStream> for MqttConnector {
    async fn connect(&self) -> Result<MqttStream> {
        services::connect_with_discovery(&self.config, |config| async {
            let token = config.token().await?;
            let config = config.read().await;
            let url = config
                .blockjoy_mqtt_url
                .ok_or_else(|| anyhow!("missing blockjoy_mqtt_url"))?
                .clone();
            let host_id = config.id;
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
            let offline = host_offline(host_id.clone());
            let status_topic = format!("/bv/hosts/{host_id}/status");
            options.set_last_will(LastWill {
                topic: status_topic.clone().into(),
                message: offline.into(),
                qos: QoS::AtLeastOnce,
                retain: true,
                properties: None,
            });
            // use jwt as username, set empty password
            options.set_credentials(token.0, "");
            if port == 8883 {
                // set ssl cert options in case of tls/ssl connection
                let mut root_certificates = rumqttc::tokio_rustls::rustls::RootCertStore::empty();
                let certificates = rustls_native_certs::load_native_certs().certs;
                for cert in certificates {
                    root_certificates.add(cert)?;
                }
                let client_config = rumqttc::tokio_rustls::rustls::ClientConfig::builder()
                    .with_root_certificates(root_certificates)
                    .with_no_client_auth();

                options.set_transport(Transport::tls_with_config(client_config.into()));
            }

            let client_channel_capacity = 100;
            let (client, eventloop) = AsyncClient::new(options, client_channel_capacity);

            // set status to online
            let online = host_online(host_id.clone());
            client
                .publish(status_topic, QoS::AtLeastOnce, true, online)
                .await?;

            // subscribe to host commands topic
            let host_topic = format!("/hosts/{host_id}/commands");
            client.subscribe(host_topic, QoS::AtLeastOnce).await?;

            Ok(MqttStream {
                event_loop: eventloop,
                config: self.config.clone(),
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
                let topic = String::from_utf8(p.topic.into())?;
                info!(
                    "MQTT incoming topic: {topic}, payload len: {}",
                    p.payload.len()
                );
                Ok(Some(p.payload.into()))
            }
            Ok(Event::Incoming(i)) => {
                debug!("MQTT incoming = {i:?}");
                Ok(None)
            }
            Ok(Event::Outgoing(o)) => {
                debug!("MQTT outgoing = {o:?}");
                Ok(None)
            }
            Err(e) => {
                // in case of connection error reset mqtt url, so it will be rediscovered on next connect
                self.config.set_mqtt_url(None).await;
                bail!("MQTT error = {e:#}")
            }
        }
    }
}

fn host_online(host_id: String) -> Vec<u8> {
    common::HostStatus {
        host_id,
        connection_status: Some(common::HostConnectionStatus::Online.into()),
    }
    .encode_to_vec()
}

fn host_offline(host_id: String) -> Vec<u8> {
    common::HostStatus {
        host_id,
        connection_status: Some(common::HostConnectionStatus::Offline.into()),
    }
    .encode_to_vec()
}
