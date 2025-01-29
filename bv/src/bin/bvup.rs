use blockvisord::{
    api_config::ApiConfig,
    bv_config::{self, Config, SharedConfig},
    hosts::HostInfo,
    linux_platform::bv_root,
    self_updater,
    services::api::{common, pb},
    services::{DEFAULT_API_CONNECT_TIMEOUT, DEFAULT_API_REQUEST_TIMEOUT},
    utils,
};
use bv_utils::cmd::{ask_confirm, ask_value, run_cmd};
use clap::{crate_version, ArgGroup, Parser};
use eyre::{anyhow, bail, Context, Result};
use std::str::FromStr;
use tonic::transport::Endpoint;

#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
#[clap(group(ArgGroup::new("input").required(true).args(&["provision_token", "skip_init"])))]
#[clap(group(ArgGroup::new("init").requires_all(&["provision_token", "region"])))]
#[clap(group(ArgGroup::new("net").requires_all(&["gateway_ip", "host_ip", "net_prefix", "available_ips"])))]
#[clap(group(ArgGroup::new("skip").args(&["skip_download", "skip_init"])))]
pub struct CmdArgs {
    /// Provision token
    pub provision_token: Option<String>,

    /// Host region
    #[clap(long = "region")]
    pub region: Option<String>,

    /// BlockJoy API url
    #[clap(long = "api", default_value = "https://api.prod.blockjoy.com")]
    pub blockjoy_api_url: String,

    /// Network bridge interface name
    #[clap(long = "ifa", default_value = "bvbr0")]
    pub bridge_ifa: String,

    /// Network gateway IP address
    #[clap(long = "gateway-ip")]
    pub gateway_ip: Option<String>,

    /// Host IP address
    #[clap(long = "host-ip")]
    pub host_ip: Option<String>,

    /// Nodes subnet prefix in bits.
    #[clap(long = "net_prefix")]
    pub net_prefix: Option<u8>,

    /// IPs available to be used by nodes.
    /// May be comma separated list of IPs (e.g. 192.168.0.2,192.168.0.2), dash IP range (192.168.0.2-192.168.0.5), or CIDR notation (e.g. 192.168.0.1/29).
    /// Host and gateway ips are automatically excluded.
    #[clap(long = "available-ips")]
    pub available_ips: Option<String>,

    /// Blockvisor service port.
    #[clap(long = "port")]
    pub blockvisor_port: Option<u16>,

    #[clap(long = "update", default_value = "60")]
    pub update_check_interval_secs: u64,

    /// Skip provisioning and init phase
    #[clap(long = "skip-init")]
    pub skip_init: bool,

    /// Skip download and install phase
    #[clap(long = "skip-download")]
    pub skip_download: bool,

    /// Make host private - visible only for your organisation.
    #[clap(long = "private")]
    pub private: bool,

    /// Use host network directly
    #[clap(long)]
    use_host_network: bool,

    /// Skip all [y/N] prompts.
    #[clap(short, long)]
    yes: bool,
}

