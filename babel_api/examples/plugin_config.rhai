import "base" as base;

/// You can define some constants to use in the code
const NETS = #{
    test: #{
        url: "https://testnet-api.helium.wtf/v1/",
        net_type: "test",
        seeds: "test seed"
    },
    main: #{
        url: "https://rpc.ankr.com/eth",
        net_type: "main",
        seeds: "main seed"
    },
};

const API_HOST = "http://localhost:4467/";
const A_DIR = node_env().protocol_data_path + "/A";
const B_DIR = node_env().protocol_data_path + "/B";
const NET =  global::NETS[node_env().node_variant];

const BASE_CONFIG = base::BASE_CONFIG;
const PLUGIN_CONFIG = #{
    /// [optional] List of configuration files to be rendered from template with provided params.
    /// See [Tera Docs](https://keats.github.io/tera/docs/#templates) for details on templating syntax.
    config_files: [
        #{
            /// It assumes that file pointed by `template` argument exists.
            template: "/var/lib/babel/templates/config.template",
            /// File pointed by `destination` path will be overwritten if exists.
            destination: "/etc/service.config",
            /// Map object serializable to JSON.
            params: #{
                node_name: node_env().node_name
            },
        },
    ],
    /// Node init actions.
    init: #{
        /// List of sh commands to be executed first.
        commands: [
            `mkdir -p /opt/netdata/var/cache/netdata`,
        ],
        /// List of sh jobs (long running tasks), to be started then.
        jobs: [
            #{
                /// Unique job name.
                name: "init_job",
                /// Sh script body. It shall be blocking (foreground) process.
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
                /// [optional] Capacity of log buffer (in megabytes).
                /// If not set default to 128.
                log_buffer_capacity_mb: 32,
                /// [optional] Prepend timestamp to each log, or not.
                /// If not set default to true.
                log_timestamp: false,
            }
        ]
    },
    /// List of services to be started once init jobs and download is finished.
    services: [
        #{
            /// Unique service job name.
            name: "protocol_service_a",
            /// Sh script body. It shall be blocking (foreground) process.
            run_sh: `/usr/bin/protocol_service_a start --home=${global::A_DIR} --chain=${global::NET.net_type} --rest-server --seeds ${global::NET.seeds} "$@"`,
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
            /// [optional] Flag indicating if service uses protocol data.
            /// Services that uses protocol data won't be started until all init steps and download
            /// is finished. They are also stopped while uploading protocol archive.
            /// If not set default to true.
            use_protocol_data: true,
            /// [optional] Capacity of log buffer (in megabytes).
            /// If not set default to 128.
            log_buffer_capacity_mb: 256,
            /// [optional] Prepend timestamp to each log, or not.
            /// If not set default to true.
            log_timestamp: false,
        },
        #{
            name: "protocol_service_b",
            run_sh: `/usr/bin/protocol_service_b --chain=${global::NET.net_type} --datadir=${global::A_DIR} --snapshots=false`,
        }
    ],
    /// [optional] Download configuration.
    /// Built-in download will be started once all init jobs are finished.
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
    /// [optional] Alternative download if archive for standard one is not available.
    alternative_download: #{
        /// Sh script body. It shall be blocking (foreground) process.
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
        /// [optional] Capacity of log buffer (in megabytes).
        /// If not set default to 128.
        log_buffer_capacity_mb: 64,
        /// [optional] Prepend timestamp to each log, or not.
        /// If not set default to true.
        log_timestamp: false,
    },
    /// [optional] Post-download sh jobs, to be started after download.
    post_download: [
        #{
            name: "post_download_job",
            run_sh: `echo restoreDB`,
        }
    ],
    /// [optional] Pre-upload action.
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
    /// [optional] Upload configuration.
    /// Built-in upload can be manually triggered with `bv node run upload`.
    upload: #{
        /// [optional] List of exclude patterns. Files in `node_env().protocol_data_path` directory that match any of pattern,
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
        /// [optional] Number of chunks that protocol data should be split into.
        /// Recommended chunk size is about 500MB. Estimated by BV based on data size, if not provided.
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
    /// [optional] Post-upload sh jobs, to be started after upload.
    post_upload: [
        #{
            name: "post_upload_job",
            run_sh: `echo cleanup_after_upload`,
        }
    ],
    /// [optional] List of tasks to be scheduled on init.
    scheduled: [
        #{
            /// Unique name of the task.
            name: "some_task",
            /// Cron schedule expression.
            schedule: "* * * * * * *",
            /// Function name to be executed according to schedule.
            function: "fn_name",
            /// [optional] Parameter to ba passed to function.
            param: "param_value",
        }
    ],
    /// [optional] Set to true, to disable all default services.
    /// Default to false.
    disable_default_services: true,
};

/// Returns protocol application status.
/// SHALL be implemented
fn protocol_status() {
    /// "uploading", "downloading", "starting" statuses are automatically handled by BV if BABEL_CONFIG const is defined
    let result = run_jrpc(#{
            host: global::API_HOST,
            method: "health.health",
            headers: [["content-type", "application/json"]]
        }).expect(200);

    if result.healthy != "false" || result.result.checks.bootstrapped.error == "subnets not bootstrapped" {
        #{state: "broadcasting", health: "healthy"}
    } else {
        #{state: "delinquent", health: "healthy"}
    }
}
