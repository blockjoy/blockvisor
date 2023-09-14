const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "v0.3.5",
    protocol: "polygon",
    node_type: "archive node",
    description: "Polygon Archive Node",
    kernel: "5.10.174-build.1+fc.ufw",
    requirements: #{
        vcpu_count: 16,
        mem_size_mb: 65536,
        disk_size_gb: 2048, // Mumbai Testnet: 1TB Polygon Mainnet: 8TB
    },

    nets: #{
        mumbai: #{
            url: "https://polygon-mumbai-bor.publicnode.com",
            heimdall_seeds: "9df7ae4bf9b996c0e3436ed4cd3050dbc5742a28@43.200.206.40:26656,d9275750bc877b0276c374307f0fd7eae1d71e35@54.216.248.9:26656,1a3258eb2b69b235d4749cf9266a94567d6c0199@52.214.83.78:26656",
            heimdall_snapshot: "https://snapshot-download.polygon.technology/heimdall-mumbai-parts.txt",
            erigon_snapshot: "https://snapshot-download.polygon.technology/erigon-mumbai-parts.txt",
            net_type: "test",
          },
        mainnet: #{
            url: "https://polygon.llamarpc.com",
            heimdall_seeds: "1500161dd491b67fb1ac81868952be49e2509c9f@52.78.36.216:26656,dd4a3f1750af5765266231b9d8ac764599921736@3.36.224.80:26656,8ea4f592ad6cc38d7532aff418d1fb97052463af@34.240.245.39:26656,e772e1fb8c3492a9570a377a5eafdb1dc53cd778@54.194.245.5:26656,6726b826df45ac8e9afb4bdb2469c7771bd797f1@52.209.21.164:26656",
            heimdall_snapshot: "https://snapshot-download.polygon.technology/heimdall-mainnet-parts.txt",
            erigon_snapshot: "https://snapshot-download.polygon.technology/erigon-mainnet-parts.txt",
            net_type: "main",
        }
    },

    babel_config: #{
        data_directory_mount_point: "/blockjoy/polygon/data",
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 4096,
    },

    // https://wiki.polygon.technology/docs/pos/reference/default-ports/
    firewall: #{
        enabled: true,
        default_in: "deny",
        default_out: "allow",

        rules: [
            #{
                name: "Allowed incoming tcp traffic on port",
                action: "allow",
                direction: "in",
                protocol: "tcp",
                ports: [8545,8546,80],
            },
        ],
    },

    keys: #{}
};

