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
        disk_size_gb: 4000,
    },

    nets: #{
        baobab: #{
            package_url: "https://packages.klaytn.net/klaytn/v1.11.0/ken-baobab-v1.11.0-0-linux-amd64.tar.gz",
            net_type: "test", 
            archive_url: "https://s3.ap-northeast-2.amazonaws.com/klaytn-chaindata/baobab/klaytn-baobab-chaindata-20230829010811.tar.gz",
          },
        cypress: #{
            package_url: "https://packages.klaytn.net/klaytn/v1.11.0/ken-v1.11.0-0-linux-amd64.tar.gz",
            net_type: "main",
            archive_url: "https://s3.ap-northeast-2.amazonaws.com/klaytn-chaindata/cypress/klaytn-cypress-chaindata-20230828010811.tar.gz",
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
    keys: #{},
  }
};

// Other Vars
const KLAYTN_RPC_URL = "http://localhost:8551";
const KEN_DOWNLOAD_PATH = global::METADATA.babel_config.data_directory_mount_point + "/downloads/ken.tar.gz";
const KLAYTN_ARCHIVE_PATH = global::METADATA.babel_config.data_directory_mount_point +"/downloads/klatyn-chaindata.tar.gz";
const KEN_INSTALL_PATH = "/var/lib/klaytn";
fn init(keys) {

    let klaytn_init_commands = [
        `mkdir -p ${global::KEN_DOWNLOAD_PATH}`,
        `wget ${global::METADATA.nets[node_params().NETWORK].package_url} -O ${global::KEN_DOWNLOAD_PATH}`,
        `mkdir -p ${global::KEN_INSTALL_PATH}`,
        `tar xvf ${global::KEN_DOWNLOAD_PATH} -C ${global::KEN_INSTALL_PATH}`,
        `sed -e "s|DATA_DIR=.*|DATA_DIR=${global::METADATA.data_directory_mount_point}|" ${global::KEN_INSTALL_PATH}/ken-linux-amd64/conf/kend.conf`,
        `sed -e 's|RPC_API=.*|RPC_API=admin,debug,klay,eth,miner,net,personal,rpc,txpool,web3|' ${global::KEN_INSTALL_PATH}/ken-linux-amd64/conf/kend.conf`,
    ];

    for (cmd, i) in klaytn_init_commands {
        let response = run_sh(cmd);
        print(`Response from shell command ${cmd}':`);
        print(response);
    
        if response.exit_code != 0 {
            print(`Command at index ${i} failed: '${cmd}'`);
        }
    }
    
    start_job("download", #{
        job_type: #{
            run_sh: `wget ${global::METADATA.nets[node_params().NETWORK].archive_url} -O ${global::KLAYTN_ARCHIVE_PATH} && tar xvf ${global::KLAYTN_ARCHIVE_PATH} -C ${global::METADATA.data_directory_mount_point}`,
        },
        restart: #{
            on_failure: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
                max_retries: 3,
            },
        },
        needs: [],
    });

    start_job("start", #{
        job_type: #{
            run_sh: "kend start && sleep infinity",
        },
        restart: #{
            on_failure: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
                max_retries: 3,
            },
        },
        needs: ["download"],
    });
    
        
    let klay_path = `${global::KEN_INSTALL_PATH}/ken-linux-amd64/bin`;
    let current_path = run_sh("echo $PATH");
    if current_path.stdout.contains(klay_path) {
        print("PATH already contains '${klay_path}'");
    } else {
        let response = run_sh(`export PATH=$PATH:${klay_path}`);
        // Check the response and handle errors if needed
        if response.exit_code != 0 {
            print("Error updating PATH");
        } else {
            print("PATH updated successfully");
        }
    }

    let response = run_sh(`kend start`);
    if response.exit_code != 0 {
        print("Error starting kend");
    } else {
        print("kend started successfully");
    }

    // to do : Clean up tar snapshot files.
    run_sh(`rm -f ${global::KLAYTN_ARCHIVE_PATH}`)
};

fn address(){
    let res = run_sh("ken --exec governance.nodeAddress attach ${global::METADATA.data_directory_mount_point}/klay.ipc");
    res.stdout;
    let response = run_jrpc(#{
        host: KLAYTN_RPC_URL,
        method: governance_nodeAddress ,
        params: #{params: ["latest", "true"], id: 1, jsonrpc:"2.0"},
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    let payload = parse_json(res.result);
}

fn application_status() {
    let res = run_sh("kend status");
    if response.contains("down") {
        "delinquent"
    } else {
        "broadcasting"
    }
}

fn name(){
    let res = run_sh(`ken --exec admin.nodeInfo.name attach ${global::METADATA.data_directory_mount_point}/klay.ipc`);
    res.stdout;
}

fn height(){
    let res = run_sh(`ken --exec klay.blockNumber attach ${global::METADATA.data_directory_mount_point}/klay.ipc`);
    res.stdout;
}

fn block_age(){
    // curl -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"klay_getBlockByNumber","params":["latest", true],"id":1}' http://localhost:8551 | jq '.result.timestamp'
    let res = run_jrpc(#{
        host: KLAYTN_RPC_URL,
        method: klay_getBlockByNumber,
        params: #{params: ["latest", "true"], id: 1, jsonrpc:"2.0"},
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    let payload = parse_json(res.result);
    let timestamp = parse_int(sub_string(payload.result.timestamp,2),16);
    let now=run_sh(`date +%s`);
    parse_int(now.stdout)-timestamp;
}


fn consensus(){
    "false"
}

fn sync_status(){
   // curl -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"klay_syncing","params":[],"id":1}' http://localhost:8551 | jq '.result'
   let res = run_jrpc(#{
        host: KLAYTN_RPC_URL,
        method: klay_syncing,
        params: #{id: 1, jsonrpc:"2.0"},
        headers: #{
            "Content-Type" : "application/json",
        }
    });
    if res.status_code != 200 {
        throw res.status_code;
    }
    let payload = parse_json(res.result);
    if payload.result {
        "Syncing"
    } else {
        "Not Syncing"
    }

}

fn staking_status(){
    "follower"
}

fn generate_keys(){
    "keys"
}
