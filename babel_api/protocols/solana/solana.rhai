const METADATA = {
    min_babel_version: "0.15.4",
    node_version: "1.16.0",
    protocol: "solana",
    node_type: "validator",
    description: "Solana node",
    requirements: {
      vcpu_count: 32,
      mem_size_mb: 524288,
      disk_size_gb: 1500
    },
    nets: {
      mainnet: {
        url: "https://api.mainnet.solana.com",
        net_type: "main",
        entrypoints: [
          "entrypoint.mainnet-beta.solana.com:8001",
          "entrypoint2.mainnet-beta.solana.com:8001",
          "entrypoint3.mainnet-beta.solana.com:8001",
          "entrypoint4.mainnet-beta.solana.com:8001",
          "entrypoint5.mainnet-beta.solana.com:8001"
        ],
      },
      testnet: {
        url: "https://api.testnet.solana.com",
        net_type: "test",
        entrypoints: [
          "entrypoint.testnet.solana.com:8001",
          "entrypoint2.testnet.solana.com:8001",
          "entrypoint3.testnet.solana.com:8001"
        ],
      },
    },
    babel_config: {
      data_directory_mount_point: "/blockjoy/solana",
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
          ports: [8899]
        },
      ],
    },
    keys: {},
  };
  
  const API_HOST = "http://localhost:8899/";
  
  // ...
  
  fn init() {
    debug("Executing startup scripts...");
    let net = sanitize_sh_param(node_params().NETWORK);
    let metadata = GLOBAL::METADATA.nets[net];
    let data_directory = GLOBAL::METADATA.babel_config.data_directory_mount_point;
    let entrypoints = metadata.entrypoints.map(entrypoint => `--entrypoint ${entrypoint}`).join(" ");
  
    start_job("Init of the Solana validator node", {
      body: `solana config set --url ${metadata.url}\nsolana-keygen new -o ${data_directory}/config/validator-keypair.json`,
      restart: {
        "always": {
          backoff_timeout_ms: 60000,
          backoff_base_ms: 10000
        },
      },
      needs: [],
    });
  
    start_job("Starting the Solana node", {
      body: `solana-validator --accounts ${data_directory}/accounts \
              --ledger ${data_directory}/ledger \
              --identity ${data_directory}/config/validator-keypair.json \
              --expected-genesis-hash ???? \
              ${entrypoints} \
              --no-voting \
              --snapshot-interval-slots 500 \
              --maximum-local-snapshot-age 500 \
              --rpc-bind-address 127.0.0.1 \
              --rpc-port 8899 \
              --gossip-port 8801 \
              --dynamic-port-range 8802-8902 \
              --no-port-check \
              --wal-recovery-mode skip_any_corrupted_record \
              --enable-rpc-transaction-history \
              --enable-cpi-and-log-storage \
              --init-complete-file ${data_directory}/init-completed \
              --snapshot-compression none \
              --require-tower \
              --no-wait-for-vote-to-start-leader \
              --no-poh-speed-test \
              --limit-ledger-size 300000000 \
              --log - \
              --account-index program-id spl-token-owner \
              --account-index-exclude-key ??? \
              --account-index-exclude-key ??? \
              --full-rpc-api`,
      restart: {
        "always": {
          backoff_timeout_ms: 60000,
          backoff_base_ms: 10000,
        },
      },
      needs: ["Init of the Solana validator node"],
    });
  }  

fn application_status() {
    let url = GLOBAL::API_HOST;
    let headers = {
        "Content-Type": "application/json",
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
    let headers = {
        "Content-Type": "application/json",
    };
  
    let body = {
      "jsonrpc": "2.0",
      "id": 1,
      "method": "getBlockHeight"
    };
  
    let resp = run_jrpc(url, body, headers);
    let response_json = json::parse(resp["body"]).unwrap();
    let block_height = response_json["result"].to_string();
  
    if block_height.is_empty() {
      throw "Failed to retrieve block height";
    }
  
    block_height
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
