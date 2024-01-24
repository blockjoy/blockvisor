const METADATA = #{
    // comments are allowed
    min_babel_version: "0.0.9",
    kernel: "5.10.174-build.1+fc.ufw",
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
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 512,
        ramdisks: [
            #{
                ram_disk_mount_point: "/mnt/ramdisk",
                ram_disk_size_mb: 512,
            },
        ]
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
};

const API_HOST = "http://localhost:4467/";

fn init(keys) {
    let res = run_sh("echo 'noop for testing'");
    if res.exit_code != 0  {
        throw res;
    }
    let param = sanitize_sh_param(node_params().TESTING_PARAM);
    start_job("upload", #{
        job_type: #{
            run_sh: "echo dummy_upload",
        },
        restart: "never",
        shutdown_signal: "SIGINT",
        needs: [],
    });
    create_job("echo", #{
        job_type: #{
            run_sh: "echo \"Testing entry_point not configured, but parametrized with " + param + "!\"",
        },
        restart: #{
            always: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
        },
        shutdown_signal: "SIGINT",
        needs: ["upload"],
    });
    start_job("echo");
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
                backoff_base_ms: 100,
                max_retries: 1,
            },
        },
        shutdown_timeout_secs: 3000
    });
    "Download started!"
}

fn sh_inside(param) {
    let res = run_sh(param);
    if res.exit_code != 0 {
        throw res.stderr;
    }
    res.stdout
}

fn upload(param) {
    stop_job("echo");
    start_job("upload", #{
        job_type: #{
            upload: #{
                source: "/blockjoy/miner/data",
                exclude: [
                    "**/something_to_ignore*",
                    ".gitignore",
                    "some_subdir/*.bak",
                ],
                compression: #{
                    ZSTD: 5,
                },
                number_of_chunks: 9,
                url_expires_secs: 900,
                data_version: 1,
            }
        },
        restart: #{
            on_failure: #{
                backoff_timeout_ms: 600000,
                backoff_base_ms: 100,
                max_retries: 1,
            },
        },
    });
    start_job("echo");
    "Upload started!"
}

fn start_progress_job(name) {
    start_job(name, #{
        job_type: #{
            run_sh: "i=0;while true; do echo {\\\"current\\\":$i,\\\"total\\\":100,\\\"message\\\":\\\"items\\\"} > /var/lib/babel/jobs/status/" + name + ".progress; i=$((1+i%100)); sleep 1; done",
        },
        restart: "never",
    });
    "job " + name + " started"
}

fn test_height_value(param) {
    if height() < 0 {
        throw "Invalid node height value: " + height();
    }
}
