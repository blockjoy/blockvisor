use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[clap(name = "bib", author, version, about)]
pub struct App {
    #[clap(flatten)]
    pub global_opts: GlobalOpts,

    #[clap(subcommand)]
    pub command: Command,
}

#[derive(Args)]
pub struct ConnectArgs {
    /// Client authentication token.
    pub token: String,

    /// BlockJoy API url.
    #[clap(long = "api", default_value = "https://api.prod.blockjoy.com")]
    pub blockjoy_api_url: String,
}

#[derive(Subcommand)]
pub enum Command {
    /// Connect bib to the API.
    Connect(ConnectArgs),

    /// Manage images and send them to the API.
    #[clap(alias = "img")]
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
    /// Create new image from scratch.
    Create {
        /// Associated protocol_key.
        protocol: String,
        /// Image variant_key.
        variant: String,
    },

    /// Print associated container image URI.
    ContainerUri {
        #[clap(default_value = "babel.yaml")]
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

        #[clap(default_value = "babel.yaml")]
        path: PathBuf,
    },

    /// Run sanity checks on given image and embedded plugin.
    Check {
        /// The properties that are passed to the node in form of JSON string.
        #[clap(long)]
        props: Option<String>,

        #[clap(default_value = "babel.yaml")]
        path: PathBuf,
    },

    /// Push image to the API.
    Push {
        #[clap(default_value = "babel.yaml")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum ProtocolCommand {
    /// Show protocols list.
    #[clap(alias = "ls")]
    List,
    /// Push protocols to the API.
    Push {
        #[clap(default_value = "protocols.yaml")]
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
