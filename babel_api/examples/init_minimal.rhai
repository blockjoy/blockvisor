const NETS = #{
    main: #{
        net_type: "main",
    },
};

fn init() {
  let A_DIR = node_env().protocol_data_path + "/A/";
  let B_DIR = node_env().protocol_data_path + "/B/";
  let NET =  global::NETS[node_env().node_variant];

  run_sh(`mkdir -p ${A_DIR} ${B_DIR} && mkdir -p /opt/netdata/var/cache/netdata && mkdir -p /opt/netdata/var/lib/netdata && rm -rf /opt/netdata/var/lib/netdata/* && rm -rf /opt/netdata/var/cache/netdata/*`).unwrap();
  start_job("protocol_service_a", #{
      job_type: #{
          run_sh: `/usr/bin/protocol_service_a --chain=${NET.net_type} --datadir=${B_DIR} --snapshots=false`,
      },
      restart: #{
          always: #{
              backoff_timeout_ms: 60000,
              backoff_base_ms: 1000,
          },
      },
  });
  start_job("protocol_service_b", #{
      job_type: #{
          run_sh: `/usr/bin/protocol_service_b start --home=${B_DIR} --chain=${NET.net_type} --rest-server --seeds ${NET.seeds} "$@"`,
      },
      restart: #{
          always: #{
              backoff_timeout_ms: 60000,
              backoff_base_ms: 1000,
          },
      },
  });
}
