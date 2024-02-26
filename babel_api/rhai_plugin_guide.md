# Rhai Plugin Scripting Guide

## Introduction

This is user guide on how to create a "bridge" between blockchain specific SW and Blockvisor (aka BV),
by implementing Babel Plugin.
Babel Plugin translates BV blockchain agnostic interface (aka Babel API) into blockchain specific calls.

Currently, Babel Plugin can be implemented using Rhai scripting language.
See [The Rhai Book](https://rhai.rs/book) for more details on Rhai language itself.

### Core of All Babel Plugins

1. `const METADATA` - Static image specification like blockchain protocol name, but also include default node configuration.
2. `fn init(params)` - Main entrypoint where everything starts. Function called only once, when node is started first time.
<br>It is the place where blockchain services running in background should be described as so called `jobs`.
Also, any other one-off operations can be done here (e.g. initializing stuff, or downloading blockchain data archives).
<br>See [Functions that SHALL be implemented by Plugin](#functions-that-shall-be-implemented-by-plugin)
and [Background Jobs](#background-jobs)
chapters for more details on `init()` function and `jobs`.
3. While image specification is statically defined in `METADATA`, dynamic node configuration is accessible via `node_params()`
function, in particular `node_params().NETWORK`. This can and should be used in order
to change the behavior of the script according to the node configuration.
4. While blockchain data are typically huge, BV provides built-in facility for efficient uploading and downloading
blockchain archives.
<br>See [Blockchain Data Archives](#blockchain-data-archives) chapter for more details.

## Plugin Interface

To add some specific blockchain support Babel Plugin must tell BV how to use blockchain specific tools inside the image,
to properly setup and maintain blockchain node. This chapter describe interface that needs to be implemented by script
for this purpose.

### Blockchain METADATA

First thing that Babel Plugin SHALL provide is Blockchain Metadata structure that describe static properties
of the blockchain. In Rhai script it is done by declaring `METADATA` constant.
See below example with comments for more details.

**Example:**
```protobuf
const METADATA = #{
    // A semver version of the babel program, indicating the minimum version of the babel
    // program that a babel script is compatible with.
    min_babel_version: "0.0.1",

    // Version of Linux kernel to use in VM.
    kernel: "5.10.174-build.1+fc.ufw",

    // A semver version of the blockchain node program.
    node_version: "1.2.3",
    
    // Name of the blockchain protocol.
    protocol: "example",
    
    // Type of the node (validator, beacon, etc).
    node_type: "node",
    
    // [optional] Some description of the node.
    description: "example blockchain node",
    
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
            url: "https://test.example.blockchain/",

            // Blockchain network type.
            // Allowed values: "dev", "test", "main"
            net_type: "test",

            // [optional] Custom network metadata can also be added.
            seeds: "1500161dd491b67fb1ac81868952be49e2509c9f@52.78.36.216:26656,dd4a3f1750af5765266231b9d8ac764599921736@3.36.224.80:26656,8ea4f592ad6cc38d7532aff418d1fb97052463af@34.240.245.39:26656,e772e1fb8c3492a9570a377a5eafdb1dc53cd778@54.194.245.5:26656,6726b826df45ac8e9afb4bdb2469c7771bd797f1@52.209.21.164:26656",
        },
        main: #{
            url: "https://main.example.blockchain/",
            net_type: "main",
            seeds: "9df7ae4bf9b996c0e3436ed4cd3050dbc5742a28@43.200.206.40:26656,d9275750bc877b0276c374307f0fd7eae1d71e35@54.216.248.9:26656,1a3258eb2b69b235d4749cf9266a94567d6c0199@52.214.83.78:26656",
        },
    },
    
    // Configuration of Babel - agent running inside VM.
    babel_config: #{
        // Capacity of log buffer (in lines).
        log_buffer_capacity_ln: 1024,
        
        // Size of swap file created on the node, in MB.
        swap_size_mb: 512,

        // Location of swap file.
        swap_file_location: "/swapfile",

        // Set RAM disks inside VM
        ramdisks: [
            #{
                // Path to mount RAM disk to.
                ram_disk_mount_point: "/mnt/ramdisk",
                // RAM disk size, in MB.
                ram_disk_size_mb: 512,
            },
        ]

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
};
```

### Functions that SHALL be implemented by Plugin

Functions listed below are required by BV to work properly.

- `init` - Function that is called by BV on first node start. It takes `params` key-value map as argument.
  It shall do all required initializations and start required background jobs. See 'Engine Interface' chapter for more details on how to start jobs.
  Once node is **successfully** started it is not called by BV anymore. Hence `init` function may be called more than
  once only if node start failed for some reason.

**Minimalistic Example (see [Blockchain Data Archives](#blockchain-data-archives) chapter for example including download step):**
```
fn init(params) {
  let A_DIR = BLOCKCHAIN_DATA_PATH + "/A/";
  let B_DIR = BLOCKCHAIN_DATA_PATH + "/B/";
  let NET =  global::METADATA.nets[node_params().NETWORK];

  let response = run_sh(`mkdir -p ${A_DIR} ${B_DIR} && mkdir -p /opt/netdata/var/cache/netdata && mkdir -p /opt/netdata/var/lib/netdata && rm -rf /opt/netdata/var/lib/netdata/* && rm -rf /opt/netdata/var/cache/netdata/*`);
    debug(`Response from shell command ${cmd}': ${response}`);
    if response.exit_code != 0 {
        error(`init failed failed: ${response}`);
        throw response;
    }

    start_job("blockchain_service_a", #{
        job_type: #{
            run_sh: `/usr/bin/blockchain_service_a --chain=${NET.net_type} --datadir=${B_DIR} --snapshots=false`,
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 1000,
            },
        },
    });

    start_job("blockchain_service_b", #{
        job_type: #{
            run_sh: `/usr/bin/blockchain_service_b start --home=${B_DIR} --chain=${NET.net_type} --rest-server --seeds ${NET.seeds} "$@"`,
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 1000,
            },
        },
    });  
}
```

- `application_status()` - Returns blockchain application status.
  <br>**Allowed return values**: _provisioning_, _broadcasting_, _cancelled_, _delegating_, _delinquent_, _disabled_, _earning_, _electing_, _elected_, _exported_, _ingesting_, _mining_, _minting_, _processing_, _relaying_, _delete_pending_, _deleting_, _deleted_, _provisioning_pending_, _update_pending_, _updating_, _initializing_, _downloading_, _uploading_

### Functions that SHOULD be implemented by Plugin

Functions listed below are used BV for collecting metrics and status monitoring.
It is recommended to implement all of them, if feasible.

- `height()` - Returns the height of the blockchain (in blocks).
- `block_age()` - Returns the block age of the blockchain (in seconds).
- `address()` - The address of the node. The meaning of this varies from blockchain to blockchain.
  <br>**Example**: _/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3_
- `name()` - Returns the name of the node. This is usually some random generated name that you may use
to recognise the node, but the purpose may vary per blockchain.
  <br>**Example**: _chilly-peach-kangaroo_
- `consensus()` - Returns `bool` whether this node is in consensus or not.
- `sync_status()` - Returns blockchain synchronization status.
  <br>**Allowed return values**: _syncing_, _synced_
- `staking_status()` - Returns blockchain staking status.
  <br>**Allowed return values**: _follower_, _staked_, _staking_, _validating_, _consensus_, _unstaked_

#### Examples of functions implemented for Polygon
```
const POLYGON_RPC_URL = "http://localhost:8545";

fn address(){
    run_sh(`heimdalld show-account | grep address | awk -F\" '{ print $4}'`).to_string();
}

fn application_status() {
    let res = run_jrpc(#{
        host: `${global::POLYGON_RPC_URL}`,
        method: "eth_blockNumber",
        params: [],
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
       "delinquent";
    } else {
        "broadcasting";
    }
}

fn height(){
    let res = run_jrpc(#{
        host: `${global::POLYGON_RPC_URL}`,
        method: "eth_blockNumber",
        params: [],
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    let hex = parse_json(res.body);
    parse_int(sub_string(hex.result,2),16)
}

fn block_age(){
    let res = run_jrpc(#{
        host: `${global::POLYGON_RPC_URL}`,
        method: "eth_getBlockByNumber",
        params: ["latest", false],
        headers: #{"Content-Type" : "application/json"},
      });
    if res.status_code != 200 {
        throw res.status_code;
    }
    return 0;
}
```

### Functions that MAY be implemented by Plugin

Plugin may additionally implement an arbitrary custom function that will be then accessible
via BV CLI interface. The only limitation is that custom functions must take only one "string"
argument (it may be more complex structure, but e.g. serialized as JSON) and must return "string" as well.

**Example:**
```
fn some_custom_function(arg) {
    let input = parse_json(arg);
    let output = #{
        result: input.value * 2
    };
    output.to_json();
}
```

## Engine Interface

To make implementation of Babel Plugin interface possible, BV provides following functions to Rhai script.

- `create_job(job_name, job_config)` - Create background job with unique name. See 'Backgound Jobs' section for more details.
- `start_job(job_name, job_config)` - Create and immediately start background job with unique name.
- `start_job(job_name)` - Start previously created background job with given name.
- `stop_job(job_name)` - Stop background job with given unique name if running.
- `job_status(job_name)` - Get background job status by unique name.
  <br>**Possible return values**: _pending_, _running_, _stopped_, _finished{exit_code, message}_
- `run_jrpc(request)` - Execute Jrpc request to the current blockchain. Request must have following structure:
```rust
{
  // This is the host for the JSON rpc request.
  host: String,
  // The name of the jRPC method that we are going to call into.
  method: String,
  // [optional] Params structure in form of Dynamic object that is serializable into JSON.
  params: Dynamic,
  // [optional] Extra HTTP headers to be added to request.
  headers: Map
}
```
Return http response (with default 15s timeout) as following structure:
```rust
{ 
  status_code: i32,
  body: String,
}
```
- `run_jrpc(request, timeout)` - Same as above, but with custom request timeout (in seconds).
- `run_rest(request)` - Execute a Rest request to the current blockchain. Request must have following structure:
```rust
{
  // This is the url of the rest endpoint.
  url: String,
  // [optional] Extra HTTP headers to be added to request.
  headers: Map
}
```
Return its http response (with default 15s timeout) as following structure:
```rust
{ 
  status_code: i32,
  body: String,
}
```
- `run_rest(request, timeout)` - Same as above, but with custom request timeout (in seconds).
- `run_sh(body)` - Run Sh script on the blockchain VM and return its result (with default 15s timeout). Result is following structure:
```rust
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
  `params` is expected to be Map object serializable to JSON.
  It assumes that file pointed by `template` argument exists.
  File pointed by `output` path will be overwritten if exists.
- `node_params()` - Get node params as key-value map.
- `save_data(value)` - Save plugin data to persistent storage. It takes string as argument. `to_json` can be used for more complex structures.
- `load_data()` - Load plugin data from persistent storage. Returns string (as it was passed to `save_data`).
- `DATA_DRIVE_MOUNT_POINT` - Globally available constant, containing absolute path to directory where data drive is mounted.
- `BLOCKCHAIN_DATA_PATH` - Globally available constant, containing absolute path to directory where blockchain data are stored.
  This is the path, where blockchain data archives are downloaded to, and where are uploaded from.

### Background Jobs

Background job is a way to asynchronously run long-running tasks. Currently, three types of tasks are supported:
1. `run_sh` - arbitrary long-running shell script.
2. `download` - download data (e.g. previously archived blockchain data, to speedup init process).
3. `upload` - upload data (e.g. archive blockchain data).
In particular, it can be used to define blockchain entrypoint(s) i.e. background process(es) that are automatically started
with the node.

Each background job has its __unique__ name and configuration structure described by following example.

**Example:**
```protobuf
    let job_config_A = #{
        job_type: #{
            // Sh script body
            run_sh: "echo \"some initial job done\"",
        },

        // Job restart policy.
        // "never" indicates that this job will never be restarted, whether succeeded or not - appropriate for jobs
        // that can't be simply restarted on failure (e.g. need some manual actions).
        restart: "never",

        // [optional] Job shutdown timeout - how long it may take to gracefully shutdown the job.
        // After given time job won't be killed, but babel will rise the error.
        // If not set default to 60s.
        shutdown_timeout_secs: 15,

        // [optional] POSIX signal that will be sent to child processes on job shutdown.
        // See [man7](https://man7.org/linux/man-pages/man7/signal.7.html) for possible values.
        // If not set default to `SIGTERM`.
        shutdown_signal: "SIGINT",
    };
    create_job("job_name_A", job_config_A);

    let job_config_B = #{
            job_type: #{
                download: #{
                    // [optional] Maximum number of parallel opened connections.
                    max_connections: 5,
                    // [optional] Maximum number of parallel workers.
                    max_runners: 8,                    
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
                backoff_base_ms: 1000,
                
                // [optional] maximum number of retries, or there is no such limit if not set
                max_retries: 5,
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
    start_job("job_name_A");
    start_job("unique_entrypoint_name", entrypoint_config);

    let upload_job_config = #{
        job_type: #{
            upload: #{
                // [optional] List of exclude patterns. Files in `BLOCKCHAIN_DATA_PATH` directory that match any of pattern,
                // won't be taken into account.
                exclude: ["file_to_be_excluded", "or_more/**"],
                // [optional] Compression to be used on chunks.
                compression: #{
                    ZSTD: 3, // compression level
                },
                // [optional] Maximum number of parallel opened connections.
                max_connections: 4,
                // [optional] Maximum number of parallel workers.
                max_runners: 8,
                // [optional] Number of chunks that blockchain data should be split into.
                // Recommended chunk size is about 1GB. Estimated by BV based on data size, if not provided.
                number_of_chunks: 700,
                // [optional] Seconds after which presigned urls in generated `UploadManifest` may expire.
                url_expires_secs: 3600,
                // [optional] Version number for uploaded data. Auto-assigned if not provided.
                data_version: 3,
            },
        },
        restart: #{
            on_failure: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 1000,
                max_retries: 5,
            },
        },
    };
    start_job("upload_job_name", upload_job_config);
```

Once job has been created, other functions in the script may fetch for its state with `job_status(job_name)`, start it
or stop on demand with `start_job(job_name)`/`stop_job(job_name)`.

### Logging

Rhai provides `print` and `debug` functions to simply print into stdout. To make plugin logs consistent with BV logs,
it is recommended to use following log functions:

- `debug` (overridden built-in `debug` implementation)
- `info`
- `warn`
- `error`

**Example:**
```
fn some_function() {
    info("some important step logged with INFO level");
}
```
Output:
```
<time>  INFO babel_api::rhai_plugin: node_id: <node_id>|some important step logged with INFO level 
```

## Blockchain Data Archives

### Uploading Data Archives

To upload blockchain data archive on remote object storage, following steps are needed:
1. Stop all blockchain synchronization. Blockchain data should not be modified while uploading.
2. Start upload job.
3. Once upload is finished blockchain synchronization can be turned on again.

Above steps can be implemented in any custom rhai function (typically named `upload`, but name can be arbitrary).
<br>See below example for details. See also example in [Background Jobs](#background-jobs) chapter for all possible
upload job config options.

To trigger upload, simply call `bv node run upload`.

### Downloading Data Archives

Once blockchain data archive is available on remote object storage, it can be used to speedup new nodes setup.

Typically, download job is started in `init()` before other blockchain services are started. This is achieved by job
`needs` configuration.
<br>See below example for details. See also example in [Background Jobs](#background-jobs) chapter for all possible
download job config options. 

### Example
```
const A_DIR = BLOCKCHAIN_DATA_PATH + "/A/";
const B_DIR = BLOCKCHAIN_DATA_PATH + "/B/";
const NET =  global::METADATA.nets[node_params().NETWORK];

fn stop_blockchain() {
    stop_job("blockchain_service_a");
    stop_job("blockchain_service_b");
}

fn start_blockchain(needed) {
    start_job("blockchain_service_a", #{
        job_type: #{
            run_sh: `/usr/bin/blockchain_service_a --chain=${NET.net_type} --datadir=${A_DIR} --snapshots=false`,
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 1000,
            },
        },
        needs: needed,
    });
    start_job("blockchain_service_b", #{
        job_type: #{
            run_sh: `/usr/bin/blockchain_service_b start --home=${B_DIR} --chain=${NET.net_type} --rest-server --seeds ${NET.seeds} "$@"`,
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 1000,
            },
        },
        needs: needed,
    });  
}

fn init(keys) {
    let response = run_sh(`mkdir -p ${A_DIR} ${B_DIR} && mkdir -p /opt/netdata/var/cache/netdata && mkdir -p /opt/netdata/var/lib/netdata && rm -rf /opt/netdata/var/lib/netdata/* && rm -rf /opt/netdata/var/cache/netdata/*`);
    debug(`Response from shell command ${cmd}': ${response}`);
    if response.exit_code != 0 {
        error(`init failed failed: ${response}`);
        throw response;
    }

    try {
        // try built-in download method first, to speedup regular nodes setup
        start_job("download", #{
            job_type: #{
                download: #{
                    max_connections: 5,
                    max_runners: 8,
                },
            },
            restart: #{
                on_failure: #{
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 1000,
                    max_retries: 5,
                },
            },
        });
    } catch {
        // if it fail (maybe not uploaded yet), try blockchain provided archives (if any), to speed up first sync
        start_job("download", #{
            job_type: #{
                run_sh: `/usr/bin/wget -q -O - ${global::SNAPSHOT_UTIL_URL}`,
            },
            restart: #{
                on_failure: #{
                    backoff_timeout_ms: 60000,
                    backoff_base_ms: 10000,
                    max_retries: 3,
                },
            },
        });
    }
    // start blockchain services once download is finished
    start_blockchain(["download"]);
}

fn upload(param) {
    stop_blockchain();
    start_job("upload", #{
        job_type: #{
            upload: #{
                exclude: [
                    "**/something_to_ignore*",
                    ".gitignore",
                    "some_subdir/*.bak",
                ],
                compression: #{
                    ZSTD: 3,
                },
                max_connections: 4,
                max_runners: 12,
            }
        },
        restart: #{
            on_failure: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 1000,
                max_retries: 5,
            },
        },
    });
    // start blockchain services again once upload is finished
    start_blockchain(["upload"]);
    "Upload started!"
}
```

## Common Use Cases

### Referencing METADATA in Functions

METADATA is regular Rhai constant, so it can be easily reference in functions.

**Example:**
```
fn some_function() {
    let net_url = global::METADATA.nets[node_params().NETWORK].url;
}
```

### Add Custom HTTP Headers to JRPC and REST Requests

`request` parameter in `run_jrpc` and `run_rest` functions has optional `headers` field,
that can be used to add custom HTTP headers to the request.

**Example:**
```
let data = #{host: "localhost:8154",
             method: "getBlockByNumber",
             headers: #{"content-type": "application/json"}
            };
