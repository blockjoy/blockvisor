// meta

const METADATA = #{
  // A semver version of the babel program, indicating the minimum version of the babel
  // program that a babel script is compatible with.
  min_babel_version: "0.15.4",
  
  // A semver version of the blockchain node program.
  node_version: "3.16.3",
  
  // Name of the blockchain protocol.
  protocol: "optimism",
  
  // Type of the node (validator, beacon, etc).
  node_type: "node",
  
  // [optional] Some description of the node.
  description: "Optimism Node",
  
  // Blockchain resource requirements.
  requirements: #{
      // Virtual cores to share with Micro VM
      vcpu_count: 4,

      // RAM allocated to Micro VM in MB
      mem_size_mb: 8192,

      // Size of data drive for storing blockchain data (not to be confused with OS drive)
      disk_size_gb: 600,
  },
  
  // Supported blockchain networks.
  nets: #{
      // Key is the name of blockchain network 
      test: #{
          // Url for given blockchain network.
          url: "https://goerli.optimism.io",

          // Blockchain network type.
          // Allowed values: "dev", "test", "main"
          net_type: "test",

          // [optional] Custom network metadata can also be added.
          sequencer_url: "https://goerli-sequencer.optimism.io/",
        },
      main: #{
          url: "https://mainnet.optimism.io",
          net_type: "main",
          sequencer_url: "https://mainnet-sequencer.optimism.io/",
      },
  },
  
  // Configuration of Babel - agent running inside Micro VM.
  babel_config: #{
      // Path to mount data drive to.
      data_directory_mount_point: "/blockjoy/optimism/data",
      
      // Capacity of log buffer (in lines).
      log_buffer_capacity_ln: 1024,
      
      // Size of swap file created on the node, in MB.
      swap_size_mb: 4096,
  },
  
  // Node firewall configuration.
  firewall: #{
      // Option to disable firewall at all. Only for debugging purpose - use on your own risk!
      enabled: true,
      
      // Fallback action for inbound traffic used when packet doesn't match any rule.
      // Allowed values: "allow", "deny", "reject"
      default_in: "deny",
      
      // Fallback action for outbound traffic used when packet doesn't match any rule.
      // Allowed values: "allow", "deny", "reject"
      default_out: "allow",
      
      // Set of rules to be applied.
      rules: [
          #{
              // Unique rule name.
              name: "Allowed incoming tcp traffic on port",
              
              // Action applied on packet that match rule.
              // Allowed values: "allow", "deny", "reject"
              action: "allow",
              
              // Traffic direction for which rule applies.
              // Allowed values: "out", "in"
              direction: "in",
              
              // [optional] Protocol - ""both" by default.
              // Allowed values: "tcp", "udp", "both"
              protocol: "tcp",
              
              // Ip(s) compliant with CIDR notation.
              // ips: "192.167.0.1/24",    
              
              // List of ports. Empty means all.
              ports: [9545,8545], // ufw allow in proto tcp port 24567
          },
          // #{
          //     name: "Allowed incoming udp traffic on ip and port",
          //     action: "allow",
          //     direction: "in",
          //     protocol: "udp",
          //     ips: "192.168.0.1",
          //     ports: [24567], //ufw allow in proto udp from 192.168.0.1 port 24567
          // },
      ],
  },
  keys: #{},
  // [optional] Configuration of blockchain keys.
  // keys: #{
  //     first: "/opt/secrets/first.key",
  //     second: "/opt/secrets/second.key",
  //     "*": "/tmp",
  // },
};

// Other Vars
const ETH_RPC_URL = "http://localhost:8545"
const OPNODE_RPC_URL = "http://localhost:9545"
const BEDROCK_JWT_PATH = "/blockjoy/optimism/jwt.txt"
const INITIALIZED = "/blockjoy/optimism/initialized"
const BEDROCK_TAR_PATH = "/blockjoy/optimism/mainnet-bedrock.tar.zst"
const BEDROCK_TAR_DATA_PATH = "/blockjoy/optimism/data"
const BEDROCK_ARHIVE_URL = "https://datadirs.optimism.io/mainnet-bedrock.tar.zst"
//  Init Node

fn init(keys) {

  let bedrock_init_job_config = #{
    job_type: #{
      run_sh: `if [ ! -e "${INITIALIZED}" ]; then
      openssl rand -hex 32 >  ${BEDROCK_JWT_PATH};
      wget -O ${BEDROCK_TAR_PATH} ${BEDROCK_ARHIVE_URL};
      tar xf ${BEDROCK_TAR_PATH} --directory ${BEDROCK_TAR_DATA_PATH}
      touch ${INITIALIZED};
      fi`
    },
    restart: "never",
  }

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
      --rollup.sequencerhttp="${METADATA.nets.${node_params().NETWORK}.sequencer_url}" \
      --rollup.disabletxpoolgossip=true \
      --verbosity=3 \
      --snapshot=false`
    },
    restart: "never",
  }

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
    restart: "never",
  }

  start_job("Bedrock Init", bedrock_init_job_config);
  start_job("OP-Geth Init", opgeth_job_config);
  start_job("OP-Node Init", opnode_job_config);
}

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
  print res.stdout;
}

fn height(){
  let res = run_sh("cast bn --rpc-url ${ETH_RPC_URL}");
  print res.stdout;
}

fn block_age(){
  let res = run_sh("echo $(expr $(date +%s) - $(cast block latest --rpc-url ${ETH_RPC_URL} -j | jq .timestamp -r |  cast to-dec))  ");
  print res.stdout;
}

fn block_diff(){
  let res = run_sh("echo $(expr $(cast bn --rpc-url https://mainnet.optimism.io) - $(cast bn --rpc-url ${ETH_RPC_URL}) )");
  print res.stdout;
}
 