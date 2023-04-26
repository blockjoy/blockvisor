# Rhai Plugin Scripting Guide

This is user guide on how to add custom blockchain support to BlockvisordD (aka BV),
by implementing Babel Plugin using Rhai scripting language. 

# Introduction

Since BV itself is blockchain agnostic, specific blockchain support is plugged-into BV by providing:
- blockchain `os.img` along with `kernel` file, that is used to run VM
- babel plugin that translates BV blockchain agnostic interface (aka Babel API) into blockchain specific calls.

Currently, Babel Plugin can be implemented using Rhai scripting language.
See [The Rhai Book](https://rhai.rs/book) for more details on Rhai language itself.

# Plugin Interface

To add some specific blockchain support Babel Plugin must tell BV how to use blockchain specific tools inside the image,
to properly setup and maintain blockchain node. This chapter describe interface that needs to be implemented by script
for this purpose.

## Blockchain METADATA

First thing that Babel Plugin SHALL provide is Blockchain Metadata structure that describe static properties
of the blockchain. In Rhai script it is done by declaring `METADATA` constant.
See below example with comments for more details.

**Example:**
```rust
const METADATA = #{
    // A semver version of the babel program, indicating the minimum version of the babel
    // program that a babel script is compatible with.
    min_babel_version: "0.0.9",
    
    // A semver version of the blockchain node program.
    node_version: "1.15.9",
    
    // Name of the blockchain protocol.
    protocol: "helium",
    
    // Type of the node (validator, beacon, etc).
    node_type: "validator",
    
    // [optional] Some description of the node.
    description: "helium blockchain validator",
    
    // Blockchain resource requirements.
    requirements: #{
        // Virtual cores to share with VM
        vcpu_count: 1,

        // RAM allocated to VM in MB
        mem_size_mb: 2048,

        // Size of data drive for storing blockchain data (not to be confused with OS drive)
        disk_size_gb: 1,
    },
    
    // Supported blockchain networks.
    nets: #{
        // Key is the name of blockchain network 
        test: #{
            // Url for given blockchain network.
            url: "https://testnet-api.helium.wtf/v1/",

            // Blockchain network type.
            // Allowed values: "dev", "test", "main"
            net_type: "test",

            // [optional] Custom network metadata can also be added.
            beacon_nodes_csv: "http://beacon01.goerli.eth.blockjoy.com,http://beacon02.goerli.eth.blockjoy.com?789",
        },
        main: #{
            url: "https://rpc.ankr.com/eth",
            net_type: "main",
        },
    },
    
    // Configuration of Babel - agent running inside VM.
    babel_config: #{
        // Path to mount data drive to.
        data_directory_mount_point: "/blockjoy/miner/data",
        
        // Capacity of log buffer (in lines).
        log_buffer_capacity_ln: 1024,
        
        // Size of swap file created on the node, in MB.
        swap_size_mb: 512,
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
                ips: "192.167.0.1/24",    
                
                // List of ports. Empty means all.
                ports: [24567], // ufw allow in proto tcp port 24567
            },
            #{
                name: "Allowed incoming udp traffic on ip and port",
                action: "allow",
                direction: "in",
                protocol: "udp",
                ips: "192.168.0.1",
                ports: [24567], //ufw allow in proto udp from 192.168.0.1 port 24567
            },
        ],
    },
    
    // [optional] Configuration of blockchain keys.
    keys: #{
        first: "/opt/secrets/first.key",
        second: "/opt/secrets/second.key",
        "*": "/tmp",
    },
};
```

## Functions that SHALL be implemented by Plugin

Babel Plugin SHALL implement at least `init` function that is called by BV on first node start.
Once node is **successfully** started it is not called by BV anymore. Hence `init` function may be called more than
once only if node start failed for some reason.

`init` function takes `secret_keys` key-value map as argument. It shall do all required initializations
and start required background jobs. See 'Engine Interface' chapter for more details on how to start jobs.

**Example:**
```rust
fn init(keys) {
    run_sh("echo 'some initialization step'");
    let param = sanitize_sh_param(node_params().TESTING_PARAM);
    start_job("echo", #{
        body: "echo \"Blockchain entry_point parametrized with " + param + "\"",
        restart: #{
            "always": #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
    });
}
```

## Functions that SHOULD be implemented by Plugin

Functions listed below are used BV for collecting metrics and status monitoring.
It is recommended to implement all of them, if feasible.

- `height()` - Returns the height of the blockchain (in blocks).
- `block_age()` - Returns the block age of the blockchain (in seconds).
- `name()` - Returns the name of the node. This is usually some random generated name that you may use
to recognise the node, but the purpose may vary per blockchain.
  <br>**Example**: _chilly-peach-kangaroo_
- `address()` - The address of the node. The meaning of this varies from blockchain to blockchain.
  <br>**Example**: _/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3_
- `consensus()` - Returns `bool` whether this node is in consensus or not.
- `application_status()` - Returns blockchain application status.
- `sync_status()` - Returns blockchain synchronization status.
   <br>**Allowed return values**: _provisioning_, _broadcasting_, _cancelled_, _delegating_, _delinquent_, _disabled_, _earning_, _electing_, _elected_, _exported_, _ingesting_, _mining_, _minting_, _processing_, _relaying_, _removed_, _removing_
- `staking_status()` - Returns blockchain staking status.
  <br>**Allowed return values**: _syncing_, _synced_
- `generate_keys()` - Generates keys on the node.
  <br>**Allowed return values**: _follower_, _staked_, _staking_, _validating_, _consensus_, _unstaked_

## Functions that MAY be implemented by Plugin

Plugin may additionally implement an arbitrary custom function that will be then accessible
via BV CLI interface. The only limitation is that custom functions must take only one "string"
argument (it may be more complex structure, but e.g. serialized as JSON) and must return "string" as well.

**Example:**
```rust
fn some_custom_function(arg) {
    let input = parse_json(arg);
    let output = #{
        result: input.value * 2
    };
    output.to_json();
}
```

# Engine Interface

To make implementation of Babel Plugin interface possible, BV provides following functions to Rhai script.

- `start_job(job_name, job_config)` - Start background job with unique name. See 'Backgound Jobs' section for more details.
- `stop_job(job_name)` - Stop background job with given unique name if running.
- `job_status(job_name)` - Get background job status by unique name.
  <br>**Possible return values**: _pending_, _running_, _stopped_, _finished{exit_code, message}_
- `run_jrpc(host, method)` - Execute Jrpc request to the current blockchain and return its response as json string.
- `run_rest(url)` - Execute a Rest request to the current blockchain and return its response as json string.
- `run_sh(body)` - Run Sh script on the blockchain VM and return its stdout as string.
- `sanitize_sh_param(param)` - Allowing people to substitute arbitrary data into sh-commands is unsafe.
  Call this function over each value before passing it to `run_sh`. This function is deliberately more
  restrictive than needed; it just filters out each character that is not a number or a
  string or absolutely needed to form an url or json file.
- `render_template(template, output, params)` -This function renders configuration template with provided `params`.
  It assumes that file pointed by `template` argument exists.
  File pointed by `output` path will be overwritten if exists.
- `node_params()` - Get node params as key-value map.
- `save_data(value)` - Save plugin data to persistent storage.
- `load_data()` - Load plugin data from persistent storage.

## Background Jobs

Background job is a way to asynchronously run long-running sh command. In particular, it can be used
to define blockchain entrypoint(s) i.e. background process(es) that are automatically started
with the node.

Each background job has its unique name and configuration structure described by following example.

**Example:**
```rust
    let job_config_A = #{
        // Sh script body
        body: "echo \"some initial job done\"",
        
        // Job restart policy.
        // "never" indicates that this job will never be restarted, whether succeeded or not - appropriate for jobs
        // that can't be simply restarted on failure (e.g. need some manual actions).
        restart: "never",
    };
    start_job("job_name_A", job_config_A);

    let job_config_B = #{
            // Sh script body
            body: "wget https://some_url",
            
            // Job restart policy.
            restart: #{
            
            // "on_failure" key means that job is restarted only if `exit_code != 0`.
            "on_failure": #{
                // if job stay alive given amount of time (in miliseconds) backoff is reset
                backoff_timeout_ms: 60000,
                
                // base time (in miliseconds) for backof,
                // multiplied by consecutive power of 2 each time                
                backoff_base_ms: 10000,
                
                // [optional] maximum number of retries, or there is no such limit if not set
                max_retries: 3,
            },
        },
    };
    start_job("job_name_B", job_config_B);

    let entrypoint_config = #{
        // Sh script body
        body: "echo \"Blockchain entry_point parametrized with " + param + "\"",

        // Job restart policy.
        restart: #{

            // "always" key means that job is always restarted - equivalent to entrypoint.
            "always": #{
                // if job stay alive given amount of time (in miliseconds) backoff is reset
                backoff_timeout_ms: 60000,
                
                // base time (in miliseconds) for backof,
                // multiplied by consecutive power of 2 each time                
                backoff_base_ms: 10000,

                // [optional] maximum number of retries, or there is no such limit if not set
                max_retries: 3,
            },
        },

        // [optional] List of job names that this job needs to be finished before start, may be empty.
        needs: ["job_name_A", "job_name_B"],
    };
    start_job("unique_entrypoint_name", entrypoint_config);
```

Once job has been started, other functions in the script may fetch for its state with `job_status(job_name)`,
or stopped on demand with `stop_job(job_name)`.

# Common Use Cases

## Referencing METADATA in Functions

METADATA is regular Rhai constant, so it can be easily reference in functions.

**Example:**
```rust
fn some_function() {
    let net_url = global::METADATA.nets[node_params().NETWORK].url;
}
```

## Handling JRPC Output

`run_jrpc` function always return JSON serialized to string. Use `parse_json` to easily access json fields.

**Example:**
```rust
const API_HOST = "http://localhost:4467/";

fn block_age() {
    parse_json(run_jrpc(global::API_HOST, "info_block_age")).result.block_age
}
```

## Output Mapping

Rhai language has convenient `switch` statement, which is very similar to Rust `match`.
Hence, it is a good candidate for output mapping. 

**Example:**
```rust
fn sync_status() {
    let out = run_sh("get_sync_status");
    let status = switch out {
        "0" => "synced",
        _ => "syncing",
    };
    status
}
```

## Running Sh Script

**IMPORTANT:** Whenever user input (`node_params()` in particular) is passed to sh script,
it should be sanitized with `sanitize_sh_param()`, to avoid malicious code injection.

**Example:**
```rust
fn custom_function(arg) {
    let user_param = node_params().USER_INPUT;
    sanitize_sh_param(user_param);
    run_sh("echo '" + user_param + "'")
}
```

# Testing

For now, the easiest way to test Rhai script is to write Rust unit test.

See [test_testing.rs](tests/test_testing.rs) for example.
