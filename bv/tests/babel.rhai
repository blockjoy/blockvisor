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
        log_buffer_capacity_mb: 4,
        swap_size_mb: 512,
        ramdisks: [
            #{
                ram_disk_mount_point: "/mnt/ramdisk",
                ram_disk_size_mb: 512,
            },
        ]
    },
    firewall: #{
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

const BABEL_VERSION = "0.9.0";
const PLUGIN_CONFIG = #{
    init: #{
        commands: [
            `echo 'noop for testing'`,
            `mkdir -p /blockjoy/.babel_jobs/ && touch /blockjoy/.babel_jobs/download.completed`,
        ],
        jobs: [
            #{
                name: "init_job",
                run_sh: `echo dummy_init && sleep 1`,
            },
        ],
    },
    services: [
        #{
            name: "echo",
            run_sh: "echo \"Testing entry_point not configured, but parametrized with " + sanitize_sh_param(node_params().TESTING_PARAM) + "!\"",
            restart_config: #{
                backoff_timeout_ms: 60000,
                backoff_base_ms: 10000,
            },
            shutdown_signal: "SIGINT",
        },
    ],
    upload: #{
        exclude: [
            "**/something_to_ignore.*",
            ".gitignore",
            "some_subdir/*.bak",
        ],
        compression: #{
            ZSTD: 5,
        },
        number_of_chunks: 9,
        url_expires_secs: 900,
        data_version: 1,
        restart_config: #{
            backoff_timeout_ms: 60000,
            backoff_base_ms: 100,
            max_retries: 1,
        },
    },
};

/// Returns blockchain application status.
/// SHALL be implemented
fn application_status() {
    "broadcasting"
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

fn sync_status() {
    "synced"
}

fn staking_status() {
    "staking"
}

fn info(param) {
    param
}

fn download(param) {
    start_job("download", #{
        job_type: #{
            download: #{
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

fn start_progress_job(name) {
    start_job(name, #{
        job_type: #{
            run_sh: "i=0;while true; do echo {\\\"current\\\":$i,\\\"total\\\":100,\\\"message\\\":\\\"items\\\"} > /var/lib/babel/jobs/status/" + name + ".progress; i=$((1+i%100)); sleep 1; done",
        },
        restart: "never",
    });
    "job " + name + " started"
}
