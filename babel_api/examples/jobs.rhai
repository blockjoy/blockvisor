fn init() {
    let job_config_A = #{
        job_type: #{
            // Sh script body. It shall be blocking (foreground) process.
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

        /// [optional] Run job as a different user.
        run_as: "some_user",

        /// [optional] Capacity of log buffer (in megabytes).
        /// If not set default to 128.
        log_buffer_capacity_mb: 256,

        /// [optional] Prepend timestamp to each log, or not.
        /// If not set default to false.
        log_timestamp: true,

        /// [optional] Indicate if job should 'lock' protocol data.
        /// Lock prevents re-initialization of the data after job is started.
        /// If not set default to false.
        protocol_data_lock: true,
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

    let service_config = #{
        job_type: #{
            // Sh script body
            run_sh: "echo \"Protocol service parametrized with " + node_params()["TESTING"] + "\"",
        },

        // Job restart policy.
        restart: #{
            // "always" key means that job is always restarted - equivalent to always running service.
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
        needs: ["job_name_A"],
        /// [optional] List of job names that this job needs to wait for, but job will be started
        /// whatever waited jobs result is.
        wait_for: ["job_name_B"],
    };
    start_job("job_name_A");
    start_job("unique_service_name", service_config);

    let upload_job_config = #{
        job_type: #{
            upload: #{
                // [optional] List of exclude patterns. Files in `node_env().protocol_data_path` directory that match any of pattern,
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
                // [optional] Number of chunks that protocol data should be split into.
                // Recommended chunk size is about 500MB. Estimated by BV based on data size, if not provided.
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
}
