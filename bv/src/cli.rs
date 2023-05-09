use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[clap(name = "bv", author, version, about)]
pub struct App {
    #[clap(flatten)]
    pub global_opts: GlobalOpts,

    #[clap(subcommand)]
    pub command: Command,
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
        ip: String,

        /// Gateway IP Address
        #[clap(long)]
        gateway: String,

        /// The properties that are passed to the node. These are used for running certain babel
        /// commands. For example, the ether nodes require that at least one property whose name
        /// starts with `key` is passed here like so: `bv node create --props '{"key1": "asdf"}'`.
        #[clap(long)]
        props: Option<String>,
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
        id_or_names: Vec<String>,

        /// Delete all nodes on this host.
        #[clap(long, short)]
        all: bool,

        /// Skip all [y/N] prompts and `just do it`.
        #[clap(short, long)]
        yes: bool,
    },

    /// Display node jobs logs
    #[clap(alias = "l")]
    Logs {
        /// Node id or name
        id_or_name: String,
    },

    /// Display node Babel logs
    #[clap(alias = "bl")]
    BabelLogs {
        /// Node id or name
        id_or_name: String,
        /// Max number of log lines returned
        #[clap(long, short, default_value = "10")]
        max_lines: u32,
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

    /// Return supported methods that could be executed on running node via `bv node run`
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
        /// `bv node capabilities <id_or_name>` is ran.
        method: String,
        /// The payload that should be passed to the endpoint. This should be a string.
        #[clap(long)]
        param: Option<String>,
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

    /// Update host system information record in API
    #[clap(alias = "u")]
    Update,

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
}

#[derive(ValueEnum, PartialEq, Eq, Debug, Clone)]
pub enum FormatArg {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct GlobalOpts {
    /// Verbosity level (can be specified multiple times)
    #[clap(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Output format
    #[clap(long, short, global = true, value_enum, default_value = "text")]
    pub format: FormatArg,
}
