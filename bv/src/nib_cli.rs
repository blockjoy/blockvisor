use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[clap(name = "nib", author, version, about)]
pub struct App {
    #[clap(flatten)]
    pub global_opts: GlobalOpts,

    #[clap(subcommand)]
    pub command: Command,
}

#[derive(Args)]
pub struct LoginArgs {
    /// User id.
    pub user_id: String,
    /// BlockJoy API url.
    #[clap(long = "api", default_value = "https://api.prod.blockjoy.com")]
    pub blockjoy_api_url: String,
}

#[derive(Subcommand)]
pub enum Command {
    /// Connect nib to the API.
    Login(LoginArgs),

    /// Manage images and send them to the API.
    #[clap(alias = "i")]
    Image {
        #[clap(subcommand)]
        command: ImageCommand,
    },

    /// Get information about protocols.
    #[clap(alias = "p")]
    Protocol {
        #[clap(subcommand)]
        command: ProtocolCommand,
    },
}

#[derive(Subcommand)]
pub enum ImageCommand {
    // LEGACY node support - remove once all nodes upgraded
    /// Generate legacy nodes image mapping. Search for all 'babel.yaml' files in subdirectories
    /// and generate mapping based on 'legacy_store_key' found.
    GenerateMapping,

    /// Create new image from scratch.
    Create {
        /// Associated protocol_key.
        protocol: String,
        /// Image variant_key.
        variant: String,
    },

    /// Print associated container image URI.
    ContainerUri {
        /// Image definition file path.
        #[clap(long, short = 'u', default_value = "babel.yaml")]
        path: PathBuf,
    },

    /// Play with dev node created form the image.
    Play {
        /// Node IP Address
        #[clap(long)]
        ip: Option<String>,

        /// Gateway IP Address
        #[clap(long)]
        gateway: Option<String>,

        /// The properties that are passed to the node in form of JSON string.
        #[clap(long)]
        props: Option<String>,

        /// Image variant key.
        #[clap(long)]
        variant: Option<String>,

        /// Image definition file path.
        #[clap(long, default_value = "babel.yaml")]
        path: PathBuf,
    },

    /// Upgrade existing dev node with the given image.
    Upgrade {
        /// Image definition file path.
        #[clap(long, default_value = "babel.yaml")]
        path: PathBuf,

        /// Node id or name.
        id_or_name: String,
    },

    /// Run sanity checks on given image and embedded plugin.
    Check {
        /// Node IP Address
        #[clap(long)]
        ip: Option<String>,

        /// Gateway IP Address
        #[clap(long)]
        gateway: Option<String>,

        /// The properties that are passed to the node in form of JSON string.
        #[clap(long)]
        props: Option<String>,

        /// Image variant key.
        #[clap(long)]
        variant: Option<String>,

        /// Image definition file path.
        #[clap(long, default_value = "babel.yaml")]
        path: PathBuf,

        /// Run Rhai linter only. Don't create and run test node.
        #[clap(long)]
        lint: bool,

        /// Delete test node if all checks pass.
        #[clap(long)]
        cleanup: bool,

        /// Node start timeout in seconds - maximum time to wait for 'Running' node.
        #[clap(long, default_value = "30")]
        start_timeout: u64,

        /// Time to wait (in seconds) before jobs status check.
        #[clap(long, default_value = "5")]
        jobs_wait: u64,
    },

    /// Push image to the API.
    Push {
        /// Minimum Babel version required by the image to run.
        /// Current version if not provided.
        #[clap(long)]
        min_babel_version: Option<String>,
        /// Image definition file path.
        #[clap(long, default_value = "babel.yaml")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum ProtocolCommand {
    /// Show protocols list.
    #[clap(alias = "ls")]
    List {
        /// Protocol name (e.g. helium, eth)
        #[clap(long)]
        name: Option<String>,

        /// Display the first N items
        #[clap(long, short, default_value = "50")]
        number: u64,
    },
    /// Push protocols to the API.
    Push {
        /// Protocol definition file path.
        #[clap(long, default_value = "protocols.yaml")]
        path: PathBuf,
    },
}

#[derive(ValueEnum, PartialEq, Eq, Clone)]
pub enum FormatArg {
    Text,
    Json,
}

#[derive(Args)]
pub struct GlobalOpts {
    /// Verbosity level (can be specified multiple times).
    #[clap(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Output format.
    #[clap(long, global = true, value_enum, default_value = "text")]
    pub format: FormatArg,
}
