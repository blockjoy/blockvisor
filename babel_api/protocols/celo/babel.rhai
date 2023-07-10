const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "1.7.4",
    protocol: "celo",
    node_type: "node",
    description: "Celo Full Node",
    requirements: #{
        vcpu_count: 4,
        mem_size_mb: 16384,
        disk_size_gb: 512, 
    },
    nets: #{
        main: #{
            url: "https://docs.celo.org/network/node/run-mainnet",
            net_type: "main",
            genesis_url: "https://www.googleapis.com/storage/v1/b/genesis_blocks/o/mainnet?alt=media",
            bootnodes_url: "https://www.googleapis.com/storage/v1/b/env_bootnodes/o/mainnet?alt=media",
            staticnodes_url: "https://www.googleapis.com/storage/v1/b/static_nodes/o/mainnet?alt=media",
        },
    },

    babel_config: #{
        data_directory_mount_point: "/blockjoy/",
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
                ports: [8545,9090],
            },
        ],
    },
    keys: #{
    },
};

const API_HOST = "http://localhost:8545";
const HOME = "/blockjoy";

fn init(keys) {
    debug("Executing startup scripts...");

    start_job("download_prerequisites", #{
        job_type: #{ 
            run_sh: `mkdir -p /blockjoy/celo && curl ${global::METADATA.nets[node_params().NETWORK].genesis_url} > ${global::HOME}/celo/genesis.json && curl ${global::METADATA.nets[node_params().NETWORK].bootnodes_url} > ${global::HOME}/celo/bootnodes && curl ${global::METADATA.nets[node_params().NETWORK].staticnodes_url} > ${global::HOME}/celo/static-nodes.json`,
        },
       restart: #{
            "on_failure": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });

    start_job("celo", #{
        job_type: #{
            run_sh: `
            sleep 30 && /usr/bin/celo --verbosity 3 --syncmode full --http --http.port 9154 --port 40405 --metrics.port 7770 --http.addr 0.0.0.0 --http.api eth,net,web3,debug,admin,personal --light.serve 9090 --light.maxpeers 20 --maxpeers 5000 --datadir ${global::HOME} --http.vhosts localhost --gcmode archive --txlookuplimit=0 --cache.preimages
            `,
        },
       restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
    });
}

fn application_status() {
    let resp = run_jrpc(global::API_HOST, "/");
    
    if resp.status_code == 200 {
        "broadcasting"
    } else {
        "delinquent"
    }
}

fn address() {
    parse_json(run_jrpc(global::API_HOST, "eth_coinbase")).result.to_string()
}

fn height() {
    let resp = run_jrpc(global::API_HOST, "eth_blockNumber");
    if resp.status_code != 200 {
      throw resp;
    }
    let h = parse_json(resp.body);
    parse_int(sub_string(h.result,2),16)
}

fn sync_status() {
    let resp = run_jrpc(global::API_HOST, "eth_syncing");
    let data = parse_json(resp.body);
    let hb = parse_int(sub_string(data.result.highestBlock,2),16);
    let cb = parse_int(sub_string(data.result.currentBlock,2),16);
    let diff = hb-cb;
    debug(resp);
    if resp.status_code != 200 {
        throw resp;
      }
      if diff < 25 {
        "synced"
      } else {
        "syncing"
      }
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
