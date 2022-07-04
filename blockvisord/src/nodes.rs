use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::fs::{self, read_dir};
use tracing::{debug, info, instrument, warn};
use zbus::export::futures_util::TryFutureExt;
use zbus::{dbus_interface, fdo};

use crate::{
    network_interface::NetworkInterface,
    node::Node,
    node_data::{NodeData, NodeState},
};

const NODES_CONFIG_FILENAME: &str = "nodes.toml";

lazy_static::lazy_static! {
    pub static ref REGISTRY_CONFIG_DIR: PathBuf = home::home_dir()
        .map(|p| p.join(".cache"))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("blockvisor");
}
lazy_static::lazy_static! {
    static ref REGISTRY_CONFIG_FILE: PathBuf = REGISTRY_CONFIG_DIR.join(NODES_CONFIG_FILENAME);
}

#[derive(Clone, Debug)]
pub enum ServiceStatus {
    Enabled,
    Disabled,
}

#[derive(Debug, Default)]
pub struct Nodes {
    pub nodes: HashMap<String, Node>,
    data: CommonData,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
pub struct CommonData {
    machine_index: Arc<Mutex<u32>>,
}

#[dbus_interface(interface = "com.BlockJoy.blockvisor.Node")]
impl Nodes {
    #[instrument(skip(self))]
    async fn create(&mut self, id: String, chain: String) -> fdo::Result<()> {
        let network_interface = self.next_network_interface();
        let node = NodeData {
            id: id.clone(),
            chain,
            state: NodeState::Stopped,
            network_interface,
        };

        let node = Node::create(node)
            .await
            .map_err(|e| fdo::Error::IOError(e.to_string()))?;
        debug!("Container with id `{}` created", id);
        self.nodes.insert(id, node);

        fdo::Result::Ok(())
    }

    #[instrument(skip(self))]
    async fn delete(&mut self, id: &str) -> fdo::Result<()> {
        let node = self.nodes.remove(id).ok_or_else(|| {
            let msg = format!("Container with id {} not found", id);
            fdo::Error::FileNotFound(msg)
        })?;
        node.delete()
            .await
            .map_err(|e| fdo::Error::IOError(e.to_string()))?;
        debug!("deleted");

        fdo::Result::Ok(())
    }

    #[instrument(skip(self))]
    async fn start(&mut self, id: &str) -> fdo::Result<()> {
        let node = self.nodes.get_mut(id).ok_or_else(|| {
            let msg = format!("Container with id {} not found", id);
            fdo::Error::FileNotFound(msg)
        })?;
        debug!("found node");
        node.start()
            .await
            .map_err(|e| fdo::Error::IOError(e.to_string()))?;
        debug!("started");

        fdo::Result::Ok(())
    }

    #[instrument(skip(self))]
    async fn stop(&mut self, id: &str) -> fdo::Result<()> {
        let node = self.nodes.get_mut(id).ok_or_else(|| {
            let msg = format!("Container with id {} not found", id);
            fdo::Error::FileNotFound(msg)
        })?;
        debug!("found node");
        node.kill()
            .await
            .map_err(|e| fdo::Error::IOError(e.to_string()))?;
        debug!("stopped");

        fdo::Result::Ok(())
    }

    #[instrument(skip(self))]
    async fn list(&self) -> Vec<NodeData> {
        debug!("listing {} nodes", self.nodes.len());

        self.nodes.values().map(|n| n.data.clone()).collect()
    }

    // TODO: Rest of the NodeCommand variants.
}

impl Nodes {
    pub async fn load() -> Result<Nodes> {
        // First load the common data file.
        info!(
            "Reading nodes common config file: {}",
            REGISTRY_CONFIG_FILE.display()
        );
        let config = fs::read_to_string(&*REGISTRY_CONFIG_FILE).await?;
        let nodes_data = toml::from_str(&config)?;

        // Now the individual node data files.
        info!(
            "Reading nodes config dir: {}",
            REGISTRY_CONFIG_DIR.display()
        );
        let mut this = Nodes {
            nodes: HashMap::new(),
            data: nodes_data,
        };
        let mut dir = read_dir(&*REGISTRY_CONFIG_DIR).await?;
        while let Some(entry) = dir.next_entry().await? {
            // blockvisord should not bail on problems with individual node files.
            // It should log warnings though.
            let path = entry.path();
            if path == *REGISTRY_CONFIG_FILE {
                // Skip the common data file.
                continue;
            }
            match NodeData::load(&*path).and_then(Node::connect).await {
                Ok(node) => {
                    this.nodes.insert(node.data.id.clone(), node);
                }
                Err(e) => warn!("Failed to read node file `{}`: {}", path.display(), e),
            }
        }

        Ok(this)
    }

    pub async fn save(&self) -> Result<()> {
        // We only save the common data file. The individual node data files save themselves.
        info!(
            "Writing nodes common config file: {}",
            REGISTRY_CONFIG_FILE.display()
        );
        let config = toml::Value::try_from(&self.data)?;
        let config = toml::to_string(&config)?;
        fs::create_dir_all(REGISTRY_CONFIG_DIR.as_path()).await?;
        fs::write(&*REGISTRY_CONFIG_FILE, &*config).await?;

        Ok(())
    }

    pub fn exists() -> bool {
        Path::new(&*REGISTRY_CONFIG_FILE).exists()
    }

    /// Get the next machine index and increment it.
    pub fn next_network_interface(&self) -> NetworkInterface {
        let mut machine_index = self.data.machine_index.lock().expect("lock poisoned");

        let idx_bytes = machine_index.to_be_bytes();
        let iface = NetworkInterface {
            name: format!("bv{}", *machine_index),
            // FIXME: Hardcoding address for now.
            ip: IpAddr::V4(Ipv4Addr::new(
                idx_bytes[0] + 74,
                idx_bytes[1] + 50,
                idx_bytes[2] + 82,
                idx_bytes[3] + 83,
            )),
        };
        *machine_index += 1;

        iface
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn network_interface_gen() {
        let nodes = super::Nodes::default();
        let iface = nodes.next_network_interface();
        assert_eq!(iface.name, "bv0");
        assert_eq!(
            iface.ip,
            super::IpAddr::V4(super::Ipv4Addr::new(74, 50, 82, 83))
        );

        let iface = nodes.next_network_interface();
        assert_eq!(iface.name, "bv1");
        assert_eq!(
            iface.ip,
            super::IpAddr::V4(super::Ipv4Addr::new(74, 50, 82, 84))
        );

        // Let's take the machine_index beyond u8 boundry.
        *nodes.data.machine_index.lock().expect("lock poisoned") = u8::MAX as u32 + 1;
        let iface = nodes.next_network_interface();
        assert_eq!(iface.name, format!("bv{}", u8::MAX as u32 + 1));
        assert_eq!(
            iface.ip,
            super::IpAddr::V4(super::Ipv4Addr::new(74, 50, 83, 83))
        );
    }
}
