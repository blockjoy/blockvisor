const METADATA = {
    min_babel_version: "0.15.4",
    node_version: "3.16.3",
    protocol: "algorand",
    node_type: "validator",
    description: "Algorand node",
    requirements: {
      vcpu_count: 32,
      mem_size_mb: 32768,
      disk_size_gb: 1500
    },
    nets: {
      mainnet: {
        url: "https://algorand-catchpoints.s3.us-east-2.amazonaws.com/channel/mainnet/latest.catchpoint",
        net_type: "main",
      },
      testnet: {
        url: "https://algorand-catchpoints.s3.us-east-2.amazonaws.com/channel/testnet/latest.catchpoint",
        net_type: "test",
      },
    },
    babel_config: {
      data_directory_mount_point: "/blockjoy/algorand",
      log_buffer_capacity_ln: 1000,
      swap_size_mb: 393216
    },
    firewall: {
      enabled: true,
      default_in: "deny",
      default_out: "allow",
      rules: [
        {
          name: "Allowed incoming traffic on port",
          action: "allow",
          direction: "in",
          protocol: "tcp",
          ports: [8080, 7833]
        },
      ],
    },
    keys: {},
};
  
const API_HOST = "http://localhost:8080";
  
fn init() {
    debug("Executing startup scripts...");
  
    let net = sanitize_sh_param(node_params().NETWORK);
    let metadata = GLOBAL::METADATA.nets[net];
    let data_directory = GLOBAL::METADATA.babel_config.data_directory_mount_point;
  
    start_job("Entrypoint for the Algorand validator node", {
        body: format!(
            "/usr/bin/algod -d {}/data -g {}/genesis/{}/genesis.json -o",
            data_directory, data_directory, net
        ),
        restart: {
            "always": {
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000
            },
        },
        needs: [],
    });
}
  
fn make_jrpc_request(path, params) {
    //main jrpc function to reduce duplications
    let headers = {
        "Content-Type": "application/json",
    };

    let body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "GET",
    };

    let full_url = format!("{}{}", API_HOST, path);
    let resp = run_jrpc(full_url, body, headers);

    if resp["status_code"] != 200 {
        throw format!("Request failed with error: {}", resp["status_code"]);
    }

    resp["body"]
}

fn get_genesis_info() {
    // Gets the genesis information.

    let path = "/genesis";
    let response = make_jrpc_request(path, {});

    let genesis_info = json::parse(response);

    if let Err(err) = genesis_info {
        throw format!("Failed to parse genesis information: {}", err);
    }

    genesis_info.unwrap()
}

fn application_status() {
    // Returns OK if healthy and fully caught up.

    let path = "/ready";
    let response = make_jrpc_request(path, {});

    if response["status_code"] != 200 {
        throw format!("Node readiness check failed with error: {}", response["status_code"]);
    }

    response["body"]
}

fn metrics() {
    // Returns metrics about algod functioning.

    let path = "/metrics";
    let response = make_jrpc_request(path, {});

    if response["status_code"] != 200 {
        throw format!("Failed to retrieve node metrics with error: {}", response["status_code"]);
    }

    response["body"]
}

fn health() {
    // Returns OK if healthy.

    let path = "/health";
    let response = make_jrpc_request(path, {});

    if response["status_code"] != 200 {
        throw format!("Node health check failed with error: {}", response["status_code"]);
    }

    response["body"]
}