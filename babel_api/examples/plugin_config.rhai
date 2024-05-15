/// You can define some constants to use in the code
const NETS = #{
    /// Key is the name of blockchain network
    test: #{
        /// Url for given blockchain network.
        url: "https://testnet-api.helium.wtf/v1/",
        /// Blockchain network type.
        /// Allowed values: "dev", "test", "main"
        net_type: "test",
        /// [optional] Custom network metadata can also be added.
        /// example: beacon_nodes_csv: "http://beacon01.goerli.eth.blockjoy.com,http://beacon02.goerli.eth.blockjoy.com?789",
        seeds: "test seed"
    },
    main: #{
        url: "https://rpc.ankr.com/eth",
        net_type: "main",
        seeds: "main seed"
    },
};

const API_HOST = "http://localhost:4467/";
const A_DIR = BLOCKCHAIN_DATA_PATH + "/A";
const B_DIR = BLOCKCHAIN_DATA_PATH + "/B";
const NET =  global::NETS[node_params().NETWORK];

const BABEL_VERSION = "0.9.0";
const PLUGIN_CONFIG = #{
    /// node init actions.
    init: #{
        /// list of sh commands to be executed first
        commands: [
            `mkdir -p /opt/netdata/var/cache/netdata`,
        ],
        /// list of sh jobs (long running tasks), to be started then
        jobs: [
            #{
                /// Unique job name.
                name: "init_job",
                /// Sh script body.
                run_sh: `openssl rand -hex 32 > ${global::A_DIR}/jwt.txt`,
                /// [optional] InitJob restart policy.
                /// If not set default to "never".
                restart: "never",
                /// [optional] Job shutdown timeout - how long it may take to gracefully shutdown the job.
                /// After given time job won't be killed, but babel will rise the error.
                /// If not set default to 60s.
                shutdown_timeout_secs: 120,
                /// [optional] POSIX signal that will be sent to child processes on job shutdown.
                /// See [man7](https://man7.org/linux/man-pages/man7/signal.7.html) for possible values.
                /// If not set default to `SIGTERM`.
                shutdown_signal: "SIGINT",
                /// [optional] List of job names that this job needs to be finished before start.
                needs: ["other_init_job_name"],
                /// [optional] Run job as a different user.
                run_as: "some_user",
            }
        ]
    },
    /// list of services to be started once init jobs and download is finished
    services: [
        #{
            /// Unique service job name.
            name: "blockchain_service_a",
            /// Sh script body
            run_sh: `/usr/bin/blockchain_service_a start --home=${global::A_DIR} --chain=${global::NET.net_type} --rest-server --seeds ${global::NET.seeds} "$@"`,
            /// [optional] Job restart config.
            /// If not set default to:
            /// #{
            ///    backoff_timeout_ms: 60000,
            ///    backoff_base_ms: 1000,
            /// }
            restart_config: #{
                /// if job stay alive given amount of time (in milliseconds) backoff is reset
                backoff_timeout_ms: 60000,
                /// base time (in milliseconds) for backoff,
                /// multiplied by consecutive power of 2 each time
                backoff_base_ms: 1000,
                /// [optional] maximum number of retries, or there is no such limit if not set
                max_retries: 13,
            },
            /// [optional] Job shutdown timeout - how long it may take to gracefully shutdown the job.
            /// After given time job won't be killed, but babel will rise the error.
            /// If not set default to 60s.
            shutdown_timeout_secs: 120,
            /// [optional] POSIX signal that will be sent to child processes on job shutdown.
            /// See [man7](https://man7.org/linux/man-pages/man7/signal.7.html) for possible values.
            /// If not set default to `SIGTERM`.
            shutdown_signal: "SIGINT",
            /// [optional] Run job as a different user.
            run_as: "some_user",
        },
        #{
            name: "blockchain_service_b",
            run_sh: `/usr/bin/blockchain_service_b --chain=${global::NET.net_type} --datadir=${global::A_DIR} --snapshots=false`,
        }
    ],
    /// [optional] download configuration
    /// built-in download will be started once all init jobs are finished
    download: #{
        /// [optional] Maximum number of parallel opened connections.
        /// If not set default to 3.
        max_connections: 5,
        /// [optional] Maximum number of parallel workers.
        /// If not set default to 8.
        max_runners: 8,
        /// [optional] Job restart config.
        /// If not set default to:
        /// #{
        ///    backoff_timeout_ms: 600000,
        ///    backoff_base_ms: 500,
        ///    max_retries: 10,
        /// }
        restart_config: #{
            backoff_timeout_ms: 60000,
            backoff_base_ms: 1000,
            max_retries: 5,
        },
    },
    /// [optional] alternative download if archive for standard one is not available
    alternative_download: #{
        /// Sh script body.
        run_sh: `/usr/bin/wget -q -O - some_url`,
        /// [optional] InitJob restart config.
        /// If not set default to "never".
        restart_config: #{
            backoff_timeout_ms: 60000,
            backoff_base_ms: 10000,
            max_retries: 3,
        },
        /// [optional] Run job as a different user.
        run_as: "some_user",
    },
    /// [optional] post-download action
    post_download: #{
        /// list of sh commands to be executed after download
        commands: [
            `echo downloaded`,
        ],
        /// list of sh jobs (long running tasks), to be started after download
        jobs: [
            #{
                name: "post_download_job",
                run_sh: `echo restoreDB`,
            }
        ]
    },
    /// [optional] pre-upload action
    pre_upload: #{
        /// list of sh commands to be executed before upload
        commands: [
            `echo uploading`,
        ],
        /// list of sh jobs (long running tasks), to be started before upload
        jobs: [
            #{
                name: "pre_upload_job",
                run_sh: `echo dumpDB`,
            }
        ]
    },
    /// [optional] upload configuration
    /// built-in upload can be manually triggered with `bv node run upload`
    upload: #{
        /// [optional] List of exclude patterns. Files in `BLOCKCHAIN_DATA_PATH` directory that match any of pattern,
        /// won't be taken into account.
        exclude: [
            "**/something_to_ignore*",
            ".gitignore",
            "some_subdir/*.bak",
        ],
        /// [optional] Compression to be used on chunks.
        /// If not set default to `ZSTD: 3`.
        compression: #{
            ZSTD: 5, /// compression level
        },
        /// [optional] Maximum number of parallel opened connections.
        /// If not set default to 3.
        max_connections: 4,
        /// [optional] Maximum number of parallel workers.
        /// If not set default to 8.
        max_runners: 12,
        /// [optional] Number of chunks that blockchain data should be split into.
        /// Recommended chunk size is about 1GB. Estimated by BV based on data size, if not provided.
        /// If not set calculated automatically by BV.
        number_of_chunks: 700,
        /// [optional] Seconds after which presigned urls in generated `UploadManifest` may expire.
        /// If not set calculated automatically by BV.
        url_expires_secs: 240000,
        /// [optional] Version number for uploaded data. Auto-assigned if not provided.
        /// If not set calculated automatically by Blockvisor API.
        data_version: 3,
        /// [optional] Job restart config.
        /// If not set default to:
        /// #{
        ///    backoff_timeout_ms: 600000,
        ///    backoff_base_ms: 500,
        ///    max_retries: 10,
        /// }
        restart_config: #{
            backoff_timeout_ms: 60000,
            backoff_base_ms: 1000,
            max_retries: 5,
        },
    },
    /// [optional] post-upload action
    post_upload: #{
        /// list of sh commands to be executed after upload
        commands: [
            `echo uploaded`,
        ],
        /// list of sh jobs (long running tasks), to be started after upload
        jobs: [
            #{
                name: "post_upload_job",
                run_sh: `echo cleanup_after_upload`,
            }
        ]
    },
};

/// Returns blockchain application status.
/// SHALL be implemented
fn application_status() {
    /// "uploading", "downloading", "initializing" statuses are automatically handled by BV if BABEL_CONFIG const is defined
    let result = run_jrpc(#{
            host: global::API_HOST,
            method: "health.health",
            headers: [["content-type", "application/json"]]
        }).expect(200);

    if result.healthy != "false" || result.result.checks.bootstrapped.error == "subnets not bootstrapped" {
        "broadcasting"
    } else {
        "delinquent"
    }
}
