const NETS = #{
    main: #{
        net_type: "main",
    },
};

const A_DIR = BLOCKCHAIN_DATA_PATH + "/A/";
const B_DIR = BLOCKCHAIN_DATA_PATH + "/B/";
const NET =  global::NETS[node_params().NETWORK];

fn stop_blockchain() {
    stop_job("blockchain_service_a");
    stop_job("blockchain_service_b");
}

fn start_blockchain(needed) {
    start_job("blockchain_service_a", #{
        job_type: #{
            run_sh: `/usr/bin/blockchain_service_a --chain=${global::NET.net_type} --datadir=${global::A_DIR} --snapshots=false`,
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
            run_sh: `/usr/bin/blockchain_service_b start --home=${global::B_DIR} --chain=${global::NET.net_type} --rest-server --seeds ${global::NET.seeds} "$@"`,
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

fn init() {
    let response = run_sh(`mkdir -p ${global::A_DIR} ${global::B_DIR} && mkdir -p /opt/netdata/var/cache/netdata && mkdir -p /opt/netdata/var/lib/netdata && rm -rf /opt/netdata/var/lib/netdata/* && rm -rf /opt/netdata/var/cache/netdata/*`).unwrap();
    debug(`Response from shell command: ${response}`);

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
