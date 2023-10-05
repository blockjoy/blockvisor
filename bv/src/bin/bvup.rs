use blockvisord::config::SharedConfig;
use blockvisord::{
    config, config::Config, hosts::HostInfo, linux_platform::bv_root, self_updater,
    services::api::pb, BV_VAR_PATH,
};
use bv_utils::cmd::{ask_confirm, run_cmd};
use cidr_utils::cidr::Ipv4Cidr;
use clap::{crate_version, ArgGroup, Parser};
use eyre::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
#[clap(group(ArgGroup::new("input").required(true).args(&["provision_token", "skip_init"])))]
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

    /// BlockJoy MQTT url
    #[clap(long = "mqtt")]
    pub blockjoy_mqtt_url: Option<String>,

    /// Network bridge interface name
    #[clap(long = "ifa", default_value = "bvbr0")]
    pub bridge_ifa: String,

    /// Network gateway IPv4 address
    #[clap(long = "ip-gateway")]
    pub ip_gateway: Option<String>,

    /// Network IP range from IPv4 value
    #[clap(long = "ip-range-from")]
    pub ip_range_from: Option<String>,

    /// Network IP range to IPv4 value
    #[clap(long = "ip-range-to")]
    pub ip_range_to: Option<String>,

    #[clap(long = "port")]
    pub blockvisor_port: Option<u16>,

    /// Skip provisioning and init phase
    #[clap(long = "skip-init")]
    pub skip_init: bool,

    /// Skip download and install phase
    #[clap(long = "skip-download")]
    pub skip_download: bool,

    /// Skip all [y/N] prompts.
    #[clap(short, long)]
    yes: bool,
}

pub fn get_ip_address(ifa_name: &str) -> Result<String> {
    let ifas = local_ip_address::list_afinet_netifas()?;
    let (_, ip) = ifas
        .into_iter()
        .find(|(name, ipaddr)| name == ifa_name && ipaddr.is_ipv4())
        .ok_or_else(|| anyhow!("interface {ifa_name} not found"))?;
    Ok(ip.to_string())
}

/// Struct to capture output of linux `ip --json route` command
#[derive(Deserialize, Serialize, Debug)]
struct IpRoute {
    pub dst: String,
    pub gateway: Option<String>,
    pub dev: String,
    pub prefsrc: Option<String>,
}

#[derive(Default, Debug, PartialEq)]
struct NetParams {
    pub ip: Option<String>,
    pub gateway: Option<String>,
    pub ip_from: Option<String>,
    pub ip_to: Option<String>,
}

fn parse_net_params_from_str(ifa_name: &str, routes_json_str: &str) -> Result<NetParams> {
    let mut routes: Vec<IpRoute> = serde_json::from_str(routes_json_str)?;
    routes.retain(|r| r.dev == ifa_name);
    if routes.len() != 2 {
        bail!("Routes count for `{ifa_name}` not equal to 2");
    }

    let mut params = NetParams::default();
    for route in routes {
        if route.dst == "default" {
            // Host gateway IP address
            params.gateway = route.gateway;
        } else {
            // IP range available for VMs
            let cidr = Ipv4Cidr::from_str(&route.dst)
                .with_context(|| format!("cannot parse {} as cidr", route.dst))?;
            let mut ips = cidr.iter();
            if cidr.get_bits() <= 30 {
                // For routing mask values <= 30, first and last IPs are
                // base and broadcast addresses and are unusable.
                ips.next();
                ips.next_back();
            }
            params.ip_from = ips.next().map(|u| Ipv4Addr::from(u).to_string());
            params.ip_to = ips.next_back().map(|u| Ipv4Addr::from(u).to_string());
            // Host IP address
            params.ip = route.prefsrc;
        }
    }
    Ok(params)
}

async fn discover_net_params(ifa_name: &str) -> Result<NetParams> {
    let routes = run_cmd("ip", ["--json", "route"]).await?;
    let params = parse_net_params_from_str(ifa_name, &routes)?;
    Ok(params)
}