/// Simple host init tool. It provisions host with PROVISION_TOKEN then download and install latest bv bundle.
#[tokio::main]
async fn main() -> Result<()> {
    let bv_root = bv_root();
    let cmd_args = CmdArgs::parse();
    let y = cmd_args.yes;
    let api_config = if !cmd_args.skip_init {
        //
        if run_cmd("systemctl", ["is-active", "blockvisor.service"])
            .await
            .is_ok()
        {
            bail!("Can't provision and init blockvisor configuration, while it is running, `bv stop` first.");
        }
        println!("Provision and init blockvisor configuration");

        let blockjoy_api_url = ask_value("blockjoy API url", &cmd_args.blockjoy_api_url, y)?
            .unwrap_or(cmd_args.blockjoy_api_url);
        if blockjoy_api_url.is_empty() {
            bail!("API url can't be empty");
        }

        let region = ask_value(
            "host region",
            &cmd_args.region.clone().unwrap_or_default(),
            y,
        )?
        .unwrap_or(cmd_args.region.unwrap_or_default());
        if region.is_empty() {
            bail!("region can't be empty");
        }

        let bridge_ifa = ask_value(
            "bridge interface name",
            &cmd_args.bridge_ifa,
            y || cmd_args.use_host_network,
        )?
        .unwrap_or(cmd_args.bridge_ifa);

        let mut net_conf = bv_config::NetConf::new(&bridge_ifa)
            .await
            .unwrap_or_default();
        if let Some(value) = cmd_args.gateway_ip {
            net_conf.override_gateway_ip(&value)?;
        }
        if let Some(value) = cmd_args.host_ip {
            net_conf.override_host_ip(&value)?;
        }
        if let Some(value) = ask_value("gateway ip", &net_conf.gateway_ip, y)? {
            net_conf.override_gateway_ip(&value)?;
        }
        if net_conf.gateway_ip.is_unspecified() {
            bail!("gateway ip can't be unspecified");
        }
        if let Some(value) = ask_value("host ip", &net_conf.host_ip, y)? {
            net_conf.override_host_ip(&value)?;
        }
        if net_conf.host_ip.is_unspecified() {
            bail!("host ip can't be unspecified");
        }
        if let Some(value) = ask_value(
            "subnet prefix",
            &cmd_args.net_prefix.unwrap_or(net_conf.prefix),
            y || cmd_args.use_host_network,
        )? {
            net_conf.prefix =
                u8::from_str(&value).with_context(|| format!("invalid subnet prefix '{value}'"))?;
        }

        if let Some(value) = &cmd_args.available_ips {
            net_conf.override_ips(value)?;
        } else if net_conf.available_ips.is_empty() || cmd_args.use_host_network {
            net_conf.available_ips = vec![net_conf.host_ip];
        }
        if let Some(value) = ask_value(
            "available IPs",
            &net_conf
                .available_ips
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(","),
            y,
        )? {
            net_conf.override_ips(&value)?;
        }

        let host_info = HostInfo::collect()?;
        let cpu_count = host_info
            .cpu_count
            .try_into()
            .with_context(|| "Cannot convert cpu count from usize to u64")?;
        let to_gb = |n| n as f64 / 1_000_000_000.0;

        println!("Hostname:            {:>16}", host_info.name);
        println!("API url:             {:>16}", blockjoy_api_url);
        println!("Region:              {:>16}", region);
        println!("OS:                  {:>16}", host_info.os);
        println!("OS version:          {:>16}", host_info.os_version);
        println!("Gateway IP address:  {:>16}", net_conf.gateway_ip);
        println!("Host IP address:     {:>16}", net_conf.host_ip);
        println!("Subnet prefix        {:>16}", net_conf.prefix);
        println!(
            "Available node IPs: {:>16}",
            format!(
                "[{}]",
                net_conf
                    .available_ips
                    .iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        );
        println!("CPU count:           {:>16}", cpu_count);
        println!(
            "Total mem:           {:>16.3} GB",
            to_gb(host_info.memory_bytes)
        );
        println!(
            "Total disk:          {:>16.3} GB",
            to_gb(host_info.disk_space_bytes)
        );

        let confirm = ask_confirm("Register the host with this configuration?", y)?;
        if !confirm {
            return Ok(());
        }

        if !cmd_args.use_host_network {
            run_cmd("sysctl", ["-w", "net.ipv4.ip_forward=1"]).await?;
            const APPTAINER_NET_CONFIG_DIR: &str = "etc/apptainer/network";
            const USR_LOCAL: &str = "usr/local";
            let apptainer_net_dir = if bv_root.join(APPTAINER_NET_CONFIG_DIR).exists() {
                bv_root.join(APPTAINER_NET_CONFIG_DIR)
            } else if bv_root
                .join(USR_LOCAL)
                .join(APPTAINER_NET_CONFIG_DIR)
                .exists()
            {
                bv_root.join(USR_LOCAL).join(APPTAINER_NET_CONFIG_DIR)
            } else {
                bail!("apptainer network config not found");
            };
            utils::render_template(
                include_str!("../../data/00_bridge.conflist.template"),
                &apptainer_net_dir.join("00_bridge.conflist"),
                &[
                    ("bridge_ifa", &bridge_ifa),
                    ("host_ip", &net_conf.host_ip.to_string()),
                ],
            )?;
        }

        let mut client = pb::host_service_client::HostServiceClient::connect(
            Endpoint::from_shared(blockjoy_api_url.clone())?
                .connect_timeout(DEFAULT_API_CONNECT_TIMEOUT)
                .timeout(DEFAULT_API_REQUEST_TIMEOUT),
        )
        .await?;

        let region = client
            .get_region(pb::HostServiceGetRegionRequest {
                region: Some(pb::host_service_get_region_request::Region::RegionKey(
                    region.clone(),
                )),
            })
            .await?
            .into_inner()
            .region
            .ok_or(anyhow!("region '{region}' not found"))?;

        let create = pb::HostServiceCreateHostRequest {
            provision_token: cmd_args.provision_token.unwrap(),
            is_private: cmd_args.private,
            network_name: host_info.name.clone(),
            display_name: None,
            bv_version: crate_version!().to_string(),
            cpu_cores: cpu_count,
            memory_bytes: host_info.memory_bytes,
            disk_bytes: host_info.disk_space_bytes,
            os: host_info.os,
            os_version: host_info.os_version,
            ip_address: net_conf.host_ip.to_string(),
            ip_gateway: net_conf.gateway_ip.to_string(),
            region_id: region.region_id,
            schedule_type: common::ScheduleType::Automatic.into(),
            ips: net_conf
                .available_ips
                .iter()
                .map(|ip| ip.to_string())
                .collect(),
            tags: Some(common::Tags {
                tags: vec![common::Tag {
                    name: "testing".to_string(),
                }],
            }),
        };

        let host = client.create_host(create).await?.into_inner();

        let mut host_config = Config {
            id: host
                .host
                .ok_or_else(|| anyhow!("No `host` in response"))?
                .host_id,
            private_org_id: cmd_args.private.then_some(host.provision_org_id),
            name: host_info.name,
            api_config: ApiConfig {
                token: host.token,
                refresh_token: host.refresh,
                blockjoy_api_url,
            },
            blockjoy_mqtt_url: None,
            update_check_interval_secs: Some(cmd_args.update_check_interval_secs),
            blockvisor_port: cmd_args
                .blockvisor_port
                .unwrap_or_else(bv_config::default_blockvisor_port),
            iface: bridge_ifa,
            net_conf,
            ..Default::default()
        };
        if cmd_args.use_host_network {
            host_config.apptainer.host_network = true;
            host_config.apptainer.cpu_limit = false;
            host_config.apptainer.memory_limit = false;
        }
        host_config.save(&bv_root).await?;
        Some(host_config)
    } else {
        None
    };
    if !cmd_args.skip_download {
        println!("Download and install bv bundle");
        let config = match api_config {
            None => Config::load(&bv_root)
                .await
                .with_context(|| "Failed to load host configuration - need to init first")?,
            Some(value) => value,
        };
        let api_config = SharedConfig::new(config, bv_root.clone());

        let mut updater =
            self_updater::new(bv_utils::timer::SysTimer, &bv_root, &api_config).await?;
        let version = updater
            .get_versions()
            .await?
            .pop()
            .ok_or_else(|| anyhow!("No bv bundle found"))?
            .to_string();
        updater
            .download_and_install(pb::BundleIdentifier { version })
            .await?;
    }
    Ok(())
}
