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
        },
        testnet: #{
            url: "https://fullnode.testnet.aptoslabs.com/v1",
            net_type: "test",
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

fn init(keys) {
    debug("Executing startup scripts...");
    start_job("Running the Aptos validator node", #{
        body: `
        cd /blockjoy/aptos/aptos-node
        cargo run -p aptos-node --release -- -f ../fullnode.yaml
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

fn application_status() {
    let net = sanitize_sh_param(node_params().NETWORK);
    let path = "/-/healthy";
    let url = global::METADATA.nets[net]["url"] + path;
    let headers = {
        "Accept": "application/json, application/x-bcs"
    };

    let resp = run_jrpc(url, {}, headers);

    if resp.status_code != 200 {
        throw resp;
    }

    let response_json = json::parse(resp.body);

    return response_json["message"];
}

fn get_ledger_info() {
    let net = sanitize_sh_param(node_params().NETWORK);
    let path = "/";
    let url = global::METADATA.nets[net]["url"] + path;
    let headers = {
        "Accept": "application/json, application/x-bcs"
    };

    let resp = run_jrpc(url, {}, headers);

    if resp.status_code != 200 {
        throw resp;
    }

    let response_json = json::parse(resp.body);

    let chain_id = response_json["chain_id"];
    let epoch = response_json["epoch"];
    let ledger_version = response_json["ledger_version"];
    let oldest_ledger_version = response_json["oldest_ledger_version"];
    let ledger_timestamp = response_json["ledger_timestamp"];
    let node_role = response_json["node_role"];
    let oldest_block_height = response_json["oldest_block_height"];
    let block_height = response_json["block_height"];
    let git_hash = response_json["git_hash"];

    let result = #{
        "chain_id": chain_id,
        "epoch": epoch,
        "ledger_version": ledger_version,
        "oldest_ledger_version": oldest_ledger_version,
        "ledger_timestamp": ledger_timestamp,
        "node_role": node_role,
        "oldest_block_height": oldest_block_height,
        "block_height": block_height,
        "git_hash": git_hash
    };

    return result;
}
