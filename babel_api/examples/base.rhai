export const BASE_CONFIG = #{
    /// [optional] List of configuration files to be rendered from template with provided params.
    /// See [Tera Docs](https://keats.github.io/tera/docs/#templates) for details on templating syntax.
    config_files: [
        #{
            /// It assumes that file pointed by `template` argument exists.
            template: "/var/lib/babel/templates/config.template",
            /// File pointed by `destination` path will be overwritten if exists.
            destination: "/etc/base_service.config",
            /// Map object serializable to JSON.
            params: #{
                node_name: node_env().node_name
            },
        },
    ],
    /// [optional] List of default services to be run in background.
    services: [
         #{
             /// Unique service job name.
             name: "default_service_a",
             /// Sh script body. It shall be blocking (foreground) process.
             run_sh: `monitoring_agent`,
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
             /// [optional] Capacity of log buffer (in megabytes).
             /// If not set default to 128.
             log_buffer_capacity_mb: 256,
             /// [optional] Prepend timestamp to each log, or not.
             /// If not set default to true.
             log_timestamp: false,
         },
         #{
             name: "default_service_b",
             run_sh: `proxy`,
         }
    ],
};
