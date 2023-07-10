// TODO
// fn height() {} 
// fn address() {} 

const METADATA = #{
    min_babel_version: "0.15.4",
    node_version: "3.16.3",
    protocol: "algorand",
    node_type: "validator",
    description: "Algorand node",
    requirements: #{
      vcpu_count: 32,
      mem_size_mb: 32768,
      disk_size_gb: 1500
    },
    nets: #{
      mainnet: #{
        url: "https://algorand-catchpoints.s3.us-east-2.amazonaws.com/channel/mainnet/latest.catchpoint",
        net_type: "main",
      },
      testnet: #{
        url: "https://algorand-catchpoints.s3.us-east-2.amazonaws.com/channel/testnet/latest.catchpoint",
        net_type: "test",
      },
    },
    babel_config: #{
      data_directory_mount_point: "/blockjoy/algorand",
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
          ports: [8080, 7833]
        },
      ],
    },
    keys: #{},
  };
  
  const API_HOST = "http://localhost:8080/";
  
  fn init(keys) {
    debug("Executing startup scripts...");
  
    let net = sanitize_sh_param(node_params().NETWORK);
    let metadata = GLOBAL::METADATA.nets[net];
    let data_directory = GLOBAL::METADATA.babel_config.data_directory_mount_point;
  
    start_job("Entrypoint for the Algorand validator node", #{
        job_type: #{ 
            run_sh:
            `/usr/bin/algod -d ${data_directory}/data -g ${data_directory}/genesis/${net}/genesis.json -o`
        },
        restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000
            },
        },
        needs: [],
    });
  }
  
  fn application_status() {
    let status = run_jrpc(global::API_HOST, "health");
    if parse_json(status.body).result != "OK" {
        "delinquent"
    } else {
        "broadcasting"
    }
  }
  
  fn get_genesis_info() {
    let res = run_jrpc(global::API_HOST, "genesis");
    if res.status_code != 200 {
        throw res.status_code;
    }
    parse_json(res.result)
  }
  
  fn sync_status() {
    let res = run_jrpc(global::API_HOST, "ready");
    if res.status_code != 200 {
        throw res.status_code;
    }
    parse_json(res.result)    
  }
