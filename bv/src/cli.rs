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
    #[clap(long = "url", default_value = "https://api.dev.blockjoy.com")]
    pub blockjoy_api_url: String,

    /// BlockJoy registry url
    #[clap(long = "registry", default_value = "https://api.dev.blockjoy.com")]
    pub blockjoy_registry_url: String,

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
    #[clap(alias = "i")]
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
    #[clap(alias = "n")]
    Node {
        #[clap(subcommand)]
        command: NodeCommand,
    },

    /// Manage host configuration and collect host info
    #[clap(alias = "h")]
    Host {
        #[clap(subcommand)]
        command: HostCommand,
    },

    /// Get information about chains
    #[clap(alias = "c")]
    Chain {
        #[clap(subcommand)]
        command: ChainCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Show nodes list
    #[clap(alias = "ls")]
    List {
        /// Display only running nodes
        #[clap(long, short)]
        running: bool,
    },

    /// Create node
    #[clap(alias = "c")]
    Create {
        /// Node Image identifier
        image: String,

        /// Node IP Address
        #[clap(long)]
        ip: Option<String>,

        /// Gateway IP Address
        #[clap(long)]
        gateway: Option<String>,
    },

    /// Upgrade node
    #[clap(alias = "u")]
    Upgrade {
        /// Node id or name
        #[clap(required(true))]
        id_or_names: Vec<String>,

        /// Node Image identifier
        image: String,
    },

    /// Start node
    Start {
        /// Node id or name
        #[clap(required(false))]
        id_or_names: Vec<String>,
    },

    /// Stop node
    Stop {
        /// One or more node id or names.
        #[clap(required(false))]
        id_or_names: Vec<String>,
    },

    /// Restart node
    Restart {
        /// One or more node id or names.
        #[clap(required(true))]
        id_or_names: Vec<String>,
    },

    /// Delete node and clean up resources
    #[clap(alias = "d", alias = "rm")]
    Delete {
        /// One or more node id or names.
        #[clap(required(true))]
        id_or_names: Vec<String>,
    },

    /// Attach to node console
    #[clap(alias = "c")]
    Console {
        /// Node id or name
        id_or_name: String,
    },

    /// Display node logs
    #[clap(alias = "l")]
    Logs {
        /// Node id or name
        id_or_name: String,
    },

    /// Get installed key names
    #[clap(alias = "k")]
    Keys {
        /// Node id or name
        id_or_name: String,
    },

    /// Get node status
    Status {
        /// One or more node id or names.
        #[clap(required(true))]
        id_or_names: Vec<String>,
    },

    /// Return the block height of the blockchain the node is running
    #[clap(alias = "caps")]
    Capabilities {
        /// Node id or name
        id_or_name: String,
    },

    /// Runs a command against the blockchain node inside the guest operating system that babel is
    /// talking to.
    Run {
        /// The id or name of the node that the command should be run against.
        id_or_name: String,
        /// The method that should be called. This should be one of the methods listed when
        /// `nv node capabilities <id_or_name>` is ran.
        method: String,
        /// The payload that should be passed to the endpoint. This should be a json encoded blob.
        #[clap(long)]
        params: Option<String>,
    },

    /// Collect metrics defining the current state of the node.
    Metrics {
        /// The id or name of the node whose metrics should be collected.
        id_or_name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum HostCommand {
    /// Collect host system information
    #[clap(alias = "i")]
    Info,

    /// Collect metrics about the current host
    #[clap(alias = "m")]
    Metrics,
}

#[derive(Debug, Subcommand)]
pub enum ChainCommand {
    /// Show chains list
    #[clap(alias = "ls")]
    List {
        /// Blockchain protocol name (e.g. helium, eth)
        protocol: String,

        /// Blockchain node type (e.g. validator)
        r#type: String,

        /// Display the first N items
        #[clap(long, short, default_value = "10")]
        number: usize,
    },

    /// Display chain status
    Status {
        /// Chain identifier
        id: String,
    },

    /// Run chain synchronization process
    Sync {
        /// Chain identifier
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
