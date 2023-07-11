const METADATA = #{
    min_babel_version: "0.18.0",
    node_version: "2.48.0",
    protocol: "erigon",
    node_type: "node",
    description: "Ethereum Erigon",
    requirements: #{
        vcpu_count: 4,
        mem_size_mb: 16384,
        disk_size_gb: 3072, 
    },
    nets: #{
        main: #{
            url: "http://ethereum.org",
            net_type: "main",
        },
    },

    babel_config: #{
        data_directory_mount_point: "/blockjoy/erigon/",
        backoff_timeout_ms: 60000,
        backoff_base_ms: 100,
        log_buffer_capacity_ln: 1000,
    swap_size_mb: 4096,
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
                ports: [30303,30304,8545,9090],
            },
        ],
    },
    keys: #{
    },
};

const API_HOST = "http://localhost:8545/";
const HOME = "/blockjoy";
const ERIGON_HOME = "/blockjoy/erigon";
const LIGHTHOUSE_HOME = "/blockjoy/lighthouse";

fn init(keys) {
    let net = sanitize_sh_param(node_params().NETWORK);
    debug("Executing startup scripts...");
    start_job("erigon", #{
        body: `
        /usr/bin/erigon --datadir=${global::ERIGON_HOME}/data --chain=${net}net --port=30303 --http.port=8545 --authrpc.port=8551 --torrent.port=42069 --private.api.addr=0.0.0.0:9090 --http --ws --http.api=eth,debug,net,trace,web3,erigon --torrent.download.rate=100mb --metrics --metrics.port=6666 --http.vhosts localhost
        `,
       restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });

    start_job("lighthouse", #{
        body: `
        /usr/bin/lighthouse bn --datadir ${global::LIGHTHOUSE_HOME} --checkpoint-sync-url https://beaconstate.ethstaker.cc --http --http-address 127.0.0.1 --http-port 6868 --execution-endpoint http://localhost:8551 --execution-jwt /blockjoy/erigon/data/jwt.hex --metrics
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
    "broadcasting"
}

fn address() {
    let api_key = run_sh(`cat ${global::ERIGON_HOME}/data/jwt.hex`);
    api_key.stdout
}

fn height() {
    let resp = run_jrpc(global::API_HOST, "eth_blockNumber");
    if resp.status_code != 200 {
      throw resp;
    }
    let h = parse_json(resp.body);
    parse_int(sub_string(h.result,2),16)
}

//curl -v --location --request GET 'http://localhost:8545/health' 
fn sync_status() {
    let resp = run_rest("http://localhost:8545/health");
    debug(resp);
    if resp.status_code != 200 {
        "syncing"
      } else {
        "synced"
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
