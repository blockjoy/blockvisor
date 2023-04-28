use crate::metadata::check_metadata;
use crate::{
    engine::Engine,
    metadata::BlockchainMetadata,
    plugin::{ApplicationStatus, Plugin, StakingStatus, SyncStatus},
};
use anyhow::{anyhow, Context, Error, Result};
use rhai;
use rhai::{
    serde::{from_dynamic, to_dynamic},
    Dynamic, AST,
};
use std::time::Duration;
use std::{collections::HashMap, path::Path, sync::Arc};

#[derive(Debug)]
pub struct RhaiPlugin<E> {
    babel_engine: Arc<E>,
    ast: AST,
    rhai_engine: rhai::Engine,
}

impl<E: Engine + Sync + Send + 'static> Clone for RhaiPlugin<E> {
    fn clone(&self) -> Self {
        let mut clone = Self {
            babel_engine: self.babel_engine.clone(),
            rhai_engine: rhai::Engine::new(),
            ast: self.ast.clone(),
        };
        clone.init_rhai_engine();
        clone
    }
}

pub fn read_metadata(script: &str) -> Result<BlockchainMetadata> {
    find_metadata_in_ast(&rhai::Engine::new().compile(script)?)
}

fn find_metadata_in_ast(ast: &AST) -> Result<BlockchainMetadata> {
    let mut vars = ast.iter_literal_variables(true, false);
    if let Some((_, _, dynamic)) = vars.find(|(name, _, _)| name == &"METADATA") {
        let meta: BlockchainMetadata = from_dynamic(&dynamic)
            .with_context(|| "Invalid Rhai script - failed to deserialize METADATA")?;
        check_metadata(&meta)?;
        Ok(meta)
    } else {
        Err(anyhow!("Invalid Rhai script - missing METADATA constant"))
    }
}

