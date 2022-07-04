use zbus::{dbus_proxy, Result};

use crate::node_data::NodeData;

#[dbus_proxy(
    interface = "com.BlockJoy.blockvisor.Node",
    default_path = "/com/BlockJoy/blockvisor/Node",
    default_service = "com.BlockJoy.blockvisor"
)]
trait Node {
    async fn create(&self, id: &str, chain: &str) -> Result<()>;
    async fn delete(&self, id: &str) -> Result<()>;
    async fn start(&self, id: &str) -> Result<()>;
    async fn stop(&self, id: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<NodeData>>;

    // TODO: Rest of the NodeCommand variants.
}
