// meta

const METADATA = #{
  min_babel_version: "0.15.4",
  node_version: "3.16.3",
  protocol: "optimism",
  node_type: "node",
  description: "Optimism Node",
  requirements: #{
      vcpu_count: 4,
      mem_size_mb: 8192,
      disk_size_gb: 600,
  },
  
  nets: #{
      test: #{
          url: "https://goerli.optimism.io",
          net_type: "test",           // Allowed values: "dev", "test", "main"
          sequencer_url: "https://goerli-sequencer.optimism.io/",
          bedrock_archive_url: "https://datadirs.optimism.io/goerli-bedrock.tar.zst",
        },
      main: #{
          url: "https://mainnet.optimism.io",
          net_type: "main",
          sequencer_url: "https://mainnet-sequencer.optimism.io/",
          bedrock_archive_url: "https://datadirs.optimism.io/mainnet-bedrock.tar.zst",
      },
  },
  
  babel_config: #{
      data_directory_mount_point: "/blockjoy/optimism/data",
      log_buffer_capacity_ln: 1024,
      swap_size_mb: 4096,
  },
  
  firewall: #{
      enabled: true,
      default_in: "deny",
      default_out: "allow",
      
      rules: [
          #{
              // Unique rule name.
              name: "Allowed incoming tcp traffic on port",
              action: "allow",
              direction: "in",
              protocol: "tcp",
              ports: [9545,8545], 
          },
      ],
  },
  keys: #{},
};

// Other Vars
const ETH_RPC_URL = "http://localhost:8545";
const OPNODE_RPC_URL = "http://localhost:9545";
const BEDROCK_JWT_PATH = "/blockjoy/optimism/jwt.txt";
const INITIALIZED = "/blockjoy/optimism/initialized";
const BEDROCK_TAR_PATH = "/blockjoy/optimism/bedrock.tar.zst";
const BEDROCK_TAR_DATA_PATH = "/blockjoy/optimism/data";


fn init(keys) {

  let net = node_params().NETWORK;
  // to do: delete archive
  let bedrock_init_job_config = #{
    job_type: #{
      run_sh: `if [ ! -e "${INITIALIZED}" ]; then
      openssl rand -hex 32 >  ${BEDROCK_JWT_PATH};
      wget -O ${BEDROCK_TAR_PATH} ${METADATA.nets[node_params().NETWORK].bedrock_archive_url};
      tar xf ${BEDROCK_TAR_PATH} --directory ${BEDROCK_TAR_DATA_PATH};
      touch ${INITIALIZED};
      fi`
    },
    restart: #{
      on_failure: #{
          backoff_timeout_ms: 60000,
          backoff_base_ms: 10000,
          max_retries: 3,
      },
    }
  };

  let opgeth_job_config  = #{
    job_type: #{
      run_sh: `/usr/local/bin/geth --datadir="${let y = METADATA.babel_config.data_directory_mount_point; y}" \
      --http \
      --http.corsdomain="*" \
      --http.vhosts="*" \
      --http.addr=0.0.0.0 \
      --http.port=8545 \
      --http.api=web3,debug,eth,txpool,net,engine \
      --ws \
      --ws.addr=0.0.0.0 \
      --ws.port=8546 \
      --ws.origins="*" \
      --ws.api=debug,eth,txpool,net,engine,web3 \
      --syncmode=full \
      --gcmode="full" \
      --nodiscover \
      --maxpeers=0 \
      --networkid=420 \
      --authrpc.vhosts="*" \
      --authrpc.addr=0.0.0.0 \
      --authrpc.port=8551 \
      --authrpc.jwtsecret=${BEDROCK_JWT_PATH} \
      --rollup.sequencerhttp="${METADATA.nets[node_params().NETWORK].sequencer_url}" \
      --rollup.disabletxpoolgossip=true \
      --verbosity=3 \
      --snapshot=false`
    },
    restart: #{
      on_failure: #{
          backoff_timeout_ms: 60000,
          backoff_base_ms: 10000,
          max_retries: 3,
      },
    }
  };

  let opnode_job_config = #{
    job_type: #{
      run_sh: `./usr/local/bin/op-node --l2=http://localhost:8551  \
      --l2.jwt-secret=${BEDROCK_JWT_PATH}  \
      --network=mainnet        \
      --rpc.addr=0.0.0.0 \
      --l1=${let l = sanitize_sh_param(node_params().ETHL1); l} \
      --rpc.port=9545 \
      --l1.rpckind=erigon  \
      --l1.trustrpc`
    },
    restart: #{
      on_failure: #{
          backoff_timeout_ms: 60000,
          backoff_base_ms: 10000,
          max_retries: 3,
      },
    }
  };

  start_job("Bedrock Init", bedrock_init_job_config);
  start_job("OP-Geth Init", opgeth_job_config);
  start_job("OP-Node Init", opnode_job_config);
};

fn application_status() {
  let res = run_rest(#{ url: "${OPNODE_RPC_URL}/health"});
  if res.status_code != 200 {
      "delinquent"
  } else {
      "broadcasting"
  }
}

fn name(){
  let res = run_sh("cat /blockjoy/optimism/data/geth/nodekey");
  res.stdout;
}

fn height(){
  let getBlockByNumber = #{host: "localhost:8154", 
             method: "getBlockByNumber", 
             headers: #{"content-type": "application/json"}
            };
  response = run_jrpc(getBlockByNumber);
  response_map = parse_json(resp.body);
  parse_int(sub_string(h.result,2),16)
}

fn block_age(){
  let res = run_sh("echo $(expr $(date +%s) - $(cast block latest --rpc-url ${ETH_RPC_URL} -j | jq .timestamp -r |  cast to-dec))  ");
  res.stdout;
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

fn block_diff(){
  let res = run_sh("echo $(expr $(cast bn --rpc-url https://mainnet.optimism.io) - $(cast bn --rpc-url ${ETH_RPC_URL}) )");
  res.stdout;
}
 