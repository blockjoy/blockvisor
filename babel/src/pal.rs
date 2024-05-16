use crate::BabelEngineClient;
use async_trait::async_trait;
use babel_api::babel::NodeContext;
use babel_api::metadata::firewall::Config;
use babel_api::metadata::RamdiskConfiguration;
use bv_utils::run_flag::RunFlag;

/// Trait that allows to inject custom babel_engine implementation.
#[async_trait]
pub trait BabelServer {
    async fn serve<T: babel_api::babel::babel_server::Babel>(
        &self,
        server: babel_api::babel::babel_server::BabelServer<T>,
        run: RunFlag,
    ) -> eyre::Result<()>;
}

/// Trait that allows to inject custom babel_engine implementation.
pub trait BabelEngineConnector {
    fn connect(&self) -> BabelEngineClient;
}

/// Trait that allows to inject custom PAL implementation.
#[async_trait]
pub trait BabelPal {
    type BabelServer: BabelServer;
    fn babel_server(&self) -> Self::BabelServer;
    type Connector: BabelEngineConnector;
    fn connector(&self) -> Self::Connector;
    async fn set_node_context(&self, node_context: NodeContext) -> eyre::Result<()>;
    async fn set_ram_disks(&self, ram_disks: Option<Vec<RamdiskConfiguration>>)
        -> eyre::Result<()>;
    async fn is_ram_disks_set(
        &self,
        ram_disks: Option<Vec<RamdiskConfiguration>>,
    ) -> eyre::Result<bool>;
    async fn apply_firewall_config(&self, config: Config) -> eyre::Result<()>;
}
