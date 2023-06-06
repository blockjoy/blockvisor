const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "4.2.0",
    protocol: "eth",
    node_type: "node",
    description: "Ethereum Erigon",
    requirements: #{
        vcpu_count: 4,
        mem_size_mb: 16384,
        disk_size_gb: 3500,
    },
    nets: #{
        mainnet: #{
            // correct url?
            url: "",
            net_type: "main",
        },
        // todo, to add test networks: goerli, gnosis, chiado
    },

    babel_config: #{
        // todo
        // do we need it? can we use vars here?
        data_directory_mount_point: "/blockjoy/erigon/data",
        backoff_timeout_ms: 60000
        backoff_base_ms: 100
        log_buffer_capacity_ln: 1000
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
                ports: [8545],
            },
        ],
    },
    keys: #{
    },
};

const API_HOST = "http://localhost:8545/";
const HOME = "/blockjoy"
const ERIGON_HOME = "/blockjoy/erigon"
const LIGHTHOUSE_HOME = "/blockjoy/lighthouse"

fn init() {
    debug("Init process for erigon...")
    run_sh("wget -O lighthouse.tar.gz https://github.com/sigp/lighthouse/releases/download/v4.2.0/lighthouse-v4.2.0-x86_64-unknown-linux-gnu.tar.gz")
    run_sh("wget -O erigon.tar.gz https://github.com/ledgerwatch/erigon/releases/download/v2.43.1/erigon_2.43.1_linux_amd64.tar.gz")
    run_sh("mkdir -p ${GLOBAL::ERIGON_HOME}/data && mkdir -p ${GLOBAL::LIGHTHOUSE_HOME}")
    run_sh("tar xvzf lighthouse.tar.gz --directory /usr/bin/")
    run_sh("tar xvzf erigon.tar.gz --directory /usr/bin/")
    run_sh("apt install ufw -y")
    debug("Init process finished...")
}

fn start_node() {
    let net = sanitize_sh_param(node_params().NETWORK);
    debug("Executing startup scripts...")
    start_job("Running the erigon eth node", #{
        body: `
        /usr/bin/erigon --datadir=${GLOBAL::ERIGON_HOME}/data --chain=${net} --port=30303 --http.port=8545 --authrpc.port=8551 --torrent.port=42069 --private.api.addr=127.0.0.1:9090 --http --ws --http.api=eth,debug,net,trace,web3,erigon --torrent.download.rate=100mb --metrics --metrics.port=6666 --externalcl --http.vhosts *
        /usr/bin/lighthouse bn --datadir ${GLOBAL::LIGHTHOUSE_HOME} --checkpoint-sync-url https://beaconstate.ethstaker.cc --http --http-address 127.0.0.1 --http-port 6868 --execution-endpoint http://localhost:8551 --execution-jwt /blockjoy/erigon/data/jwt.hex --metrics
        `
        restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
   ) };
}

// todo to check
fn application_status() -> String {
    let request_body = r#"
        {
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 67
        }
    "#;

    let response = parse_json(run_jrpc(global::API_HOST, request_body));

    if let Some(block_number) = response.get("result") {
        format!("Latest block number: {}", block_number)
    } else {
        "Response does not contain block number".to_string()
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

// todo
// fn main() {
//     start_node should be added here?
// }