// const KLAYTN_RPC_URL = "http://localhost:8551";
// const KLAYTN_DOWNLOADS_DIR = global::METADATA.babel_config.data_directory_mount_point + "/downloads/";
// const KEN_DOWNLOAD_PATH = global::KLAYTN_DOWNLOADS_DIR + "ken.tar.gz";
// const KLAYTN_ARCHIVE_PATH = global::KLAYTN_DOWNLOADS_DIR +"klatyn-chaindata.tar.gz";
// const KEN_INSTALL_PATH = "/var/lib/klaytn";
// const KEN_PATH = `${global::KEN_INSTALL_PATH}/ken-linux-amd64/bin`;
const ERIGON_VERSION = "v0.0.8"
fn init(keys) {
    let net = node_params().NETWORK

    let polygon_init_commands = [
        // Install Heimdall
        `curl -L https://raw.githubusercontent.com/maticnetwork/install/main/heimdall.sh | bash -s -- ${global::METADATA.node_version} ${net} sentry}`,
        `chown heimdall /var/lib/heimdall`,
        `sudo -u heimdall heimdalld init --chain=${net} --home /var/lib/heimdall`,
        // Install Erigon
        `git clone --recurse-submodules -j8 --branch ${global::ERIGON_VERSION} https://github.com/maticnetwork/erigon.git`,
        `cd erigon && make erigon`,
        `mv ./build/bin/erigon /usr/bin/`,                                     
        `wget ${global::METADATA.nets[node_params().NETWORK].package_url} -O ${global::KEN_DOWNLOAD_PATH}`,
        `mkdir -p ${global::KEN_INSTALL_PATH}`,
        `tar xvf ${global::KEN_DOWNLOAD_PATH} -C ${global::KEN_INSTALL_PATH}`,
        `sed -i -e "s|DATA_DIR=.*|DATA_DIR=${global::METADATA.babel_config.data_directory_mount_point} |" ${global::KEN_INSTALL_PATH}/ken-linux-amd64/conf/kend.conf`,
        `sed -i -e 's|RPC_API=.*|RPC_API="admin,debug,klay,eth,miner,net,personal,rpc,txpool,web3"|' ${global::KEN_INSTALL_PATH}/ken-linux-amd64/conf/kend.conf`,
        `sed -i -e 's|RPC_ADDR=.*|RPC_ADDR="127.0.0.1"|' ${global::KEN_INSTALL_PATH}/ken-linux-amd64/conf/kend.conf`,
        `rm -rf /opt/netdata/var/lib/netdata/*`,
        `rm -rf /opt/netdata/var/cache/netdata/*`,
        `apt-get install nginx -y`,
        `systemctl enable nginx`,
        `sed -i '/location \/ {/,/}/d' /etc/nginx/sites-enabled/default`,
        `sed -i '/server_name _;/a \\nlocation \/ {\n    proxy_set_header Host $host;\n    proxy_set_header X-Real-IP $remote_addr;\n    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n    proxy_set_header X-Forwarded-Proto $scheme;\n    proxy_pass http://localhost:8551;\n    proxy_read_timeout 90;\n}' /etc/nginx/sites-enabled/default`,
        `systemctl restart nginx`,
        `ufw allow http`,
    ];

    for (cmd, i) in klaytn_init_commands {
        let response = run_sh(cmd);
        debug(`Response from shell command ${cmd}':`);
        debug(response);
    
        if response.exit_code != 0 {
            debug(`Command at index ${i} failed: '${cmd}'`);
        }
    }
    
    // start_job("download", #{
    //     job_type: #{
    //         run_sh: `wget ${global::METADATA.nets.cypress.archive_url} -O ${global::KLAYTN_ARCHIVE_PATH} && tar xvf ${global::KLAYTN_ARCHIVE_PATH} -C ${global::METADATA.babel_config.data_directory_mount_point}`,
    //     },
    //     restart: #{
    //         on_failure: #{
    //             backoff_timeout_ms: 60000,
    //             backoff_base_ms: 10000,
    //             max_retries: 3,
    //         },
    //     },
    //     needs: [],
    // });
    start_job("start_erigon", #{
        job_type: #{
            run_sh: `${global::ERIGON_PATH}/erigon --chain=${node_params().NETWORK} --datadir=${global::METADATA.babel_config.data_directory_mount_point}`,
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });

    start_job("start_heimdall", #{
        // heimdalld --home=$HEIMDALL_DIR "$@" or  exec heimdallcli --home=$HEIMDALL_DIR "$@"
        job_type: #{
            run_sh: `${global::HEIMDALL_PATH}/heimdalld --home=$HEIMDALL_DIR "$@"`, 
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });

    start_job("netdata", #{
        job_type: #{
            run_sh: `sleep 10 && /opt/netdata/usr/sbin/netdata -D`,
        },
       restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });
    
    //let response = run_sh(`rm -f ${global::KLAYTN_ARCHIVE_PATH}`);
};

