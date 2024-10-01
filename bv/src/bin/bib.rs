use blockvisord::{
    bib,
    bib_cli::{self, App, Command},
    bib_config::Config,
    linux_platform::bv_root,
};
use clap::Parser;
use eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args = App::parse();
    let bv_root = bv_root();

    match args.command {
        Command::Connect(bib_cli::ConnectArgs {
            token,
            blockjoy_api_url,
        }) => {
            Config {
                token,
                blockjoy_api_url,
                blockvisor_port: None,
            }
            .save()
            .await?;
        }
        Command::Image { command } => {
            let config = Config::load(&bv_root).await?;
            let connector = blockvisord::services::PlainConnector {
                token: config.token,
                url: config.blockjoy_api_url,
            };
            bib::process_image_command(connector, command).await?;
        }
        Command::Protocol { command } => {
            let config = Config::load(&bv_root).await?;
            let connector = blockvisord::services::PlainConnector {
                token: config.token,
                url: config.blockjoy_api_url,
            };
            bib::process_protocol_command(connector, command).await?;
        }
    }
    Ok(())
}
