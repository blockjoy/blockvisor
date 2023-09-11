const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "1.11.0",
    protocol: "klaytn",
    node_type: "endpoint node",
    description: "Klaytn Endpoint Node",
    kernel: "5.10.174-build.1+fc.ufw",
    requirements: #{
        vcpu_count: 8,
        mem_size_mb: 65536,
        disk_size_gb: 5000, // 1.5TB / 3.3TB uncompressed
    },

    nets: #{
        baobab: #{
            url: "https://public-en-baobab.klaytn.net/",
            package_url: "https://packages.klaytn.net/klaytn/v1.11.0/ken-baobab-v1.11.0-0-linux-amd64.tar.gz",
            net_type: "test",
            archive_url: "https://s3.ap-northeast-2.amazonaws.com/klaytn-chaindata/baobab/klaytn-baobab-chaindata-20230829010811.tar.gz",
          },
        cypress: #{
            url: "https://public-en-cypress.klaytn.net/",
            package_url: "https://packages.klaytn.net/klaytn/v1.11.0/ken-v1.11.0-0-linux-amd64.tar.gz",
            net_type: "main",
            archive_url: "https://s3.ap-northeast-2.amazonaws.com/klaytn-chaindata/cypress/klaytn-cypress-chaindata-20230828010811.tar.gz",
        }
    },

    babel_config: #{
        data_directory_mount_point: "/blockjoy/klaytn/data",
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 4096,
    },

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
                ports: [8551,8552],
            },
        ],
     },
    keys: #{}
};

const KLAYTN_RPC_URL = "http://localhost:8551";
const KLAYTN_DOWNLOADS_DIR = global::METADATA.babel_config.data_directory_mount_point + "/downloads/";
const KEN_DOWNLOAD_PATH = global::KLAYTN_DOWNLOADS_DIR + "ken.tar.gz";
const KLAYTN_ARCHIVE_PATH = global::KLAYTN_DOWNLOADS_DIR +"klatyn-chaindata.tar.gz";
const KEN_INSTALL_PATH = "/var/lib/klaytn";
const KEN_PATH = `${global::KEN_INSTALL_PATH}/ken-linux-amd64/bin`;

fn init(keys) {

    let klaytn_init_commands = [
        `mkdir -p ${global::KLAYTN_DOWNLOADS_DIR}`,
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

    start_job("start", #{
        job_type: #{
            run_sh: `${global::KEN_PATH}/kend start-docker`,
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
        // needs: ["download"],
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
