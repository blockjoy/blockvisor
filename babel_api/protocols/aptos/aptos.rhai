//how to install axel and lz4?
//fullnode.yaml to add to final node?
//other needed methods?

const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "1.2.4",
    protocol: "aptos",
    node_type: "validator",
    description: "Aptos node",
    requirements: #{
        vcpu_count: 8,
        mem_size_mb: 32768,
        disk_size_gb: 1000,
    },
    nets: #{
        mainnet: #{
            url: "https://fullnode.mainnet.aptoslabs.com/v1",
            net_type: "main",
            waypoint_url: "https://raw.githubusercontent.com/aptos-labs/aptos-networks/main/mainnet/waypoint.txt"
            genesis_url: "https://raw.githubusercontent.com/aptos-labs/aptos-networks/main/mainnet/genesis.blob"
        },
        testnet: #{
            url: "https://fullnode.testnet.aptoslabs.com/v1",
            net_type: "test",
            waypoint_url: "https://raw.githubusercontent.com/aptos-labs/aptos-networks/main/devnet/waypoint.txt"
            genesis_url: "https://raw.githubusercontent.com/aptos-labs/aptos-networks/main/devnet/genesis.blob"
        },
    },
    babel_config: #{
        data_directory_mount_point: "/blockjoy/aptos/",
        log_buffer_capacity_ln: 1000,
        swap_size_mb: 1024,
    },
    firewall: #{
        enabled: true,
        default_in: "deny",
        default_out: "allow",
        rules: [
            #{
                name: "Allowed incoming traffic on port",
                action: "allow",
                direction: "in",
                protocol: "tcp",
                ports: [8080],
            },
        ],
    },
    keys: #{
    },
};

//do we need it if we checking everything from mainnet/devnet urls?
const API_HOST = "http://localhost:8080";

fn init() {
    debug("Executing startup scripts...");

    let net = sanitize_sh_param(node_params().NETWORK);
    let metadata = &global::METADATA.nets[&net];

    let waypoint_url = metadata.waypoint_url;
    let genesis_url = metadata.genesis_url;

    let waypoint_download = run(format!("curl {} > /blockjoy/aptos/genesis.blob", waypoint_url));
    if waypoint_download.exit_code != 0 {
        return Err(waypoint_download.into());
    }

    let genesis_download = run(format!("curl {} > /blockjoy/aptos/waypoint.txt", genesis_url));
    if genesis_download.exit_code != 0 {
        return Err(genesis_download.into());
    }
    start_job("Running the Aptos validator node", #{
        body: `
        apt-get install axel lz4 -y
        wget -O aptos.tar.gz https://github.com/aptos-labs/aptos-core/archive/refs/tags/aptos-node-v1.2.4.tar.gz
        tar xvzf aptos.tar.gz
        cd aptos-core* && ./scripts/dev_setup.sh -b && cargo build -p aptos-node --release
        cp aptos-node /usr/bin/
        axel -o /blockjoy/aptos/node/aptos.tar.lz4 -n 10 http://swarm01.stakejoy.com/aptos.tar.lz4
        lz4 -d -c  /blockjoy/aptos/node/aptos.tar.lz4 | tar -x -C /blockjoy/aptos/node/
        `,
        restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });
}

fn application_status(duration_secs: INT) -> Result<String, String> {
    let net = sanitize_sh_param(node_params().NETWORK);
    let mut url = global::METADATA.nets[&net].url.clone();

    if duration_secs > 0 {
        url.push_str(&format!("?duration_secs={}", duration_secs));
    }

    let resp = http_get(&url);

    if resp.status_code != 200 {
        return Err(resp.body.clone().into());
    }

    let body = json::parse(&resp.body).map_err(|err| err.to_string());

    return body.into();
}

fn address() -> Result<string, string> {
    let net = sanitize_sh_param(node_params().NETWORK);
    let url = global::METADATA.nets[&net].url.clone();
    let resp = http_get(&url)?;

    if resp.status_code != 200 {
        return Err(resp.body.clone());
    }

    let body = json::parse(&resp.body).map_err(|err| err.to_string())?;

    let address = body.signature.public_key
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(address)
}