fn address(){
    // curl -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"admin_nodeInfo","id":1}' http://localhost:8551 | jq .id
    let res = run_jrpc(#{
        host: `${global::KLAYTN_RPC_URL}`,
        method: "admin_nodeInfo",
        params: #{ id: 1, jsonrpc:"2.0"},
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    parse_json(res.body).result.id
}

fn application_status() {
    // curl localhost:26657/status
    let res = run_jrpc(#{
        host: `${global::KLAYTN_RPC_URL}`,
        method: "klay_blockNumber",
        params: #{ id: 1, jsonrpc:"2.0"},
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
       "delinquent";
    } else {
        "broadcasting";
    }
}

fn name(){
    let res = run_jrpc(#{
        host: `${global::KLAYTN_RPC_URL}`,
        method: "admin_nodeInfo",
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    parse_json(res.body).result.name
}

fn height(){
    let res = run_jrpc(#{
        host: `${global::KLAYTN_RPC_URL}`,
        method: "klay_blockNumber",
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    let hex = parse_json(res.body);
    parse_int(sub_string(hex.result,2),16)
}

fn block_age(){
    // curl -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"klay_getBlockByNumber","params":["latest", true],"id":1}' http://localhost:8551 
    // {
    //     "jsonrpc": "2.0",
    //     "id": 1,
    //     "result": {
    //       "blockScore": "0x1",
    //       "extraData": "xxxxxxx",
    //       "gasUsed": "0x0",
    //       "governanceData": "0x",
    //       "hash": "0xc8e222726fc0bd85c1a0823259fbe8d0c0cf347d061c40aae57c0a24fc2651cd",
    //       "logsBloom": "0x0",
    //       "number": "0x38d7de",
    //       "parentHash": "0x0c534b1b7ef97b54c47e85faa407dea5c8d03e15e2181892a42f87d1a0f983d9",
    //       "receiptsRoot": "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
    //       "reward": "0xc5b67d88c3a26fcee67fc542009d8526f6b4c8a3",
    //       "size": "0x716",
    //       "stateRoot": "0xb37d99b34d60daf641555e5ed3e30c2b764d9cb6a02419ac97a771d03ed28688",
    //       "timestamp": "0x5d4ae20b",
    //       "timestampFoS": "0x19",
    //       "totalBlockScore": "0x38d7df",
    //       "transactions": [],
    //       "transactionsRoot": "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
    //       "voteData": "0x"
    //     }
    //   }

    let res = run_jrpc(#{
        host: `${global::KLAYTN_RPC_URL}`,
        method: "klay_getBlockByNumber",
        // params: [true],
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    return 0;
    // let now=run_sh(`date +%s`);
    //let hex = parse_json(res.body);
    //hex.result.timestamp;
    // let result = parse_json(hex.result);
    // let timestamp = parse_int(sub_string(hex.result.timestamp,2),16);
    //parse_int(sub_string(hex.result,2),16)
    // print(hex);
    // parse_int(now.stdout)
    
    // parse_int(now.stdout)-timestamp;
}


fn consensus(){
    false
}

fn sync_status(){
    // curl -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"klay_syncing","params":[],"id":1}' http://localhost:8551 | jq '.result'
    //    let res = run_jrpc(#{
    //         host: `${global::KLAYTN_RPC_URL}`,
    //         method: "klay_blockNumber",
    //         params: #{ id: 1, jsonrpc:"2.0"},
    //         headers: #{"Content-Type" : "application/json"},
    //     });
    //     if res.status_code != 200 {
    //         throw res.status_code;
    //     }
    //     let hex = parse_json(res.body);
    //     let t0 = parse_int(sub_string(hex.result,2),16);
    //     sleep(5);
    //     let res1 = run_jrpc(#{
    //         host: `${global::KLAYTN_RPC_URL}`,
    //         method: "klay_blockNumber",
    //         params: #{ id: 1, jsonrpc:"2.0"},
    //         headers: #{"Content-Type" : "application/json"},
    //     });
    //     if res1.status_code != 200 {
    //         throw res1.status_code;
    //     }
    //     let hex1 = parse_json(res.body);
    //     let t1 = parse_int(sub_string(hex1.result,2),16);

    //     if t1-t0 > 0 {
    //         "synced"
    //     } else {
    //         "syncing"
    //     }

    // curl -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"klay_syncing","params":[],"id":1}' http://localhost:8551
    let res = run_jrpc(#{
        host: `${global::KLAYTN_RPC_URL}`,
        method: "klay_syncing",
        params: #{ id: 1, jsonrpc:"2.0"},
        headers: #{"Content-Type" : "application/json"},
    });
    if res.status_code != 200 {
        throw res.status_code;
    }
    if parse_json(res.body).result != false {
        "syncing"
    } else {
        "synced"
    }
}

fn staking_status(){
    "follower"
}

fn generate_keys(){
    ()
}