impl<E: Engine + Sync + Send + 'static> RhaiPlugin<E> {
    pub fn new(script: &str, babel_engine: E) -> Result<Self> {
        let rhai_engine = rhai::Engine::new();
        // compile script to AST
        let ast = rhai_engine.compile(script)?;
        let mut plugin = RhaiPlugin {
            babel_engine: Arc::new(babel_engine),
            rhai_engine,
            ast,
        };
        plugin.init_rhai_engine();
        Ok(plugin)
    }

    /// register all Babel engine methods
    fn init_rhai_engine(&mut self) {
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("start_job", move |job_name: &str, job_config: Dynamic| {
                into_rhai_result(babel_engine.start_job(job_name, from_dynamic(&job_config)?))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("stop_job", move |job_name: &str| {
                into_rhai_result(babel_engine.stop_job(job_name))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("job_status", move |job_name: &str| {
                to_dynamic(into_rhai_result(babel_engine.job_status(job_name))?)
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_jrpc", move |host: &str, method: &str, timeout: i64| {
                let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
                into_rhai_result(babel_engine.run_jrpc(
                    host,
                    method,
                    Some(Duration::from_secs(timeout)),
                ))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_jrpc", move |host: &str, method: &str| {
                into_rhai_result(babel_engine.run_jrpc(host, method, None))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_rest", move |url: &str, timeout: i64| {
                let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
                into_rhai_result(babel_engine.run_rest(url, Some(Duration::from_secs(timeout))))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine.register_fn("run_rest", move |url: &str| {
            into_rhai_result(babel_engine.run_rest(url, None))
        });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_sh", move |body: &str, timeout: i64| {
                let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
                into_rhai_result(babel_engine.run_sh(body, Some(Duration::from_secs(timeout))))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine.register_fn("run_sh", move |body: &str| {
            into_rhai_result(babel_engine.run_sh(body, None))
        });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("sanitize_sh_param", move |param: &str| {
                into_rhai_result(babel_engine.sanitize_sh_param(param))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine.register_fn(
            "render_template",
            move |template: &str, output: &str, params: &str| {
                into_rhai_result(babel_engine.render_template(
                    Path::new(&template.to_string()),
                    Path::new(&output.to_string()),
                    params,
                ))
            },
        );
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine.register_fn("node_params", move || {
            rhai::Map::from_iter(
                babel_engine
                    .node_params()
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into())),
            )
        });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine
            .register_fn("save_data", move |value: &str| {
                into_rhai_result(babel_engine.save_data(value))
            });
        let babel_engine = self.babel_engine.clone();
        self.rhai_engine.register_fn("load_data", move || {
            into_rhai_result(babel_engine.load_data())
        });

        // register other utils
        self.rhai_engine
            .register_fn("parse_json", move |json: &str| {
                rhai::Engine::new().parse_json(json, true)
            });
    }

    fn call_fn<P: rhai::FuncArgs, R: Clone + Send + Sync + 'static>(
        &self,
        name: &str,
        args: P,
    ) -> Result<R> {
        let mut scope = rhai::Scope::new();
        Ok(self
            .rhai_engine
            .call_fn::<R>(&mut scope, &self.ast, name, args)?)
    }
}

impl<E: Engine + Sync + Send + 'static> Plugin for RhaiPlugin<E> {
    fn metadata(&self) -> Result<BlockchainMetadata> {
        find_metadata_in_ast(&self.ast)
    }

    fn capabilities(&self) -> Vec<String> {
        self.ast
            .iter_functions()
            .map(|meta| meta.name.to_string())
            .collect()
    }

    fn has_capability(&self, name: &str) -> bool {
        self.ast.iter_functions().any(|meta| meta.name == name)
    }

    fn init(&self, secret_keys: &HashMap<String, String>) -> Result<()> {
        let secret_keys =
            rhai::Map::from_iter(secret_keys.iter().map(|(k, v)| (k.into(), v.into())));
        self.call_fn("init", (secret_keys,))
    }

    fn height(&self) -> Result<u64> {
        Ok(self.call_fn::<_, i64>("height", ())?.try_into()?)
    }

    fn block_age(&self) -> Result<u64> {
        Ok(self.call_fn::<_, i64>("block_age", ())?.try_into()?)
    }

    fn name(&self) -> Result<String> {
        self.call_fn("name", ())
    }

    fn address(&self) -> Result<String> {
        self.call_fn("address", ())
    }

    fn consensus(&self) -> Result<bool> {
        self.call_fn("consensus", ())
    }

    fn application_status(&self) -> Result<ApplicationStatus> {
        Ok(from_dynamic(
            &self.call_fn::<_, Dynamic>("application_status", ())?,
        )?)
    }

    fn sync_status(&self) -> Result<SyncStatus> {
        Ok(from_dynamic(
            &self.call_fn::<_, Dynamic>("sync_status", ())?,
        )?)
    }

    fn staking_status(&self) -> Result<StakingStatus> {
        Ok(from_dynamic(
            &self.call_fn::<_, Dynamic>("staking_status", ())?,
        )?)
    }

    fn generate_keys(&self) -> Result<()> {
        self.call_fn::<_, ()>("generate_keys", ())
    }

    fn call_custom_method(&self, name: &str, param: &str) -> Result<String> {
        self.call_fn(name, (param.to_string(),))
    }
}

fn into_rhai_result<T>(result: Result<T>) -> std::result::Result<T, Box<rhai::EvalAltResult>> {
    Ok(result.map_err(|err| <String as Into<rhai::EvalAltResult>>::into(err.to_string()))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{JobConfig, JobStatus, RestartConfig, RestartPolicy};
    use crate::metadata::{firewall, BabelConfig, NetConfiguration, NetType, Requirements};
    use anyhow::bail;
    use mockall::*;

    mock! {
        pub BabelEngine {}

        impl Engine for BabelEngine {
            fn start_job(&self, job_name: &str, job_config: JobConfig) -> Result<()>;
            fn stop_job(&self, job_name: &str) -> Result<()>;
            fn job_status(&self, job_name: &str) -> Result<JobStatus>;
            fn run_jrpc(&self, host: &str, method: &str, timeout: Option<Duration>) -> Result<String>;
            fn run_rest(&self, url: &str, timeout: Option<Duration>) -> Result<String>;
            fn run_sh(&self, body: &str, timeout: Option<Duration>) -> Result<String>;
            fn sanitize_sh_param(&self, param: &str) -> Result<String>;
            fn render_template(
                &self,
                template: &Path,
                output: &Path,
                params: &str,
            ) -> Result<()>;
            fn node_params(&self) -> HashMap<String, String>;
            fn save_data(&self, value: &str) -> Result<()>;
            fn load_data(&self) -> Result<String>;
        }
    }

    #[test]
    fn test_capabilities() -> Result<()> {
        let script = r#"
        fn function_A(any_param) {
        }
        
        fn function_b(more, params) {
        }
        
        fn functionC() {
            // no params
        }
"#;
        let plugin = RhaiPlugin::new(script, MockBabelEngine::new())?;
        let mut expected_capabilities = vec![
            "function_A".to_string(),
            "function_b".to_string(),
            "functionC".to_string(),
        ];
        expected_capabilities.sort();
        let mut capabilities = plugin.capabilities();
        capabilities.sort();
        assert_eq!(expected_capabilities, capabilities);
        assert!(!plugin.has_capability("function_a"));
        assert!(plugin.has_capability("function_b"));
        Ok(())
    }

    #[test]
    fn test_call_build_in_functions() -> Result<()> {
        let script = r#"
        fn init(keys) {
            save_data(keys.key1.to_string());
        }
        
        fn height() {
            77
        }
        
        fn block_age() {
            18
        }
        
        fn name() {
            "block name"
        }

        fn address() {
            "node address"
        }

        fn consensus() {
            true
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
            save_data("keys generated");
        }
"#;
        let mut babel = MockBabelEngine::new();
        babel
            .expect_save_data()
            .with(predicate::eq("key1_value"))
            .return_once(|_| Ok(()));
        babel
            .expect_save_data()
            .with(predicate::eq("keys generated"))
            .return_once(|_| Ok(()));
        let plugin = RhaiPlugin::new(script, babel)?.clone(); // call clone() to make sure it works as well
        plugin.init(&HashMap::from_iter([(
            "key1".to_string(),
            "key1_value".to_string(),
        )]))?;
        assert_eq!(77, plugin.height()?);
        assert_eq!(18, plugin.block_age()?);
        assert_eq!("block name", &plugin.name()?);
        assert_eq!("node address", &plugin.address()?);
        assert!(plugin.consensus()?);
        assert_eq!(
            ApplicationStatus::Broadcasting,
            plugin.application_status()?
        );
        assert_eq!(SyncStatus::Synced, plugin.sync_status()?);
        assert_eq!(StakingStatus::Staking, plugin.staking_status()?);
        plugin.generate_keys()?;
        Ok(())
    }

    #[test]
    fn test_call_custom_method_call_engine() -> Result<()> {
        let script = r#"
    fn custom_method(param) {
        let out = parse_json(param).a;
        start_job("test_job_name", #{
            body: "job body",
            restart: #{
                "on_failure": #{
                    max_retries: 3,
                    backoff_timeout_ms: 1000,
                    backoff_base_ms: 500,
                },
            },
            needs: ["needed"],
        });
        stop_job("test_job_name");
        out += "|" + job_status("test_job_name");
        out += "|" + run_jrpc("host", "method");
        out += "|" + run_jrpc("host", "method", 1);
        out += "|" + run_rest("url");
        out += "|" + run_rest("url", 2);
        out += "|" + run_sh("body");
        out += "|" + run_sh("body", 3);
        out += "|" + sanitize_sh_param("sh param");
        render_template("/template/path", "output/path.cfg", #{ PARAM1: "Value I"}.to_json());
        out += "|" + node_params().to_json().to_string(); 
        save_data("some plugin data"); 
        out += "|" + load_data(); 
        out
    }
"#;
        let mut babel = MockBabelEngine::new();
        babel
            .expect_start_job()
            .with(
                predicate::eq("test_job_name"),
                predicate::eq(JobConfig {
                    body: "job body".to_string(),
                    restart: RestartPolicy::OnFailure(RestartConfig {
                        backoff_timeout_ms: 1000,
                        backoff_base_ms: 500,
                        max_retries: Some(3),
                    }),
                    needs: Some(vec!["needed".to_string()]),
                }),
            )
            .return_once(|_, _| Ok(()));
        babel
            .expect_stop_job()
            .with(predicate::eq("test_job_name"))
            .return_once(|_| Ok(()));
        babel
            .expect_job_status()
            .with(predicate::eq("test_job_name"))
            .return_once(|_| {
                Ok(JobStatus::Finished {
                    exit_code: Some(1),
                    message: "error msg".to_string(),
                })
            });
        babel
            .expect_run_jrpc()
            .with(
                predicate::eq("host"),
                predicate::eq("method"),
                predicate::eq(None),
            )
            .return_once(|_, _, _| Ok("jrpc_response".to_string()));
        babel
            .expect_run_jrpc()
            .with(
                predicate::eq("host"),
                predicate::eq("method"),
                predicate::eq(Some(Duration::from_secs(1))),
            )
            .return_once(|_, _, _| Ok("jrpc_with_timeout_response".to_string()));
        babel
            .expect_run_rest()
            .with(predicate::eq("url"), predicate::eq(None))
            .return_once(|_, _| Ok("rest_response".to_string()));
        babel
            .expect_run_rest()
            .with(
                predicate::eq("url"),
                predicate::eq(Some(Duration::from_secs(2))),
            )
            .return_once(|_, _| Ok("rest_with_timeout_response".to_string()));
        babel
            .expect_run_sh()
            .with(predicate::eq("body"), predicate::eq(None))
            .return_once(|_, _| Ok("sh_response".to_string()));
        babel
            .expect_run_sh()
            .with(
                predicate::eq("body"),
                predicate::eq(Some(Duration::from_secs(3))),
            )
            .return_once(|_, _| Ok("sh_with_timeout_response".to_string()));
        babel
            .expect_sanitize_sh_param()
            .with(predicate::eq("sh param"))
            .return_once(|_| Ok("sh_sanitized".to_string()));
        babel
            .expect_render_template()
            .with(
                predicate::eq(Path::new("/template/path")),
                predicate::eq(Path::new("output/path.cfg")),
                predicate::eq(r#"{"PARAM1":"Value I"}"#),
            )
            .return_once(|_, _, _| Ok(()));
        babel
            .expect_node_params()
            .return_once(|| HashMap::from_iter([("key_A".to_string(), "value_A".to_string())]));
        babel
            .expect_save_data()
            .with(predicate::eq("some plugin data"))
            .return_once(|_| Ok(()));
        babel
            .expect_load_data()
            .return_once(|| Ok("loaded data".to_string()));

        let plugin = RhaiPlugin::new(script, babel)?;
        assert_eq!(
            r#"json_as_param|#{"finished": #{"exit_code": 1, "message": "error msg"}}|jrpc_response|jrpc_with_timeout_response|rest_response|rest_with_timeout_response|sh_response|sh_with_timeout_response|sh_sanitized|{"key_A":"value_A"}|loaded data"#,
            plugin.call_custom_method("custom_method", r#"{"a":"json_as_param"}"#)?
        );
        Ok(())
    }

    #[test]
    fn test_errors_handling() -> Result<()> {
        let script = r#"
        const METADATA = #{
    // invalid metadata
};

    fn custom_method_with_exception(param) {
        throw "some Rhai exception";
        ""
    }
    
    fn custom_failing_method(param) {
        load_data()
    }
"#;
        let mut babel = MockBabelEngine::new();
        babel
            .expect_load_data()
            .return_once(|| bail!("some Rust error"));
        let plugin = RhaiPlugin::new(script, babel)?;
        assert!(plugin
            .call_custom_method("custom_method_with_exception", "")
            .unwrap_err()
            .to_string()
            .contains("some Rhai exception"));
        assert!(plugin
            .call_custom_method("custom_failing_method", "")
            .unwrap_err()
            .to_string()
            .contains("some Rust error"));
        assert!(plugin
            .call_custom_method("no_method", "")
            .unwrap_err()
            .to_string()
            .contains("Function not found: no_method"));
        assert!(plugin
            .metadata()
            .unwrap_err()
            .to_string()
            .contains("Invalid Rhai script - failed to deserialize METADATA"));
        Ok(())
    }

    #[test]
    fn test_metadata() -> Result<()> {
        let meta_rhai = r#"
const METADATA = #{
    // comments are allowed
    min_babel_version: "0.0.9",
    node_version: "node_v",
    protocol: "proto",
    node_type: "n_type",
    description: "node description",
    requirements: #{
        vcpu_count: 2,
        mem_size_mb: 1024,
        disk_size_gb: 10,
    },
    nets: #{
        test: #{
            url: "test_url",
            net_type: "test",
            some_custom: "metadata",
        },
        main: #{
            url: "main_url",
            net_type: "main",
        },
    },
    babel_config: #{
        data_directory_mount_point: "/mnt/data/",
        log_buffer_capacity_ln: 1024,
        swap_size_mb: 1024,
    },
    firewall: #{
        enabled: true,
        default_in: "deny",
        default_out: "allow",
        rules: [
            #{
                name: "Rule A",
                action: "allow",
                direction: "in",
                protocol: "tcp",
                ips: "192.168.0.1/24",
                ports: [77, 1444, 8080],
            },
            #{
                name: "Rule B",
                action: "deny",
                direction: "out",
                protocol: "udp",
                ips: "192.167.0.1/24",
                ports: [77],
            },
            #{
                name: "Rule C",
                action: "reject",
                direction: "out",
                ips: "192.169.0.1/24",
                ports: [],
            },
        ],
    },
    keys: #{
        key_a_name: "key A Value",
        key_B_name: "key B Value",
        key_X_name: "X",
        "*": "/*"
    },
};
fn any_function() {}
"#;
        let meta_rust = BlockchainMetadata {
            node_version: "node_v".to_string(),
            protocol: "proto".to_string(),
            node_type: "n_type".to_string(),
            description: Some("node description".to_string()),
            requirements: Requirements {
                vcpu_count: 2,
                mem_size_mb: 1024,
                disk_size_gb: 10,
            },
            nets: HashMap::from_iter([
                (
                    "test".to_string(),
                    NetConfiguration {
                        url: "test_url".to_string(),
                        net_type: NetType::Test,
                        meta: HashMap::from_iter([(
                            "some_custom".to_string(),
                            "metadata".to_string(),
                        )]),
                    },
                ),
                (
                    "main".to_string(),
                    NetConfiguration {
                        url: "main_url".to_string(),
                        net_type: NetType::Main,
                        meta: Default::default(),
                    },
                ),
            ]),
            min_babel_version: "0.0.9".to_string(),
            babel_config: BabelConfig {
                data_directory_mount_point: "/mnt/data/".to_string(),
                log_buffer_capacity_ln: 1024,
                swap_size_mb: 1024,
            },
            firewall: firewall::Config {
                enabled: true,
                default_in: firewall::Action::Deny,
                default_out: firewall::Action::Allow,
                rules: vec![
                    firewall::Rule {
                        name: "Rule A".to_string(),
                        action: firewall::Action::Allow,
                        direction: firewall::Direction::In,
                        protocol: Some(firewall::Protocol::Tcp),
                        ips: Some("192.168.0.1/24".to_string()),
                        ports: vec![77, 1444, 8080],
                    },
                    firewall::Rule {
                        name: "Rule B".to_string(),
                        action: firewall::Action::Deny,
                        direction: firewall::Direction::Out,
                        protocol: Some(firewall::Protocol::Udp),
                        ips: Some("192.167.0.1/24".to_string()),
                        ports: vec![77],
                    },
                    firewall::Rule {
                        name: "Rule C".to_string(),
                        action: firewall::Action::Reject,
                        direction: firewall::Direction::Out,
                        protocol: None,
                        ips: Some("192.169.0.1/24".to_string()),
                        ports: vec![],
                    },
                ],
            },
            keys: Some(HashMap::from_iter([
                ("key_X_name".to_string(), "X".to_string()),
                ("*".to_string(), "/*".to_string()),
                ("key_a_name".to_string(), "key A Value".to_string()),
                ("key_B_name".to_string(), "key B Value".to_string()),
            ])),
        };
        let plugin = RhaiPlugin::new(meta_rhai, MockBabelEngine::new())?;
        assert_eq!(meta_rust, plugin.metadata()?);
        Ok(())
    }
}
