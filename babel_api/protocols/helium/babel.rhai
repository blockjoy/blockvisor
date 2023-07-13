const METADATA = #{
    // comments are allowed
    min_babel_version: "0.13.0",
    node_version: "1.17.2",
    protocol: "helium",
    node_type: "validator",
    description: "helium blockchain validator",
    requirements: #{
        vcpu_count: 4,
        mem_size_mb: 16384,
        disk_size_gb: 200,
    },
    nets: #{
        devnet: #{
            url: "https://snapshots.helium.wtf/genesis.devnet",
            net_type: "test",
        },
        testnet: #{
            url: "https://snapshots.helium.wtf/genesis.testnet",
            net_type: "test",
        },
        mainnet: #{
            url: "https://snapshots.helium.wtf/genesis.mainnet",
            net_type: "main",
        },
    },
    babel_config: #{
        data_directory_mount_point: "/blockjoy/miner/data",
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 512,
    },
    firewall: #{
        enabled: true,
        default_in: "deny",
        default_out: "allow",
        rules: [
            #{
                name: "Allowed incoming tcp traffic on port",
                action: "allow",
                direction: "in",
                protocol: "tcp",
                ports: [4467],
            },
        ],
    },
    keys: #{
        "*": "/opt/secrets",
    },
};

const API_HOST = "http://localhost:4467/";

fn init(keys) {
    let net = sanitize_sh_param(node_params().NETWORK);
    let net_url = global::METADATA.nets[net].url;
    let res = run_sh("curl " + net_url + " > /opt/miner/update/genesis");
    if res.exit_code != 0  {
        throw res;
    }
    // FIXME: this seems to timeout
    // let res = run_sh("curl https://tools.stakejoy.com/sys.config.src." + net + " > /blockjoy/miner/releases/" + global::METADATA.node_version + "/sys.config.src");
    // if res.exit_code != 0  {
    //     throw res;
    // }
    start_job("helium", #{
        job_type: #{
            run_sh: "/blockjoy/miner/bin/miner foreground",
        },
        restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 100,
            },
        },
        needs: [],
    });
    return;
}

fn height() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_height"}).body).result.height
}

fn block_age() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_block_age"}).body).result.block_age
}

fn name() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_name"}).body).result.name.to_string()
}

fn address() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "peer_addr"}).body).result.peer_addr.to_string()
}

fn consensus() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_in_consensus"}).body).result.in_consensus
}

fn application_status() {
    "broadcasting"
}

fn sync_status() {
    "synced"
}

fn staking_status() {
    "staking"
}

fn generate_keys() {
    run_sh("mkdir -p /opt/secrets && echo first > /opt/secrets/first.key && echo second > /opt/secrets/second.key");
    return;
}
