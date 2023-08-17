const METADATA = #{
    min_babel_version: "0.31.0",
    kernel: "5.10.174-build.1+fc.ufw",
    node_version: "2.48.1",
    protocol: "gnosis",
    node_type: "node",
    description: "gnosis blockchain",
    requirements: #{
        vcpu_count: 4,
        mem_size_mb: 16384,
        disk_size_gb: 300, 
    },
    nets: #{
        main: #{
            url: "https://gnosischain.com",
            net_type: "main"
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
                ports: [80,30303,9000],
            },
            #{
                name: "Allowed incoming traffic on port",
                action: "allow",
                direction: "in",
                protocol: "udp",
                ports: [9000],
            },
        ],
    },
    keys: #{
    },
};

const API_HOST = "http://localhost:8545";
const HOME = "/blockjoy/gnosis";

fn init(keys) {
    start_job("erigon_gnosis", #{
        job_type: #{
            run_sh: `export HOME=/root && /usr/bin/erigon  --txpool.disable --datadir=${global::HOME} --chain=gnosis --log.dir.path=${global::HOME}/logs --metrics --maxpeers=500 --torrent.download.rate=100mb --http.vhosts=localhost`,
        },
       restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });

    start_job("lighthouse_gnosis", #{
        job_type: #{
            run_sh: `sleep 10 && lighthouse bn --disable-upnp --datadir=${global::HOME}/lighthouse --port=9000 --http --http-address=127.0.0.1 --network=gnosis --target-peers=50 --execution-endpoints=http://localhost:8551 --execution-jwt=${global::HOME}/jwt.hex --debug-level=info --validator-monitor-auto --subscribe-all-subnets --import-all-attestations --metrics --metrics-port=5054 --metrics-address=127.0.0.1 --checkpoint-sync-url=https://checkpoint.gnosischain.com`,
        },
       restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });

    start_job("netdata-visuals", #{
        job_type: #{
            run_sh: `sleep 10 && /opt/netdata/usr/sbin/netdata -D`,
        },
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
    let resp = run_jrpc(#{host: global::API_HOST, method: "/"});
    
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
    let resp = run_jrpc(#{host: global::API_HOST, method: "eth_blockNumber"});
    if resp.status_code != 200 {
      throw resp;
    }
    let h = parse_json(resp.body);
    parse_int(sub_string(h.result,2),16)
}

fn sync_status() {
    let data = #{url: global::API_HOST+"/health",
             method: "getBlockByNumber",
             headers: #{"X-ERIGON-HEALTHCHECK": "synced"}
            };

    let resp = run_rest(data);
    if resp.status_code > 225 {
        "syncing"
      } else {
        "synced"
      }
}
