const METADATA = #{
    // comments are allowed
    min_babel_version: "0.0.9",
    node_version: "1.15.9",
    protocol: "testing",
    node_type: "validator",
    description: "testing blockchain validator",
    requirements: #{
        vcpu_count: 1,
        mem_size_mb: 2048,
        disk_size_gb: 1,
    },
    nets: #{
        test: #{
            url: "https://testnet-api.helium.wtf/v1/",
            net_type: "test",
        },
        main: #{
            url: "https://rpc.ankr.com/eth",
            net_type: "main",
        },
    },
    babel_config: #{
        data_directory_mount_point: "/blockjoy/miner/data",
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 512,
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
    keys: #{
        first: "/opt/secrets/first.key",
        second: "/opt/secrets/second.key",
        "*": "/tmp",
    },
};

const API_HOST = "http://localhost:4467/";

fn init(keys) {
    let res = run_sh("echo 'noop for testing'");
    if res.exit_code != 0  {
        throw res;
    }
    let param = sanitize_sh_param(node_params().TESTING_PARAM);
    start_job("echo", #{
        job_type: #{
            run_sh: "echo \"Testing entry_point not configured, but parametrized with " + param + "!\"",
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        needs: [],
    });
}

fn height() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_height"}).body).result.height
}

fn block_age() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_block_age"}).body).result.block_age
}

fn name() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_name"}).body).result.name.to_string()
}

fn address() {
    parse_json(run_jrpc(#{ host: global::API_HOST, method: "peer_addr"}).body).result.peer_addr.to_string()
}

fn consensus() {
    false
}

fn application_status() {
    "broadcasting"
}

fn sync_status() {
    "synced"
}

fn staking_status() {
    "staking"
}

fn generate_keys() {
    run_sh("mkdir -p /opt/secrets && echo first > /opt/secrets/first.key && echo second > /opt/secrets/second.key");
    return;
}

fn stop_babelsup(param) {
    run_sh("systemctl stop babelsup");
    for line in run_sh("ps -e|grep babel").stdout.split("\n") {
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
    run_sh("systemctl disable babelsup").stdout
}

fn info(param) {
    let info;
    try {
        info = load_data()
    } catch {
        info = parse_json(run_jrpc(#{ host: global::API_HOST, method: "info_summary"}).body).result.to_string();
        save_data(info);
    };
    info
}

fn custom_download(param) {
    start_job("custom_download", #{
        job_type: #{
            download: #{
                destination: "/blockjoy/miner/data",
                manifest: parse_json(param),
            }
        },
        restart: "never",
    });
    ""
}

fn download(param) {
    start_job("download", #{
        job_type: #{
            download: #{
                destination: "/blockjoy/miner/data",
            }
        },
        restart: #{
            on_failure: #{
                backoff_timeout_ms: 600000,
                backoff_base_ms: 500,
                max_retries: 1,
            },
        },
    });
    "Download started!"
}

fn download_status(param) {
    job_status("download").to_string()
}

fn data_file_sha1(param) {
    run_sh(`sha1sum /blockjoy/miner/data/${param}`).stdout
}

fn upload(param) {
    start_job("upload", #{
        job_type: #{
            upload: #{
                source: "/blockjoy/miner/data",
                manifest: parse_json(param),
            }
        },
        restart: #{
            on_failure: #{
                backoff_timeout_ms: 600000,
                backoff_base_ms: 5000,
                max_retries: 3,
            },
        },
    });
    ""
}

fn upload_status(param) {
    job_status("upload").to_string()
}

fn test_height_value(param) {
    if height() < 0 {
        throw "Invalid node height value: " + height();
    }
}
