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

Functions listed below are required by BV to work properly.

- `init` - Function that is called by BV on first node start. It takes `secret_keys` key-value map as argument.
  It shall do all required initializations and start required background jobs. See 'Engine Interface' chapter for more details on how to start jobs.
  Once node is **successfully** started it is not called by BV anymore. Hence `init` function may be called more than
  once only if node start failed for some reason.

**Example:**
```rust
fn init(keys) {
    let res = run_sh("echo 'some initialization step'");
    if res.exit_code != 0  {
      throw res;
    }
    let param = sanitize_sh_param(node_params().TESTING_PARAM);
    start_job("echo", #{
        job_type: #{
          run_sh: "echo \"Blockchain entry_point parametrized with " + param + "\"",
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
    });
}
```

- `address()` - The address of the node. The meaning of this varies from blockchain to blockchain.
  <br>**Example**: _/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3_
- `application_status()` - Returns blockchain application status.
  <br>**Allowed return values**: _provisioning_, _broadcasting_, _cancelled_, _delegating_, _delinquent_, _disabled_, _earning_, _electing_, _elected_, _exported_, _ingesting_, _mining_, _minting_, _processing_, _relaying_, _removed_, _removing_

## Functions that SHOULD be implemented by Plugin

Functions listed below are used BV for collecting metrics and status monitoring.
It is recommended to implement all of them, if feasible.

- `height()` - Returns the height of the blockchain (in blocks).
- `block_age()` - Returns the block age of the blockchain (in seconds).
- `name()` - Returns the name of the node. This is usually some random generated name that you may use
to recognise the node, but the purpose may vary per blockchain.
  <br>**Example**: _chilly-peach-kangaroo_
- `consensus()` - Returns `bool` whether this node is in consensus or not.
- `sync_status()` - Returns blockchain synchronization status.
  <br>**Allowed return values**: _syncing_, _synced_
- `staking_status()` - Returns blockchain staking status.
  <br>**Allowed return values**: _follower_, _staked_, _staking_, _validating_, _consensus_, _unstaked_
- `generate_keys()` - Generates keys on the node.

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
- `run_jrpc(request)` - Execute Jrpc request to the current blockchain. Request must have following structure:
```
{
  // This is the host for the JSON rpc request.
  host: String,
  // The name of the jRPC method that we are going to call into.
  method: String,
  // [optional] Extra HTTP headers to be added to request.
  headers: Map
}
```
Return http response (with default 15s timeout) as following structure:
```
{ 
  status_code: i32,
  body: String,
}
```
- `run_jrpc(request, timeout)` - Same as above, but with custom request timeout (in seconds).
- `run_rest(request)` - Execute a Rest request to the current blockchain. Request must have following structure:
```
{
  // This is the url of the rest endpoint.
  url: String,
  // [optional] Extra HTTP headers to be added to request.
  headers: Map
}
```
Return its http response (with default 15s timeout) as following structure:
```
{ 
  status_code: i32,
  body: String,
}
```
- `run_rest(request, timeout)` - Same as above, but with custom request timeout (in seconds).
- `run_sh(body)` - Run Sh script on the blockchain VM and return its result (with default 15s timeout). Result is following structure:
```
{ 
  exit_code: i32,
  stdout: String,
  stderr: String,
}
```
- `run_sh(body, timeout)` - Same as above, but with custom execution timeout (in seconds).
- `sanitize_sh_param(param)` - Allowing people to substitute arbitrary data into sh-commands is unsafe.
  Call this function over each value before passing it to `run_sh`. This function is deliberately more
  restrictive than needed; it just filters out each character that is not a number or a
  string or absolutely needed to form an url or json file.
