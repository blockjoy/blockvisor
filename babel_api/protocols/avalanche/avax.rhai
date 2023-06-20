const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "1.9.9",
    protocol: "avax",
    node_type: "validator",
    description: "Avalanche node",
    requirements: #{
        vcpu_count: 4,
        mem_size_mb: 16384,
        disk_size_gb: 1000,
    },
    nets: #{
        mainnet: #{
            url: "https://api.avax.network/ext/bc/C/rpc",
            net_type: "main",
        },
        testnet: #{
            url: "https://api.avax-test.network/ext/bc/C/rpc",
            net_type: "test",
        },

    },

    babel_config: #{
        data_directory_mount_point: "/blockjoy/avalanche/",
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
                ports: [9650],
            },
        ],
    },
    keys: #{
    },
};

const API_HOST = "http://localhost:9650/ext/";
const HOME = "/blockjoy";
const AVAX_HOME = "/blockjoy/avalanche";
const HEADERS = {
    "Content-Type": "application/json"
};

fn init(keys) {
    debug("Executing startup scripts...");
    start_job("Running the avalanche validator node", #{
        body: `
        avalanchego
        `,
        restart: {
            "always": {
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });
}

fn make_jrpc_request(path, method, params) {
    let url = GLOBAL::API_HOST + path;

    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    };

    let resp = run_jrpc(url, body, HEADERS);
    if resp.status_code != 200 {
        throw resp;
    }

    let response_json = json::parse(resp.body);
    return response_json["result"];
}

fn application_status() {
    let method = "info.isBootstrapped";
    let path = "info";
    let params = {
        "chain": "X"
    };

    return make_jrpc_request(path, method, params);
}

fn get_health() {
    let method = "health.health";
    let path = "health";

    return make_jrpc_request(path, method, {});
}

fn get_metrics() {
    let path = "metrics";

    return make_jrpc_request(path, {}, {});
}

fn get_node_id() {
    let method = "info.getNodeID";
    let path = "info";

    return make_jrpc_request(path, method, {});
}

fn get_node_version() {
    let method = "info.getNodeVersion";
    let path = "info";

    return make_jrpc_request(path, method, {});
}