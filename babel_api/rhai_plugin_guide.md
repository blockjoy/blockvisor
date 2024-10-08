# Rhai Plugin Scripting Guide

## Introduction

This is user guide on how to create a "bridge" between protocol specific SW and Blockvisor (aka BV),
by implementing Babel Plugin.
Babel Plugin translates BV protocol agnostic interface (aka Babel API) into protocol specific calls.

Currently, Babel Plugin can be implemented using Rhai scripting language.
See [The Rhai Book](https://rhai.rs/book) for more details on Rhai language itself.

### Core of All Babel Plugins

1. `const PLUGIN_CONFIG` - The easiest way to describe node initialization and background services. Define this constant
as described in [PLUGIN_CONFIG](#plugin_config) chapter, to use default implementation for `init()` and `upload()` functions.
2. Dynamic node configuration is accessible via `node_params()` function. This can and should be used in order
to change the behavior of the script according to the node configuration.
3. While protocol data are typically huge, BV provides built-in facility for efficient uploading and downloading
protocol archives.
<br>See [Protocol Data Archives](#protocol-data-archives) chapter for more details.

## Plugin Interface

To add some specific protocol support Babel Plugin must tell BV how to use protocol specific tools inside the image,
to properly setup and maintain protocol node. This chapter describe interface that needs to be implemented by script
for this purpose.

### PLUGIN_CONFIG

`PLUGIN_CONFIG` constant is a convenient way to specify following things:
 - node initialization process
 - download job configuration
 - alternative download command
 - post-download actions
 - configuration for background services, to be run on the node
 - pre-upload actions
 - upload job configuration

See [example](examples/plugin_config.rhai) with comments for more details.

### BASE_CONFIG

`BASE_CONFIG` constant is a convenient way to define configuration files and services that are shared between multiple plugins.
Config files defined in `BASE_CONFIG` are rendered before anything else in `init()` function. Services defined in `BASE_CONFIG`
are started right after config files are rendered, and are not stopped with other protocol services e.g. during data upload.

See [example](examples/base.rhai) with comments for more details.

### Functions that SHALL be implemented by Plugin

Functions listed below are required by BV to work properly.

- `init` - Main entrypoint where everything starts. Function called only once, when node is started first time.
  <br>It is the place where protocol services running in background should be described as so called `jobs`.
  Also, any other one-off operations can be done here (e.g. initializing stuff, or downloading protocol data archives).
  <br>See [Engine Interface](#engine-interface) and [Background Jobs](#background-jobs) chapter for more details on how to start jobs.
  <br> BV provide default implementation if [PLUGIN_CONFIG](#plugin_config) constant is defined.
  Default implementation run all init commands and start init jobs according to config,
  then try to start build-in download job, if fail then fallback to alternative download (if provided),
  finally starts configured services.

  See [minimalistic example](examples/init_minimal.rhai) or go to [Protocol Data Archives](#protocol-data-archives) chapter for example including download step)
- `application_status()` - Returns protocol application status.
  <br>**Allowed return values**: _provisioning_, _broadcasting_, _cancelled_, _delegating_, _delinquent_, _disabled_, _earning_, _electing_, _elected_, _exported_, _ingesting_, _mining_, _minting_, _processing_, _relaying_, _delete_pending_, _deleting_, _deleted_, _provisioning_pending_, _update_pending_, _updating_, _initializing_, _downloading_, _uploading_
  <br>**Following values indicate that BV should not gather metrics for given node**: _initializing_, _downloading_, _uploading_

### Functions that SHOULD be implemented by Plugin

Functions listed below are used BV for collecting metrics and status monitoring.
It is recommended to implement all of them, if feasible.

- `height()` - Returns the height of the blockchain (in blocks).
- `block_age()` - Returns the block age of the blockchain (in seconds).
- `address()` - The address of the node. The meaning of this varies from protocol to protocol.
  <br>**Example**: _/p2p/11Uxv9YpMpXvLf8ZyvGWBdbgq3BXv8z1pra1LBqkRS5wmTEHNW3_
- `name()` - Returns the name of the node. This is usually some random generated name that you may use
to recognise the node, but the purpose may vary per protocol.
  <br>**Example**: _chilly-peach-kangaroo_
- `consensus()` - Returns `bool` whether this node is in consensus or not.
- `upload()` - Upload protocol data snapshot to cloud storage, so it can be quickly reused by newly created nodes.
  <br> BV provide default implementation if [PLUGIN_CONFIG](#plugin_config) constant is defined.
  Default implementation stop all services, start upload job according to config, and then start services again.

Beside above functions there should be `BABEL_VERSION` constant defined, defining minimum version of babel, required
by this script. E.g: `const BABEL_VERSION = "0.55.0";`.

See [example](examples/polygon_functions.rhai) of functions implemented for Polygon.

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
- `run_jrpc(request)` - Execute Jrpc request to the current node and return [HttpResponse](#httpresponse) (with default 15s timeout). Request must have following structure:
```rust
{
  // This is the host for the JSON rpc request.
  host: String,
  // The name of the jRPC method that we are going to call into.
  method: String,
  // [optional] Params structure in form of Dynamic object that is serializable into JSON.
  params: Dynamic,
  // [optional] Extra HTTP headers to be added to request.
  headers: HeadersMap
}
```
- `run_jrpc(request, timeout)` - Same as above, but with custom request timeout (in seconds).
- `run_rest(request)` - Execute a Rest request to the current node and return [HttpResponse](#httpresponse) (with default 15s timeout). Request must have following structure:
```rust
{
  // This is the url of the rest endpoint.
  url: String,
  // [optional] Extra HTTP headers to be added to request.
  headers: HeadersMap
}
```
- `run_rest(request, timeout)` - Same as above, but with custom request timeout (in seconds).
- `run_sh(body)` - Run Sh script on the node and return [ShResponse](#shresponse) (with default 15s timeout).
- `run_sh(body, timeout)` - Same as above, but with custom execution timeout (in seconds).
- `parse_hex(hex)` - Convert `0x` hex string into decimal number.
- `parse_json(json)` - Parse json string into a Rhai object.
- `system_time()` - Get system time in seconds since UNIX EPOCH.
- `parse_rfc3339(date_time)` - Parse an RFC 3339 date-and-time string into seconds since UNIX EPOCH.
- `parse_rfc2822(date_time)` - Parse an RFC 2822 date-and-time string into seconds since UNIX EPOCH.
- `parse_time(date_time, format)` - Parse date-and-time string into seconds since UNIX EPOCH, with respect to given format.
  See [chrono::format::strftime](https://docs.rs/chrono/latest/chrono/format/strftime/index.html) for format details.
- `sanitize_sh_param(param)` - Allowing people to substitute arbitrary data into sh-commands is unsafe.
  Call this function over each value before passing it to `run_sh`. This function is deliberately more
  restrictive than needed; it just filters out each character that is not a number or a
  string or absolutely needed to form an url or json file.
- `render_template(template, destination, params)` - This function renders configuration template with provided `params`.
  See [Tera Docs](https://tera.netlify.app/docs/#templates) for details on templating syntax.
  `params` is expected to be Map object serializable to JSON.
  It assumes that file pointed by `template` argument exists.
  File pointed by `destination` path will be overwritten if exists.
- `node_params()` - Get node params as key-value map.
- `node_env()` - Get node environment/context metadata, see [NodeEnv](#nodeenv).
- `add_task(task_name, schedule, function_name, function_param)` - Schedule Rhai function with given `function_name` and `function_param` (optional), according to cron-compatible `schedule` string.
- `delete_task(task_name)` - Delete previously scheduled task.
- `save_data(value)` - Save plugin data to persistent storage. It takes string as argument. `to_json` can be used for more complex structures.
- `load_data()` - Load plugin data from persistent storage. Returns string (as it was passed to `save_data`).
- `put_secret(key, value)` - Put plugin secret data to encrypted cloud storage. It takes string `key` and BLOB `value` as arguments. Use `to_blob` if you want to store simple string.
- `get_secret(key)` - Get plugin secret data from encrypted cloud storage. It takes string `key` and returns BLOB (as it was passed to `put_secret`). Use `as_string` if you expect simple string.
- `file_write(path, content)` - Write binary `content` (BLOB) into node filesystem. Use `to_blob` if you want to store simple string.
- `file_read(path)` - Read binary content (BLOB) from node filesystem. Use `as_string` if you expect simple string.
- `DATA_DRIVE_MOUNT_POINT` - Globally available constant, containing absolute path to directory where data drive is mounted.
- `PROTOCOL_DATA_PATH` - Globally available constant, containing absolute path to directory where protocol data are stored.
  This is the path, where protocol data archives are downloaded to, and where are uploaded from.

### HttpResponse

```rust
struct HttpResponse { 
  status_code: i32,
  body: String,
}
```
Above structure provide following helper functions:
- `expect(expected_code)` - Check if `status_code` match expected one and then return `body` parsed as json.
- `expect(check)` - If provided `check` function return `true`, then return `body` parsed as json (e.g. `http_resp.expect(|code| code >= 200).json_field`).

### ShResponse

```rust
struct ShResponse{ 
  exit_code: i32,
  stdout: String,
  stderr: String,
}
```
Above structure provide following helper function:
- `unwrap()` - Check if `exit_code` is 0 and then return `stdout`.

### NodeEnv
```rust
pub struct NodeEnv {
    /// Node id.
    pub node_id: String,
    /// Node name.
    pub node_name: String,
    /// Node version.
    pub node_version: String,
    /// Node protocol.
    pub node_protocol: String,
    /// Node variant.
    pub node_variant: String,
    /// Node IP.
    pub node_ip: String,
    /// Node gateway IP.
    pub node_gateway: String,
    /// Indicate if node run in dev mode.
    pub dev_mode: bool,
    /// Host id.
    pub bv_host_id: String,
    /// Host name.
    pub bv_host_name: String,
    /// API url used by host.
    pub bv_api_url: String,
    /// Organisation id to which node belongs to.
    pub org_id: String,
}
```

### Background Jobs

Background job is a way to asynchronously run long-running tasks. Currently, three types of tasks are supported:
1. `run_sh` - arbitrary long-running shell script.
2. `download` - download data (e.g. previously archived protocol data, to speedup init process).
3. `upload` - upload data (e.g. archive protocol data).
In particular, it can be used to define protocol service(s) i.e. background process(es) that are automatically started
with the node.

Each background job has its __unique__ name and configuration structure described by following [example](examples/jobs.rhai).

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

## Protocol Data Archives

### Uploading Data Archives

To upload protocol data archive on remote object storage, following steps are needed:
1. Stop all protocol synchronization. Protocol data should not be modified while uploading.
2. Start upload job.
3. Once upload is finished protocol synchronization can be turned on again.

Above steps can be implemented in any custom rhai function (typically named `upload`, but name can be arbitrary).
<br>See [example](examples/custom_download_upload.rhai) for details. See also example in [Background Jobs](#background-jobs) chapter for all possible
upload job config options.

To trigger upload, simply call `bv node run upload`.

Define [PLUGIN_CONFIG](#plugin_config) constant to use default implementation for `upload`.

### Downloading Data Archives

Once protocol data archive is available on remote object storage, it can be used to speedup new nodes setup.

To use default implementation define [PLUGIN_CONFIG](#plugin_config) constant. Otherwise
see [example](examples/custom_download_upload.rhai) of custom `init()` function.

Typically, download job is started in `init()` before other protocol services are started. This is achieved by job
`needs` configuration. See also example in [Background Jobs](#background-jobs) chapter for all possible
download job config options. 

## Common Use Cases

### Add Custom HTTP Headers to JRPC and REST Requests

`request` parameter in `run_jrpc` and `run_rest` functions has optional `headers` field,
that can be used to add custom HTTP headers to the request.

**Example:**
```
let data = #{host: "localhost:8154",
             method: "getBlockByNumber",
             headers: [["content-type", "application/json"]]
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
             headers: [["content-type", "application/json"]]
            };
run_jrpc(data);
```

### Handling JRPC Output

`run_jrpc` function return raw http response. Use `expect` method or `parse_json` on response body, to easily access json fields.

**Example:**
```
const API_HOST = "http://localhost:4467/";

fn block_age() {
    let resp = run_jrpc(#{host: global::API_HOST, method: "info_block_age"}).expect(200);
    resp.result.block_age
}
```

### Output Mapping

Rhai language has convenient `switch` statement, which is very similar to Rust `match`.
Hence, it is a good candidate for output mapping. 

**Example:**
```
fn status() {
    let stdout = run_sh("get_status").unwrap();
    let status = switch stdout {
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

### Put and Get Plugin Secrets

Plugin specific secrets (e.g. node key or config) can be persisted in encrypted cloud storage with `put/get_secret` functions. It takes/returns BLOB for given key.

**Example:**
```
fn custom_function(arg) {
    let peer_key;
    let peer_id;
    try {
        peer_key = get_secret("key");
        peer_id = get_secret("id").as_string();
    } (err) catch {
        if err == "not_found" {
            peer_key = file_read("/blockjoy/peer.key");
            peer_id = file_read("/blockjoy/peer.id").as_string();
            put_secret("key", peer_key);
            put_secret("id", peer_id.to_blob());
        } else {
           throw err;
        }
    };
    // do stuff with peer_key and peer_id
    arg
}
```

### Rendering Configuration File

Some protocols may require configuration files to be filled up with some user data. For that `render_template` can be used.

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
    render_template("/etc/protocol.config.template", "/etc/protocol.config", params);
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

See [test_examples.rs](tests/test_examples.rs) for example.
