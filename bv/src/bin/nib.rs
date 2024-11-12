use blockvisord::{
    linux_platform::bv_root,
    nib,
    nib_cli::{self, App, Command},
    nib_config::Config,
    services,
    services::api::{common, pb},
};
use clap::Parser;
use eyre::{anyhow, Result};
use std::io::{BufRead, Write};

#[tokio::main]
async fn main() -> Result<()> {
    let args = App::parse();
    let bv_root = bv_root();

    match args.command {
        Command::Login(nib_cli::LoginArgs {
            user_id,
            blockjoy_api_url,
        }) => {
            print!("email: ");
            std::io::stdout().flush()?;
            let mut email = String::new();
            std::io::stdin().lock().read_line(&mut email)?;
            let password = rpassword::prompt_password("password: ")?;
            let mut client =
                pb::auth_service_client::AuthServiceClient::connect(blockjoy_api_url.clone())
                    .await?;
            let login = client
                .login(pb::AuthServiceLoginRequest {
                    email: email.trim().to_string(),
                    password,
                })
                .await?
                .into_inner();
            let channel = tonic::transport::Endpoint::from_shared(blockjoy_api_url.clone())?
                .connect()
                .await?;
            let mut client = pb::api_key_service_client::ApiKeyServiceClient::with_interceptor(
                channel,
                services::AuthToken(login.token),
            );
            let response = client
                .create(pb::ApiKeyServiceCreateRequest {
                    label: "nib_token".to_string(),
                    resource: Some(common::Resource {
                        resource_type: common::ResourceType::User.into(),
                        resource_id: user_id,
                    }),
                })
                .await?
                .into_inner();
            Config {
                token: response
                    .api_key
                    .ok_or(anyhow!("missing api_key in response"))?,
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