/// Simple host init tool. It provision host with PROVISION_TOKEN then download and install latest bv bundle.
#[tokio::main]
async fn main() -> Result<()> {
    let bv_root = bv_root();
    let cmd_args = CmdArgs::parse();
    let api_config = if !cmd_args.skip_init {
        //
        if run_cmd("systemctl", ["is-active", "blockvisor.service"])
            .await
            .is_ok()
        {
            bail!("Can't provision and init blockvisor configuration, while it is running, `bv stop` first.");
        }
        println!("Provision and init blockvisor configuration");

        let net = discover_net_params(&cmd_args.bridge_ifa)
            .await
            .unwrap_or_default();
        // if network params are not provided, try to use auto-discovered values
        // or fail if both methods do not resolve to useful values
        let ip = get_ip_address(&cmd_args.bridge_ifa)
            .ok()
            .or(net.ip)
            .ok_or_else(|| anyhow!("Failed to resolve `ip` address"))?;
        let gateway = cmd_args
            .ip_gateway
            .or(net.gateway)
            .ok_or_else(|| anyhow!("Failed to resolve `gateway` address"))?;
        let range_from = cmd_args
            .ip_range_from
            .or(net.ip_from)
            .ok_or_else(|| anyhow!("Failed to resolve `from` address"))?;
        let range_to = cmd_args
            .ip_range_to
            .or(net.ip_to)
            .ok_or_else(|| anyhow!("Failed to resolve `to` address"))?;

        let host_info = HostInfo::collect()?;
        let cpu_count = host_info
            .cpu_count
            .try_into()
            .with_context(|| "Cannot convert cpu count from usize to u64")?;
        let to_gb = |n| n as f64 / 1_000_000_000.0;

        println!("Hostname:            {:>16}", &host_info.name);
        println!(
            "Region:              {:>16}",
            cmd_args.region.as_deref().unwrap_or("(not specified)")
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
        println!("OS:                  {:>16}", &host_info.os);
        println!("OS version:          {:>16}", &host_info.os_version);
        println!("API url:             {:>16}", &cmd_args.blockjoy_api_url);
        println!(
            "MQTT url:            {:>16}",
            cmd_args.blockjoy_mqtt_url.as_deref().unwrap_or("(auto)")
        );
        println!("Network IP from:     {:>16}", &range_from);
        println!("Network IP to:       {:>16}", &range_to);
        println!("IP address:          {:>16}", &ip);
        println!("Gateway IP address:  {:>16}", &gateway);

        let confirm = ask_confirm("Register the host with this configuration?", cmd_args.yes)?;
        if !confirm {
            return Ok(());
        }

        let create = pb::HostServiceCreateRequest {
            provision_token: cmd_args.provision_token.unwrap(),
            name: host_info.name,
            version: crate_version!().to_string(),
            cpu_count,
            mem_size_bytes: host_info.memory_bytes,
            disk_size_bytes: host_info.disk_space_bytes,
            os: host_info.os,
            os_version: host_info.os_version,
            ip_addr: ip,
            ip_gateway: gateway,
            ip_range_from: range_from,
            ip_range_to: range_to,
            org_id: None,
            region: cmd_args.region,
            billing_amount: None,
            vmm_mountpoint: Some(format!("{}", bv_root.join(BV_VAR_PATH).to_string_lossy())),
        };

        let mut client =
            pb::host_service_client::HostServiceClient::connect(cmd_args.blockjoy_api_url.clone())
                .await?;

        let host = client.create(create).await?.into_inner();

        let api_config = Config {
            id: host
                .host
                .ok_or_else(|| anyhow!("No `host` in response"))?
                .id,
            token: host.token,
            refresh_token: host.refresh,
            blockjoy_api_url: cmd_args.blockjoy_api_url.clone(),
            blockjoy_mqtt_url: cmd_args.blockjoy_mqtt_url,
            update_check_interval_secs: None,
            blockvisor_port: cmd_args
                .blockvisor_port
                .unwrap_or_else(config::default_blockvisor_port),
            iface: cmd_args.bridge_ifa,
        };
        api_config.save(&bv_root).await?;
        Some(api_config)
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
        let bundle_id = updater
            .get_latest()
            .await?
            .ok_or_else(|| anyhow!("No bv bundle found"))?;
        updater.download_and_install(bundle_id).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_net_params_from_str() {
        let json = r#"[
            {
               "dev" : "bvbr0",
               "dst" : "default",
               "flags" : [],
               "gateway" : "192.69.220.81",
               "protocol" : "static"
            },
            {
               "dev" : "bvbr0",
               "dst" : "192.69.220.80/28",
               "flags" : [],
               "prefsrc" : "192.69.220.82",
               "protocol" : "kernel",
               "scope" : "link"
            }
         ]
         "#;
        let expected = NetParams {
            ip: Some("192.69.220.82".to_string()),
            gateway: Some("192.69.220.81".to_string()),
            ip_from: Some("192.69.220.81".to_string()),
            ip_to: Some("192.69.220.94".to_string()),
        };
        assert_eq!(parse_net_params_from_str("bvbr0", json).unwrap(), expected);
    }
}