- `render_template(template, output, params)` - This function renders configuration template with provided `params`.
  See [Tera Docs](https://tera.netlify.app/docs/#templates) for details on templating syntax.
  `params` is expected to be JSON serialized to string.
  It assumes that file pointed by `template` argument exists.
  File pointed by `output` path will be overwritten if exists.
- `node_params()` - Get node params as key-value map.
- `save_data(value)` - Save plugin data to persistent storage. It takes string as argument. `to_json` can be used for more complex structures.
- `load_data()` - Load plugin data from persistent storage. Returns string (as it was passed to `save_data`).

## Background Jobs

Background job is a way to asynchronously run long-running tasks. Currently, two types of tasks are supported:
1. `run_sh` - arbitrary long-running shell script.
2. `download` - download data (e.g. previously archived blockchain data, to speedup init process).
3. `upload` - upload data (e.g. archive blockchain data).
In particular, it can be used to define blockchain entrypoint(s) i.e. background process(es) that are automatically started
with the node.

Each background job has its unique name and configuration structure described by following example.

**Example:**
```rust
    let job_config_A = #{
        job_type: #{
            // Sh script body
            run_sh: "echo \"some initial job done\"",
        },

        // Job restart policy.
        // "never" indicates that this job will never be restarted, whether succeeded or not - appropriate for jobs
        // that can't be simply restarted on failure (e.g. need some manual actions).
        restart: "never",
    };
    start_job("job_name_A", job_config_A);

    let job_config_B = #{
            job_type: #{
                download: #{
                    destination: "destination/path/for/blockchain_data",    
                },
            },
            
        // Job restart policy.
        restart: #{
            // "on_failure" key means that job is restarted only if `exit_code != 0`.
            on_failure: #{
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
        job_type: #{
            // Sh script body
            run_sh: "echo \"Blockchain entry_point parametrized with " + param + "\"",
        },

        // Job restart policy.
        restart: #{
            // "always" key means that job is always restarted - equivalent to entrypoint.
            always: #{
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

    let upload_job_config = #{
        job_type: #{
            upload: #{
                source: "source/path/for/blockchain_data",
            },
        },
        restart: "never",
    };
    start_job("upload_job_name", upload_job_config);
```

Once job has been started, other functions in the script may fetch for its state with `job_status(job_name)`,
or stopped on demand with `stop_job(job_name)`.

## Logging

Rhai provides `print` and `debug` functions to simply print into stdout. To make plugin logs consistent with BV logs,
it is recommended to use following log functions:

- `debug` (overridden built-in `debug` implementation)
- `info`
- `warn`
- `error`

**Example:**
```rust
fn some_function() {
    info("some important step logged with INFO level");
}
```
Output:
```
<time>  INFO babel_api::rhai_plugin: node_id: <node_id>|some important step logged with INFO level 
```


# Common Use Cases

## Referencing METADATA in Functions

METADATA is regular Rhai constant, so it can be easily reference in functions.

**Example:**
```rust
fn some_function() {
    let net_url = global::METADATA.nets[node_params().NETWORK].url;
}
```

## Add Custom HTTP Headers to JRPC and REST Requests

`request` parameter in `run_jrpc` and `run_rest` functions has optional `headers` field,
that can be used to add custom HTTP headers to the request.

**Example:**
```rust
let data = #{host: "localhost:8154", 
             method: "getBlockByNumber", 
             headers: #{"content-type": "application/json"}
            };
run_jrpc(data);
```

## Handling JRPC Output

`run_jrpc` function return raw http response. Use `parse_json` on response body, to easily access json fields.

**Example:**
```rust
const API_HOST = "http://localhost:4467/";

fn block_age() {
    let resp = run_jrpc(#{host: global::API_HOST, method: "info_block_age"});
    if resp.status_code != 200 {
      throw resp;
    }
    parse_json(resp.body).result.block_age
}
```

## Output Mapping

Rhai language has convenient `switch` statement, which is very similar to Rust `match`.
Hence, it is a good candidate for output mapping. 

**Example:**
```rust
fn sync_status() {
    let res = run_sh("get_sync_status");
    if res.exit_code != 0 {
        debug(res.stderr);
        throw res;
    }
    let status = switch res.stdout {
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
  let user_param = sanitize_sh_param(node_params().USER_INPUT);
  let res = run_sh("echo '" + user_param + "'");
  if res.exit_code == 0 {
    res.stdout
  } else {
    res.stderr
  }
}
```

## Save and Load Plugin Data

Plugin specific data can be persisted with `save/load_data` functions. It takes/returns simple string, but e.g. json can
be easily used for more complex structures.

**Example:**
```rust
fn custom_function(arg) {
    let info;
    try {
        info = parse_json(load_data());
    } catch {
        info = #{
            some_field: "initial value",
            some_other_field: false,
        };
        save_data(info.to_json());
    };
    info.some_field
}
```

## Rendering Configuration File

Some blockchains may require configuration files to be filled up with some user data. For that `render_template` can be used.

At first, configuration template file must be included into the image.

**Example:**
```
first_parameter: {{ param1 }}
second_parameter: {{param2}}
optional_parameter: {% if param3 %}{{ param3 }}{% else %}None{% endif %}
array: [{% for p in aParam %}{{p}},{% endfor %}]
```

Then it can be rendered e.g. during node initialization:

**Example:**
```rust
fn init(keys) {
    let params = #{
        param1: "value1", 
        param2: 2,
        aParam: ["a", "bb", "ccc"],
    };
    render_template("/etc/blockchain.config.template", "/etc/blockchain.config", params.to_json());
}
```

After `init` is called, following file should be created:

```
first_parameter: "value1"
second_parameter: 2
optional_parameter: None
array: ["a","bb","ccc",]
```

# Testing

## Unit Testing

For now, the easiest way to test Rhai script is to write Rust unit test.

See [test_testing.rs](tests/test_testing.rs) for example.

## Manual Integration Testing

If you want to test how rhai script integrates with BV and running node, it can be done in following way:

1. Stop BV if already running: `systemctl stop blockvisor`
2. Run BV in standalone mode, by defining `BV_STANDALONE` env variable, e.g.: `BV_STANDALONE=1 blockvisord`
3. Create node, e.g.: `bv node create --ip 192.168.0.75 --gateway 192.168.0.1 testing/validator/0.0.1 --props='{"network": "mainnet", "TESTING_PARAM": "test_value"}'`
4. Start node: `bv node start <node_id/node_name>`
5. Call `bv node capabilities <node_id/node_name>`, to see available functions.
6. Edit & Save script located in `/var/lib/blockvisor/node/<node_id>.rhai`.
7. Run function(s) from script, that you want to test, via BV CLI: `bv node run <node_id/node_name> <function_name> [--param='string argument passed to the function']`
8. Repeat 6-7 until done, then remember to copy script, to safe location, since it will be removed with the node on `bv node delete <node_id/node_name>` call.
