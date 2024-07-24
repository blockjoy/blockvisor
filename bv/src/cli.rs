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

    /// Get information about chains
    #[clap(alias = "c")]
    Chain {
        #[clap(subcommand)]
        command: ChainCommand,
    },

    /// Manage workspace
    #[clap(alias = "w")]
    Workspace {
        #[clap(subcommand)]
        command: WorkspaceCommand,
    },

    /// Manage blockchain node images
    #[clap(alias = "i")]
    Image {
        #[clap(subcommand)]
        command: ImageCommand,
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
        /// Node IP Address
        #[clap(long)]
        ip: Option<String>,

        /// Gateway IP Address
        #[clap(long)]
        gateway: Option<String>,

        /// The properties that are passed to the node. These are used for running certain babel
        /// commands. For example, the ether nodes require that at least one property whose name
        /// starts with `key` is passed here like so: `bv node create --props '{"key1": "asdf"}'`.
        #[clap(long)]
        props: Option<String>,

        /// Blockchain network name
        #[clap(long)]
        network: String,

        /// Node Image identifier
        image: Option<String>,

        /// Create node in dev mode - without involvement of the API.
        /// Non-dev mode mean that BV CLI acts like the API frontend. Once node is created
        /// in non-dev mode, all operations on the node will be performed via API.
        /// If node is created in dev node, then BV CLI will omit the API, and it won't
        /// be aware of the node existence or any operation on it.
        #[clap(long, default_value = "false")]
        dev_mode: bool,
    },

    /// Upgrade node
    #[clap(alias = "u")]
    Upgrade {
        /// Node Image identifier
        image: String,

        /// Node id or name
        #[clap(required(false))]
        id_or_names: Vec<String>,
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

        /// Try to stop node, even if its in failed state.
        #[clap(long)]
        force: bool,
    },

    /// Restart node
    Restart {
        /// One or more node id or names.
        #[clap(required(false))]
        id_or_names: Vec<String>,

        /// Try to restart node, even if its in failed state.
        #[clap(long)]
        force: bool,
    },

    /// Delete node and clean up resources
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

    /// Get node status
    Status {
        /// One or more node id or names.
        #[clap(required(false))]
        id_or_names: Vec<String>,
    },

    /// Return supported methods that could be executed on running node via `bv node run`
    #[clap(alias = "caps")]
    Capabilities {
        /// Node id or name. BV tries to get it from workspace if not provided.
        id_or_name: Option<String>,
    },

    /// Runs a babel method defined by babel plugin.
    Run {
        /// The method that should be called. This should be one of the methods listed when
        /// `bv node capabilities <id_or_name>` is ran.
        method: String,
        /// The id or name of the node that the command should be run against. BV tries to get it from workspace if not provided.
        id_or_name: Option<String>,
        /// String parameter passed to the method.
        #[clap(long)]
        param: Option<String>,
        /// Path to file with string parameter content, passed to the method.
        #[clap(long)]
        param_file: Option<PathBuf>,
    },

    /// Collect metrics defining the current state of the node.
    Metrics {
        /// The id or name of the node whose metrics should be collected. BV tries to get it from workspace if not provided.
        id_or_name: Option<String>,
    },

    /// Execute node runtime checks.
    Check {
        /// The id or name of the node to check. BV tries to get it from workspace if not provided.
        id_or_name: Option<String>,
    },

    /// Manage jobs on given node
    #[clap(alias = "j")]
    Job {
        #[clap(subcommand)]
        command: JobCommand,

        /// The id or name of the node to check. BV tries to get it from workspace if not provided.
        id_or_name: Option<String>,
    },

    /// Shell-into node
    Shell {
        /// The id or name of the node. BV tries to get it from workspace if not provided.
        id_or_name: Option<String>,
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
    #[clap(group(ArgGroup::new("job_input").required(true).args(&["name", "all"])))]
    Stop {
        /// Job name to be stoped.
        name: Option<String>,
        /// Stop all jobs.
        #[clap(long, short)]
        all: bool,
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
        /// Job name.
        name: String,
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

#[derive(Subcommand)]
pub enum WorkspaceCommand {
    /// Create new BV workspace
    #[clap(alias = "c")]
    Create {
        /// workspace relative path
        path: String,
    },

    /// Set active node for current workspace (override previously set).
    #[clap(alias = "n")]
    SetActiveNode {
        /// The id or name of the node
        id_or_name: String,
    },

    /// Set active image for current workspace (override previously set).
    #[clap(alias = "i")]
    SetActiveImage {
        /// The id of node image
        image_id: String,
    },
}

#[derive(Subcommand)]
pub enum ImageCommand {
    /// Create new node image from scratch
    Create {
        /// New node image identifier in the following format: protocol/type/version
        image_id: String,

        /// Debian version
        #[clap(long, default_value = "jammy")]
        debian_version: String,

        /// Size of image disk for image, in GB
        #[clap(long, default_value = "20")]
        rootfs_size: u64,
    },

    /// Create new node image from existing one
    Clone {
        /// Source image identifier
        source_image_id: String,

        /// New node image identifier
        destination_image_id: String,
    },

    /// Capture image files from given node.
    /// Node must be stopped first.
    Capture {
        /// The id or name of the source node. BV tries to get it from workspace if not provided.
        node_id_or_name: Option<String>,
    },

    /// Upload image to S3 compatible storage.
    /// Following environment variables are expected to be set:
    /// AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
    Upload {
        /// Node image identifier
        image_id: Option<String>,
        /// S3 endpoint
        #[clap(
            long,
            default_value = "https://19afdffb308beea3e9c1ef3a95085d3b.r2.cloudflarestorage.com"
        )]
        s3_endpoint: String,
        /// S3 region
        #[clap(long, default_value = "us-east-1")]
        s3_region: String,
        /// S3 bucket
        #[clap(long, default_value = "cookbook-dev")]
        s3_bucket: String,
        /// S3 prefix
        #[clap(long, default_value = "chains")]
        s3_prefix: String,
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
