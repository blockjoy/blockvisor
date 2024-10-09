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

fn application_status() {
    #{state: "broadcasting", health: "healthy"}
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

fn config_check(param) {
    try {
        let config = get_config("config_key");
    } catch (err) {
        if err == "not_found" {
            put_config("config_key", "config-value".to_blob());
            let config = get_config("config_key").as_string();
            return if config == "config-value" {"ok"} else {"got unexpected config: $config"}
        }
    }
    throw "first get_config should fail"
}

fn file_access_check(param) {
    file_write("/blockjoy/secret.file", "some secret".to_blob());
    let secret = file_read("/blockjoy/secret.file").as_string();
    let content = run_sh("cat /blockjoy/secret.file").unwrap();
    return if secret == "some secret" && secret == content {"ok"} else {"got unexpected file content: $secret"}
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

fn info(param) {
    param
}
