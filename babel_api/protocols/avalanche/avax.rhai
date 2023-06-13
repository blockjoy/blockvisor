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
        backoff_timeout_ms: 60000,
        backoff_base_ms: 100,
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

// const API_HOST = "http://localhost:9650/ext/bc/";
const API_HOST = "http://localhost:9650/ext/";
const HOME = "/blockjoy";
const AVAX_HOME = "/blockjoy/avalanche";


fn init() {
    // let net = sanitize_sh_param(node_params().NETWORK); for future use
    debug("Executing startup scripts...");
    start_job("Running the avalanche validator node", #{
        body: `
        wget https://github.com/ava-labs/avalanchego/releases/download/v1.9.9/avalanchego-linux-amd64-v1.9.9.tar.gz
        tar xvzf avalanchego-linux-amd64-v1.9.9.tar.gz
        sudo cp avalanchego-v1.9.9/avalanchego /usr/bin/
        avalanchego --version
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
    const method = "info.isBootstrapped";
    const path = "info";
    let params = {
        "chain": "X"
    };
    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    };
    let headers = {
        "Content-Type": "application/json"
    };
    let url = GLOBAL::API_HOST + path;

    let resp = http_post(url, json::stringify(body), headers);
    if resp.status_code != 200 {
        throw resp;
    }

    let bodyJson = json::parse(resp.body);
    let result = bodyJson["result"]["isBootstrapped"];
    return result;
}

fn get_health() {
    const path = "health";
    let url = GLOBAL::API_HOST + path;

    let resp = http_post(url);
    if resp.status_code != 200 {
        throw resp;
    }
  
    let body = json::parse(resp.body);
    let healthy = body["result"]["healthy"];
    return healthy;
}

fn get_metrics() {
    const path = "metrics";
    let url = GLOBAL::API_HOST + path;

    let resp = http_post(url);
    if resp.status_code != 200 {
        throw resp;
    }
    return resp.body;
}

fn get_node_id() {
    const method = "info.getNodeID";
    const path = "info";
    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": method
    };
    let headers = {
        "Content-Type": "application/json"
    };
    let url = GLOBAL::API_HOST + path;

    let resp = http_post(url, json::stringify(body), headers);
    if resp.status_code != 200 {
        throw resp;
    }

    let bodyJson = json::parse(resp.body);
    let result = bodyJson["result"];
    return result;
}

fn get_node_version() {
    const method = "info.getNodeVersion";
    const path = "info";
    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": method
    };
    let headers = {
        "Content-Type": "application/json"
    };
    let url = API_HOST + path;

    let resp = http_post(url, json::stringify(body), headers);
    if resp.status_code != 200 {
        throw resp;
    }

    let bodyJson = json::parse(resp.body);
    let result = bodyJson["result"];
    return result;
}

fn stop_babelsup(param) {
    run_sh("systemctl stop babelsup");
    for line in run_sh("ps -e|grep babel").split("\n") {
        let parts = line.split();
        if !parts.is_empty() {
            let pid = parts[0];
            pid.trim();
            if !pid.is_empty() {
                run_sh("kill -9 " + pid);
            }
        }
    }
    ""
}

fn disable_babelsup(param) {
    run_sh("systemctl disable babelsup")
}
