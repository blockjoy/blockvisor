// TO check
// sync_status function on the bottom of script
// entrypoints list from nets section in start command 
// --account-index params in start command

const METADATA = #{
  min_babel_version: "0.15.4",
  node_version: "1.16.0",
  protocol: "solana",
  node_type: "validator",
  description: "Solana node",
  requirements: #{
    vcpu_count: 32,
    mem_size_mb: 524288,
    disk_size_gb: 1500
  },
  nets: #{
    mainnet: #{
      url: "https://api.mainnet-beta.solana.com",
      net_type: "main",
      entrypoints: [
        "entrypoint.mainnet-beta.solana.com:8001",
        "entrypoint2.mainnet-beta.solana.com:8001",
        "entrypoint3.mainnet-beta.solana.com:8001",
        "entrypoint4.mainnet-beta.solana.com:8001",
        "entrypoint5.mainnet-beta.solana.com:8001"
      ],
    },
    testnet: #{
      url: "https://api.testnet.solana.com",
      net_type: "test",
      entrypoints: [
        "entrypoint.testnet.solana.com:8001",
        "entrypoint2.testnet.solana.com:8001",
        "entrypoint3.testnet.solana.com:8001"
      ],
    },
  },
  babel_config: #{
    data_directory_mount_point: "/blockjoy/solana",
    log_buffer_capacity_ln: 1000,
    swap_size_mb: 393216
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
        ports: [8899]
      },
    ],
  },
  keys: #{},
};

const API_HOST = "http://localhost:8899/";

fn init(keys) {
  debug("Executing startup scripts...");
  let net = sanitize_sh_param(node_params().NETWORK);
  let metadata = GLOBAL::METADATA.nets[net];
  let data_directory = GLOBAL::METADATA.babel_config.data_directory_mount_point;
  let entrypoints = "";
  for entrypoint in metadata.entrypoints {
    entrypoints = entrypoints + "--entrypoint " + entrypoint + " ";
  }
  start_job("Init of the Solana validator node", #{
      job_type: #{
          run_sh:
          `solana config set --url ${metadata.url}\nsolana-keygen new -o ${data_directory}/config/validator-keypair.json`,
      },
      restart: #{
          "always": #{
              backoff_timeout_ms: 60000,
              backoff_base_ms: 10000
           },
      },
      needs: [],
  });

  start_job("Starting the Solana node", #{
      job_type: #{
          run_sh:
          `solana-validator --accounts ${data_directory}/accounts \
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
      },
      restart: #{
          "always": #{
              backoff_timeout_ms: 60000,
              backoff_base_ms: 10000,
          },
      },
      needs: ["Init of the Solana validator node"],
  });
}

fn application_status() {
  let resp = run_jrpc(global::API_HOST, "getHealth");
  
  if resp.status_code == 200 {
      "Running"
  } else {
      "Node is unhealthy"
  }
}

fn height() {
  let resp = run_jrpc(global::API_HOST, "getBlockHeight");
  
  if resp.status_code != 200 {
      throw resp.status_code;
  }
  parse_json(resp.result)
}

fn address() {
  let resp = run_jrpc(global::API_HOST, "getIdentity");
  
  if resp.status_code != 200 {
      throw resp.status_code;
  }
  parse_json(resp.result)
}

//TBD
fn sync_status() {
  let resp = run_jrpc(global::API_HOST, "getBlockHeight");
  
  if resp.status_code != 200 {
    echo("Failed to get response");
    return false;
  }
  
  let latestBlockHeight = parse_json(resp.result);
  
  let blockchainBlockHeight = get_latest_block_height_from_blockchain();
  
  let blockHeightDifference = blockchainBlockHeight - latestBlockHeight;
  
  if blockHeightDifference > 100 {
    echo("Still Syncing");
    return false;
  }
  echo("Synced");
  true
}

//TBD
fn get_latest_block_height_from_blockchain() {
  let resp = run_jrpc(METADATA.nets.mainnet.url, "getBlockHeight");
  
  if resp.status_code != 200 {
    throw "Latest block height from the blockchain not found";
  }
  
  let result = parse_json(resp.body);
  result.blockHeight
}