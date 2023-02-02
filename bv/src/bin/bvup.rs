use anyhow::{anyhow, Context, Result};
use blockvisord::config::Config;
use blockvisord::hosts::get_host_info;
use blockvisord::self_updater;
use blockvisord::services::api::pb;
use clap::{crate_version, ArgGroup, Parser};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
#[clap(group(ArgGroup::new("input").required(true).args(&["otp", "skip_init"])))]
#[clap(group(ArgGroup::new("skip").args(&["skip_download", "skip_init"])))]
pub struct CmdArgs {
    /// One-time password
    pub otp: Option<String>,

    /// BlockJoy API url
    #[clap(long = "api", default_value = "https://api.dev.blockjoy.com")]
    pub blockjoy_api_url: String,

    /// BlockJoy keys service url
    #[clap(long = "keys", default_value = "https://api.dev.blockjoy.com")]
    pub blockjoy_keys_url: String,

    /// BlockJoy registry url
    #[clap(long = "registry", default_value = "https://api.dev.blockjoy.com")]
    pub blockjoy_registry_url: String,

    /// Network interface name
    #[clap(long = "ifa", default_value = "bvbr0")]
    pub ifa: String,

    /// Skip provision and init phase
    #[clap(long = "skip-init")]
    pub skip_init: bool,

    /// Skip download and install phase
    #[clap(long = "skip-download")]
    pub skip_download: bool,
}

pub fn get_ip_address(ifa_name: &str) -> Result<String> {
    let ifas = local_ip_address::list_afinet_netifas()?;
    let (_, ip) = ifas
        .into_iter()
        .find(|(name, ipaddr)| name == ifa_name && ipaddr.is_ipv4())
        .ok_or_else(|| anyhow!("interface {ifa_name} not found"))?;
    Ok(ip.to_string())
}

/// Simple host init tool. It provision host with OTP than download and install latest bv bundle.
#[tokio::main]
async fn main() -> Result<()> {
    let cmd_args = CmdArgs::parse();
    let api_config = if !cmd_args.skip_init {
        println!("Provision and init blockvisor configuration");

        let ip = get_ip_address(&cmd_args.ifa).with_context(|| "failed to get ip address")?;
        let host_info = get_host_info();

        let info = pb::HostInfo {
            id: None,
            name: host_info.name,
            version: Some(crate_version!().to_string()),
            location: None,
            cpu_count: host_info.cpu_count,
            mem_size: host_info.mem_size,
            disk_size: host_info.disk_size,
            os: host_info.os,
            os_version: host_info.os_version,
            ip: Some(ip),
            ip_range_to: None,
            ip_range_from: None,
            ip_gateway: None,
        };
        let create = pb::ProvisionHostRequest {
            request_id: Some(Uuid::new_v4().to_string()),
            otp: cmd_args.otp.unwrap(),
            info: Some(info),
            status: pb::ConnectionStatus::Online.into(),
        };

        let mut client =
            pb::hosts_client::HostsClient::connect(cmd_args.blockjoy_api_url.clone()).await?;

        let host = client.provision(create).await?.into_inner();

        let api_config = Config {
            id: host.host_id,
            token: host.token,
            blockjoy_api_url: cmd_args.blockjoy_api_url,
            blockjoy_keys_url: cmd_args.blockjoy_keys_url,
            blockjoy_registry_url: cmd_args.blockjoy_registry_url,
            update_check_interval_secs: None,
        };
        api_config.save().await?;
        Some(api_config)
    } else {
        None
    };
    if !cmd_args.skip_download {
        println!("Download and install bv bundle");
        let api_config = match api_config {
            None => Config::load().await.with_context(|| {
                "failed to load configuration - need to provision and init first"
            })?,
            Some(value) => value,
        };

        let mut updater = self_updater::new::<self_updater::SysTimer>(&api_config)?;
        let bundle_id = updater
            .get_latest()
            .await?
            .ok_or_else(|| anyhow!("No bv bundle found"))?;
        updater.download_and_install(bundle_id).await?;
    }
    Ok(())
}