run_jrpc(data);
```

### Add custom parameters to JRPC Requests

`request` parameter in `run_jrpc` has optional `params` field,
that can be used to add params to the request.

**Example:**
```
let data = #{host: "localhost:8154",
             method: "getBlockByNumber",
             params: #{"chain": "x"},
             headers: #{"content-type": "application/json"}
            };
run_jrpc(data);
```

### Handling JRPC Output

`run_jrpc` function return raw http response. Use `parse_json` on response body, to easily access json fields.

**Example:**
```
const API_HOST = "http://localhost:4467/";

fn block_age() {
    let resp = run_jrpc(#{host: global::API_HOST, method: "info_block_age"});
    if resp.status_code != 200 {
      throw resp;
    }
    parse_json(resp.body).result.block_age
}
```

### Output Mapping

Rhai language has convenient `switch` statement, which is very similar to Rust `match`.
Hence, it is a good candidate for output mapping. 

**Example:**
```
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

### Running Sh Script

**IMPORTANT:** Whenever user input (`node_params()` in particular) is passed to sh script,
it should be sanitized with `sanitize_sh_param()`, to avoid malicious code injection.

**Example:**
```
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

### Save and Load Plugin Data

Plugin specific data can be persisted with `save/load_data` functions. It takes/returns simple string, but e.g. json can
be easily used for more complex structures.

**Example:**
```
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

### Rendering Configuration File

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
```
fn init(keys) {
    let params = #{
        param1: "value1", 
        param2: 2,
        aParam: ["a", "bb", "ccc"],
    };
    render_template("/etc/blockchain.config.template", "/etc/blockchain.config", params);
}
```

After `init` is called, following file should be created:

```
first_parameter: "value1"
second_parameter: 2
optional_parameter: None
array: ["a","bb","ccc",]
```

## Unit Testing

For now, the easiest way to automatically test Rhai script is to write Rust unit test.

See [test_testing.rs](tests/test_testing.rs) for example.
