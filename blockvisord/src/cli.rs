use clap::{ArgEnum, Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[clap(name = "bv", author, version, about)]
pub struct App {
    #[clap(flatten)]
    pub global_opts: GlobalOpts,

    #[clap(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// One-time password
    pub otp: String,

    /// BlockJoy API url
    #[clap(long = "url", default_value = "https://api.blockvisor.dev")]
    pub blockjoy_api_url: String,

    /// Network interface name
    #[clap(long = "ifa", default_value = "bvbr0")]
    pub ifa: String,
}

#[derive(Debug, Args)]
pub struct ResetArgs {
    /// Assume "yes" as answer to all prompts
    #[clap(short, long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct StartArgs {}

#[derive(Debug, Args)]
pub struct StopArgs {}

#[derive(Debug, Args)]
pub struct StatusArgs {}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialise blockvisor to run on this host
    Init(InitArgs),

    /// Completelly remove all nodes, configs and unregister the host from the API
    Reset(ResetArgs),

    /// Start blockvisor service
    Start(StartArgs),

    /// Stop blockvisor service
    Stop(StopArgs),

    /// Return blockvisor status
    Status(StatusArgs),

    /// Manage nodes on this host
    Node {
        #[clap(subcommand)]
        command: NodeCommand,
    },

    /// Manage host configuration and collect host info
    Host {
        #[clap(subcommand)]
        command: HostCommand,
    },

    /// Get information about chains
    Chain {
        #[clap(subcommand)]
        command: ChainCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Show nodes list
    List {
        /// Should we display all nodes including stopped
        #[clap(long)]
        all: bool,

        /// Display nodes of particular chain
        #[clap(long)]
        chain: Option<String>,
    },

    /// Create node
    Create {
        /// Chain identifier
        chain: String,
    },

    /// Start node
    Start {
        /// Node id
        id: String,
    },

    /// Stop node
    Stop {
        /// Node id
        id: String,
    },

    /// Restart node
    Restart {
        /// Node id
        id: String,
    },

    /// Delete node and clean up resources
    Delete {
        /// Node id
        id: String,
    },

    /// Attach to node console
    Console {
        /// Node id
        id: String,
    },

    /// Display node logs
    Logs {
        /// Node id
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum HostCommand {
    /// Collect host system information
    Info,

    /// Manage host network configuration
    Network {
        #[clap(subcommand)]
        command: HostNetworkCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum HostNetworkCommand {
    Info,

    Bridge {
        #[clap(subcommand)]
        command: HostNetworkBridgeCommand,
    },

    Ip {
        #[clap(subcommand)]
        command: HostNetworkIpCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum HostNetworkBridgeCommand {
    List,

    Create {
        #[clap(long)]
        name: String,
    },

    Delete {
        #[clap(long)]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum HostNetworkIpCommand {
    List,

    Add {
        #[clap(long)]
        net: String,
    },

    Delete {
        #[clap(long)]
        net: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChainCommand {
    /// Show chains list
    List,

    /// Display chain status
    Status {
        /// Chain identifier
        #[clap(long)]
        id: String,
    },

    /// Run chain synchronization process
    Sync {
        /// Chain identifier
        #[clap(long)]
        id: String,
    },
}

#[derive(ArgEnum, PartialEq, Eq, Debug, Clone)]
pub enum FormatArg {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct GlobalOpts {
    /// Verbosity level (can be specified multiple times)
    #[clap(long, short, global = true, parse(from_occurrences))]
    verbose: usize,

    /// Output format
    #[clap(long, short, global = true, arg_enum, default_value = "text")]
    pub format: FormatArg,
}
