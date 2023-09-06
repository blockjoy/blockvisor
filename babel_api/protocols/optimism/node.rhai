const METADATA = #{
    min_babel_version: "0.34.0",
    kernel: "5.10.174-build.1+fc.ufw",
    node_version: "1.101200.1",
    protocol: "ethereum",
    node_type: "node",
    description: "optimism blockchain",
    requirements: #{
        vcpu_count: 4,
        mem_size_mb: 16384,
        disk_size_gb: 2000, 
    },
    nets: #{
        main: #{
            sequencer_url: "https://mainnet-sequencer.optimism.io/",
            net_type: "main",
            url: "https://mainnet.optimism.io",
            l1_url: "https://gorilla01.lax.blockjoy.com",
            snapshot_url: "https://datadirs.optimism.io/mainnet-bedrock.tar.zst",
        },
        test: #{
            sequencer_url: "https://goerli-sequencer.optimism.io/",
            net_type: "test",
            url: "https://goerli.optimism.io",
            l1_url: "https://gorilla01.lax.blockjoy.com",
            snapshot_url: "https://datadirs.optimism.io/goerli-bedrock.tar.zst",
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
                ports: [9992,9991,8551,9545,8545,8547],
            },
            #{
                name: "Allowed incoming traffic on port",
                action: "allow",
                direction: "in",
                protocol: "udp",
                ports: [9992,9991,8551,9545,8545,8547],
            },
        ],
    },
    keys: #{},
};

const ETH_API = "http://localhost:8545";
const OPTIMISM_API = "http://localhost:8547";


fn init(keys) {
    let net = sanitize_sh_param(node_params().NETWORK);
    let optimism_directory = global::METADATA.babel_config.data_directory_mount_point + "optimism";

   start_job("initialize", #{
        job_type: #{
            run_sh: `mkdir -p /blockjoy/optimism && \
            openssl rand -hex 32 > /blockjoy/optimism/jwt.txt && \
            apt-get install zstd -y && \
            wget -O /blockjoy/optimism/bedrock.tar.zst ${global::METADATA.nets[node_params().NETWORK].snapshot_url} && \
            zstd -d -o /blockjoy/optimism/bedrock.tar /blockjoy/optimism/bedrock.tar.zst && \
            tar -xvf /blockjoy/optimism/bedrock.tar -C /blockjoy/optimism`
        },
        restart: #{
            "on_failure": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
                max_retries: 3,
            },
        },
        needs: [],
    });

    start_job("op-geth", #{
        job_type: #{
            run_sh: `geth \
            --ws \
            --ws.port=8546 \
            --ws.addr=0.0.0.0 \
            --ws.origins="*" \
            --http \
            --http.port=8545 \
            --http.addr=0.0.0.0 \
            --http.vhosts="*" \
            --http.corsdomain="*" \
            --authrpc.addr=localhost \
            --authrpc.jwtsecret=/blockjoy/optimism/jwt.txt \
            --authrpc.port=8551 \
            --authrpc.vhosts="*" \
            --verbosity=3 \
            --rollup.sequencerhttp="https://mainnet-sequencer.optimism.io" \
            --nodiscover \
            --syncmode=full \
            --maxpeers=0 \
            --datadir=/blockjoy/optimism/geth \
            --snapshot=false \
            --networkid=10`
        },
    restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: ["initialize"],
    });
      
    start_job("op-node", #{
        job_type: #{
            run_sh: `op-node \
            --l1=https://gorilla01.lax.blockjoy.com \
            --l2=http://localhost:8551 \
            --network=mainnet \
            --rpc.addr=0.0.0.0 \
            --rpc.port=8547 \
            --l2.jwt-secret=/blockjoy/optimism/jwt.txt`
        },
    restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: ["op-geth"],
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
        needs: ["op-node"],
    });
}

fn application_status() {
    let resp = run_jrpc(#{host: global::ETH_API, method: ""});
    
    if resp.status_code == 200 {
        "broadcasting"
    } else {
        "delinquent"
    }
}

fn height() {
    let resp = run_jrpc(#{host: global::ETH_API, method: "eth_blockNumber"});
    if resp.status_code != 200 {
    throw resp;
    }
    let h = parse_json(resp.body);
    parse_int(sub_string(h.result,2),16)
}

fn sync_status() {
    let data = #{url: global::OPTIMISM_API,
            method: "optimism_syncStatus",
            };

    let resp = run_rest(data);
    if resp.status_code >= 200 {
        "syncing"
    } else {
        "synced"
    }
}



