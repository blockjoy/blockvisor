use blockvisord::{
    linux_platform::bv_root,
    nib,
    nib_cli::{self, App, Command},
    nib_config::Config,
};
use clap::Parser;
use eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args = App::parse();
    let bv_root = bv_root();

    match args.command {
        Command::Config(nib_cli::ConfigArgs {
            token,
            blockjoy_api_url,
        }) => {
            Config {
                token,
                blockjoy_api_url,
            }
            .save()
            .await?;
        }
        Command::Image { command } => {
            let config = Config::load().await?;
            let connector = blockvisord::services::PlainConnector {
                token: config.token,
                url: config.blockjoy_api_url,
            };
            nib::process_image_command(connector, &bv_root, command).await?;
        }
        Command::Protocol { command } => {
            let config = Config::load().await?;
            let connector = blockvisord::services::PlainConnector {
                token: config.token,
                url: config.blockjoy_api_url,
            };
            nib::process_protocol_command(connector, command).await?;
        }
    }
    Ok(())
}
