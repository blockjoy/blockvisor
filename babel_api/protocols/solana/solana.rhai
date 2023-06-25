const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "1.16.0",
    protocol: "solana",
    node_type: "validator",
    description: "Solana node",
    requirements: #{
        vcpu_count: 32,
        mem_size_mb: 524288,
        disk_size_gb: 1500,
    },
    nets: #{
        mainnet: #{
            url: "https://api.mainnet.solana.com",
            net_type: "main",
        },
        testnet: #{
            url: "https://api.testnet.solana.com",
            net_type: "test",
        },
    },
    babel_config: #{
        data_directory_mount_point: "/blockjoy/solana/",
        log_buffer_capacity_ln: 1000,
        swap_size_mb: 393216,
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
                ports: [8899],
            },
        ],
    },
    keys: #{
    },
};

const API_HOST = "http://localhost:8899/";

// TODO
// start node/create start file/ other way to start solana?

fn init() {
    debug("Executing startup scripts...");
    let net = sanitize_sh_param(node_params().NETWORK);
    let metadata = GLOBAL::METADATA.nets[net];
    let data_directory = GLOBAL::METADATA.babel_config.data_directory_mount_point;
    
    start_job("Running the Solana validator node", #{
        body: format!(
            "solana config set --url {}\nsolana-keygen new -o {}/config/validator-keypair.json",
            metadata.url,
            data_directory
        ),
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
    let url = GLOBAL::API_HOST;
    let headers = {
        "Content-Type": "application/json"
    };

    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getHealth"
    };

    let resp = run_jrpc(url, body, headers);

    if resp["status_code"] != 200 {
        throw resp["body"].to_string();
    }

    let body_json = json::parse(resp["body"]).unwrap();
    let result = body_json["result"];

    return result;
}

fn address() {
    let url = GLOBAL::API_HOST;

    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBlockHeight"
    };

    let headers = {
        "Content-Type": "application/json"
    };

    let resp = run_jrpc(url, body, headers);
    let response_json = json::parse(resp["body"]);

    let block_height = response_json["result"].to_string();

    if block_height.is_empty() {
        throw "Failed to retrieve block height";
    }

    return block_height;
}

fn get_identity() {
    let url = GLOBAL::API_HOST;
    let headers = {
        "Content-Type": "application/json",
    };
    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getIdentity"
    };

    let resp = run_jrpc(url, body, headers);

    if resp["status_code"] != 200 {
        throw resp["body"];
    }

    let body_json = json::parse(resp["body"]);

    if body_json.is_err() {
        throw "Invalid response body";
    }

    let identity = body_json.unwrap()["result"]["identity"];

    return identity;
}

fn get_node_version() {
    let url = GLOBAL::API_HOST;
    let headers = {
        "Content-Type": "application/json",
    };
    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getVersion"
    };

    let resp = run_jrpc(url, body, headers);

    if resp["status_code"] != 200 {
        throw resp["body"];
    }

    let body_json = json::parse(resp["body"]);

    if body_json.is_err() {
        throw "Invalid response body";
    }

    let version = body_json.unwrap()["result"]["solana-core"];

    return version;
}
