const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "1.16.0",
    protocol: "solana",
    node_type: "validator",
    description: "Solana node",
    requirements: #{
        vcpu_count: 32,
        mem_size_mb: 131072,
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
                ports: [8899],
            },
        ],
    },
    keys: #{
    },
};

const API_HOST = "http://localhost:8899/";

// TODO
// fn to start node
// possible to use babel vars below ,data_directory_mount_point as example ?
// configure swap?

fn init() {
    debug("Executing startup scripts...");
    let net = sanitize_sh_param(node_params().NETWORK);
    let metadata = global::METADATA.nets[net]
    start_job("Running the Solana validator node", #{
        body: `
        mkdir -p /blockjoy/solana/accounts
        mkdir /blockjoy/solana/config
        curl -sSf https://release.solana.com/v1.16.0/install | sh
        solana config set --url https://api.mainnet.solana.com
        solana-keygen new -o /blockjoy/solana/config/validator-keypair.json
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
    let url = GLOBAL::API_HOST;
    let headers = #{
        "Content-Type": "application/json"
    };

    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getHealth"
    };

    let response = http_post(url, body, headers);

    if response["status_code"] != 200 {
        throw response["body"].to_string();
    }

    let body_json = json::parse(response["body"]).unwrap();
    let result = body_json["result"].to_string();

    if result == "\"ok\"" {
        return "Healthy".to_string();
    } else if result == "\"Node is unhealthy\"" {
        throw result.to_string();
    } else {
        throw response["body"].to_string();
    }
}

fn get_block_height() {
    let url = GLOBAL::API_HOST;

    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBlockHeight"
    };

    let headers = {
        "Content-Type": "application/json"
    };

    let resp = http_post(url, json::stringify(body), headers);
    let response_json = json::parse(resp.body);

    let block_height = response_json["result"].to_string();

    if block_height.is_empty() {
        throw "Failed to retrieve block height";
    }

    return block_height;
}

fn get_identity() {
    let url = GLOBAL::API_HOST;
    let headers = #{
        "Content-Type": "application/json",
    };
    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getIdentity"
    };

    let resp = http_post(url, body, headers);

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
    let headers = #{
        "Content-Type": "application/json",
    };
    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getVersion"
    };

    let resp = http_post(url, body, headers);

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
