use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[clap(name = "bv", author, version, about)]
pub struct App {
    #[clap(flatten)]
    pub global_opts: GlobalOpts,

    #[clap(subcommand)]
    pub command: Command,
}

#[derive(Args)]
pub struct StartArgs {}

#[derive(Args)]
pub struct StopArgs {}

#[derive(Args)]
pub struct StatusArgs {}

#[derive(Subcommand)]
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

    /// Get information about protocols
    #[clap(alias = "p")]
    Protocol {
        #[clap(subcommand)]
        command: ProtocolCommand,
    },

    /// Manage host cluster connections
    #[clap(alias = "p2p")]
    Cluster {
        #[clap(subcommand)]
        command: ClusterCommand,
    },
}

#[derive(Subcommand)]
pub enum NodeCommand {
    // LEGACY node support - remove once all nodes upgraded
    /// Fix legacy nodes image metadata (to make it upgradeable), according to image mapping file.
    FixLegacy {
        /// Image definition file path.
        #[clap(default_value = "image_mapping.json")]
        path: PathBuf,
    },

    /// Show nodes list.
    #[clap(alias = "ls")]
    List {
        /// Display only running nodes.
        #[clap(long, short)]
        running: bool,
    },

    /// Create node.
    #[clap(alias = "c")]
    Create {
        /// Protocol key.
        protocol: String,
        /// Protocol variant key
        variant: String,
        /// Version of image, or skip to use latest,
        version: Option<String>,
        /// Version of image build, or skip to use latest,
        build: Option<u64>,

        /// The properties that are passed to the node in form of JSON string.
        #[clap(long)]
        props: Option<String>,
    },

    /// Start node.
    Start {
        /// Node id or name.
        #[clap(required(false))]
        id_or_names: Vec<String>,
    },

    /// Stop node.
    Stop {
        /// One or more node id or names.
        #[clap(required(false))]
        id_or_names: Vec<String>,

        /// Try to stop node, even if its in failed state.
        #[clap(long)]
        force: bool,
    },

    /// Restart node.
    Restart {
        /// One or more node id or names.
        id_or_names: Vec<String>,

        /// Try to restart node, even if its in failed state.
        #[clap(long)]
        force: bool,
    },

    /// Trigger node upgrade.
    Upgrade {
        /// Version of image, or skip to use latest,
        #[clap(long)]
        version: Option<String>,
        /// Version of image build, or skip to use latest,
        #[clap(long)]
        build: Option<u64>,
        /// Node id or name.
        #[clap(required(false))]
        id_or_names: Vec<String>,
    },

    /// Delete node and clean up resources.
    #[clap(alias = "d", alias = "rm")]
    Delete {
        /// One or more node id or names.
        #[clap(required(false))]
        id_or_names: Vec<String>,

        /// Delete all nodes on this host.
        #[clap(long, short)]
        all: bool,

        /// Skip all [y/N] prompts and `just do it`.
        #[clap(short, long)]
        yes: bool,
    },

    /// Get node status.
    Status {
        /// One or more node id or names.
        id_or_names: Vec<String>,
    },

    /// Return supported methods that could be executed on running node via `bv node run`.
    #[clap(alias = "caps")]
    Capabilities {
        /// Node id or name.
        id_or_name: String,
    },

    /// Runs a babel method defined by babel plugin.
    Run {
        /// The method that should be called. This should be one of the methods listed when
        /// `bv node capabilities <id_or_name>` is ran.
        method: String,
        /// The id or name of the node that the command should be run against
        id_or_name: String,
        /// String parameter passed to the method.
        #[clap(long)]
        param: Option<String>,
        /// Path to file with string parameter content, passed to the method.
        #[clap(long)]
        param_file: Option<PathBuf>,
    },

    /// Check current state of the node (including metrics).
    Info {
        /// The id or name of the node to be checked.
        id_or_name: String,
    },

    /// Manage jobs on given node.
    #[clap(alias = "j")]
    Job {
        #[clap(subcommand)]
        command: JobCommand,

        /// The id or name of the node to check.
        id_or_name: String,
    },

    /// Shell-into the node.
    Shell {
        /// The id or name of the node.
        id_or_name: String,
    },
}

#[derive(Subcommand)]
pub enum JobCommand {
    /// Show node jobs list.
    #[clap(alias = "ls")]
    List,

    /// Start job with given name.
    Start {
        /// Job name to be started.
        name: String,
    },

    /// Stop job.
    #[clap(group(ArgGroup::new("job_input").required(true).args(& ["name", "all"])))]
    Stop {
        /// Job name to be stopped.
        name: Option<String>,
        /// Stop all jobs.
        #[clap(long, short)]
        all: bool,
    },

    /// Skip job. If it's Running, it will be stopped first, then marked as Finished with 0 code.
    Skip {
        /// Job name to be skipped.
        name: String,
    },

    /// Cleanup job - remove any intermediate files, so next time it will start from scratch.
    Cleanup {
        /// Job name to be cleaned up.
        name: String,
    },

    /// Get job info.
    Info {
        /// Job name.
        name: String,
    },

    /// Get job logs.
    Logs {
        /// Job name. Get all jobs logs if not specified.
        name: Option<String>,
        /// Output the last N lines.
        #[clap(long, short, alias = "n", default_value = "50")]
        lines: usize,
        /// Follow logs.
        #[clap(long, short, alias = "f", default_value = "false")]
        follow: bool,
    },
}

#[derive(Subcommand)]
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

#[derive(Subcommand)]
pub enum ProtocolCommand {
    /// Show protocol images list
    #[clap(alias = "ls")]
    List {
        /// Protocol name (e.g. helium, eth)
        #[clap(long)]
        name: Option<String>,

        /// Display the first N items
        #[clap(long, short, default_value = "50")]
        number: u64,
    },
}

#[derive(Subcommand)]
pub enum ClusterCommand {
    /// Show host cluster information
    #[clap(alias = "s")]
    Status {},
}

#[derive(ValueEnum, PartialEq, Eq, Clone)]
pub enum FormatArg {
    Text,
    Json,
}

#[derive(Args)]
pub struct GlobalOpts {
    /// Verbosity level (can be specified multiple times)
    #[clap(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Output format
    #[clap(long, global = true, value_enum, default_value = "text")]
    pub format: FormatArg,
}
