use crate::{
    engine::{
        self, Engine, HttpResponse, JobConfig, JobStatus, JrpcRequest, RestRequest, ShResponse,
    },
    metadata::{check_metadata, BlockchainMetadata},
    plugin::{ApplicationStatus, Plugin, StakingStatus, SyncStatus},
    plugin_config::{self, Actions, DefaultService, Job, PluginConfig, Service},
};
use eyre::{anyhow, bail, Context, Error, Report, Result};
use rhai::{
    self,
    serde::{from_dynamic, to_dynamic},
    Dynamic, FnPtr, Map, AST,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{path::Path, sync::Arc, time::Duration};
use tracing::Level;

const DOWNLOAD_JOB_NAME: &str = "download";
const UPLOAD_JOB_NAME: &str = "upload";
const INIT_FN_NAME: &str = "init";
const PLUGIN_CONFIG_CONST_NAME: &str = "PLUGIN_CONFIG";
const DEFAULT_SERVICES_CONST_NAME: &str = "DEFAULT_SERVICES";
const BABEL_VERSION_CONST_NAME: &str = "BABEL_VERSION";

#[derive(Debug)]
pub struct RhaiPlugin<E> {
    bare: BarePlugin<E>,
    rhai_engine: rhai::Engine,
}

impl<E: Engine + Sync + Send + 'static> Clone for RhaiPlugin<E> {
    fn clone(&self) -> Self {
        let mut clone = Self {
            bare: self.bare.clone(),
            rhai_engine: rhai::Engine::new(),
        };
        clone.init_rhai_engine();
        clone
    }
}

#[derive(Debug)]
struct BarePlugin<E> {
    babel_engine: Arc<E>,
    ast: AST,
}

impl<E: Engine + Sync + Send + 'static> Clone for BarePlugin<E> {
    fn clone(&self) -> Self {
        Self {
            babel_engine: self.babel_engine.clone(),
            ast: self.ast.clone(),
        }
    }
}

pub fn read_metadata(script: &str) -> Result<BlockchainMetadata> {
    find_metadata_in_ast(
        &new_rhai_engine()
            .compile(script)
            .with_context(|| "Rhai syntax error")?,
    )
}

fn new_rhai_engine() -> rhai::Engine {
    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(64, 32);
    engine
}

fn find_metadata_in_ast(ast: &AST) -> Result<BlockchainMetadata> {
    let meta = find_const_in_ast(ast, "METADATA")?;
    check_metadata(&meta)?;
    Ok(meta)
}

fn build_scope() -> rhai::Scope<'static> {
    let mut scope = rhai::Scope::new();
    scope.push("DATA_DRIVE_MOUNT_POINT", engine::DATA_DRIVE_MOUNT_POINT);
    scope.push(
        "BLOCKCHAIN_DATA_PATH",
        engine::BLOCKCHAIN_DATA_PATH.to_string_lossy().to_string(),
    );
    scope
}

fn find_const_in_ast<T: for<'a> Deserialize<'a>>(ast: &AST, name: &str) -> Result<T> {
    let mut vars = ast.iter_literal_variables(true, true);
    if let Some((_, _, dynamic)) = vars.find(|(v, _, _)| *v == name) {
        let value: T = from_dynamic(&dynamic)
            .with_context(|| format!("Invalid Rhai script - failed to deserialize {name}"))?;
        Ok(value)
    } else {
        Err(anyhow!("Invalid Rhai script - missing {name} constant"))
    }
}

impl<E: Engine + Sync + Send + 'static> RhaiPlugin<E> {
    pub fn new(script: &str, babel_engine: E) -> Result<Self> {
        let rhai_engine = new_rhai_engine();
        // compile script to AST
        let ast = rhai_engine
            .compile(script)
            .with_context(|| "Rhai syntax error")?;
        let mut plugin = RhaiPlugin {
            bare: BarePlugin {
                babel_engine: Arc::new(babel_engine),
                ast,
            },
            rhai_engine,
        };
        plugin.check_babel_version()?;
        plugin.init_rhai_engine();
        Ok(plugin)
    }

    fn check_babel_version(&self) -> Result<()> {
        if let Ok(min_babel_version) =
            find_const_in_ast::<String>(&self.bare.ast, BABEL_VERSION_CONST_NAME)
        {
            let min_babel_version = semver::Version::parse(&min_babel_version)?;
            let version = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
            if version < min_babel_version {
                bail!(
                "Required minimum babel version is `{min_babel_version}`, running is `{version}`"
            );
            }
        }
        Ok(())
    }

    /// register all Babel engine methods
    fn init_rhai_engine(&mut self) {
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("create_job", move |job_name: &str, job_config: Dynamic| {
                into_rhai_result(babel_engine.create_job(job_name, from_dynamic(&job_config)?))
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("start_job", move |job_name: &str| {
                into_rhai_result(babel_engine.start_job(job_name))
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("start_job", move |job_name: &str, job_config: Dynamic| {
                into_rhai_result(
                    babel_engine
                        .create_job(job_name, from_dynamic(&job_config)?)
                        .and_then(|_| babel_engine.start_job(job_name)),
                )
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("stop_job", move |job_name: &str| {
                into_rhai_result(babel_engine.stop_job(job_name))
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("job_info", move |job_name: &str| {
                to_dynamic(into_rhai_result(babel_engine.job_info(job_name))?)
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn("get_jobs", move || {
            to_dynamic(into_rhai_result(babel_engine.get_jobs())?)
        });
        self.rhai_engine
            .register_type_with_name::<DressedHttpResponse>("DressedHttpResponse")
            .register_get("body", DressedHttpResponse::get_body)
            .register_get("status_code", DressedHttpResponse::get_status_code)
            .register_fn("expect", DressedHttpResponse::expect)
            .register_fn("expect", DressedHttpResponse::expect_with);
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_jrpc", move |req: Dynamic, timeout: i64| {
                let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
                let req = into_rhai_result(from_dynamic::<BareJrpcRequest>(&req)?.try_into())?;
                into_rhai_result(
                    babel_engine
                        .run_jrpc(req, Some(Duration::from_secs(timeout)))
                        .map(DressedHttpResponse::from),
                )
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_jrpc", move |req: Dynamic| {
                let req = into_rhai_result(from_dynamic::<BareJrpcRequest>(&req)?.try_into())?;
                into_rhai_result(
                    babel_engine
                        .run_jrpc(req, None)
                        .map(DressedHttpResponse::from),
                )
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_rest", move |req: Dynamic, timeout: i64| {
                let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
                let req = into_rhai_result(from_dynamic::<BareRestRequest>(&req)?.try_into())?;
                into_rhai_result(
                    babel_engine
                        .run_rest(req, Some(Duration::from_secs(timeout)))
                        .map(DressedHttpResponse::from),
                )
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_rest", move |req: Dynamic| {
                let req = into_rhai_result(from_dynamic::<BareRestRequest>(&req)?.try_into())?;
                into_rhai_result(
                    babel_engine
                        .run_rest(req, None)
                        .map(DressedHttpResponse::from),
                )
            });
        self.rhai_engine
            .register_type_with_name::<DressedShResponse>("DressedShResponse")
            .register_get("exit_code", DressedShResponse::get_exit_code)
            .register_get("stdout", DressedShResponse::get_stdout)
            .register_get("stderr", DressedShResponse::get_stderr)
            .register_fn("unwrap", DressedShResponse::unwrap);
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("run_sh", move |body: &str, timeout: i64| {
                let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
                into_rhai_result(
                    babel_engine
                        .run_sh(body, Some(Duration::from_secs(timeout)))
                        .map(DressedShResponse::from),
                )
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn("run_sh", move |body: &str| {
            into_rhai_result(babel_engine.run_sh(body, None).map(DressedShResponse::from))
        });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("sanitize_sh_param", move |param: &str| {
                into_rhai_result(babel_engine.sanitize_sh_param(param))
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn(
            "render_template",
            move |template: &str, destination: &str, params: Map| {
                into_rhai_result(babel_engine.render_template(
                    Path::new(&template.to_string()),
                    Path::new(&destination.to_string()),
                    &rhai::format_map_as_json(&params),
                ))
            },
        );
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn("node_params", move || {
            Map::from_iter(
                babel_engine
                    .node_params()
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into())),
            )
        });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("node_env", move || to_dynamic(babel_engine.node_env()));
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("save_data", move |value: &str| {
                into_rhai_result(babel_engine.save_data(value))
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn("load_data", move || {
            into_rhai_result(babel_engine.load_data())
        });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn(
            "add_task",
            move |task_name: &str, schedule: &str, function_name: &str, function_param: &str| {
                into_rhai_result(babel_engine.add_task(
                    task_name,
                    schedule,
                    function_name,
                    function_param,
                ))
            },
        );
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn(
            "add_task",
            move |task_name: &str, schedule: &str, function_name: &str| {
                into_rhai_result(babel_engine.add_task(task_name, schedule, function_name, ""))
            },
        );
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine
            .register_fn("delete_task", move |task_name: &str| {
                into_rhai_result(babel_engine.delete_task(task_name))
            });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.on_debug(move |msg, _, _| {
            babel_engine.log(Level::DEBUG, msg);
        });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn("info", move |msg: &str| {
            babel_engine.log(Level::INFO, msg);
        });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn("warn", move |msg: &str| {
            babel_engine.log(Level::WARN, msg);
        });
        let babel_engine = self.bare.babel_engine.clone();
        self.rhai_engine.register_fn("error", move |msg: &str| {
            babel_engine.log(Level::ERROR, msg);
        });

        // register other utils
        self.rhai_engine.register_fn("parse_json", |json: &str| {
            rhai::Engine::new().parse_json(json, true)
        });
        self.rhai_engine.register_fn("parse_hex", |hex: &str| {
            into_rhai_result(
                i64::from_str_radix(hex.strip_prefix("0x").unwrap_or(hex), 16).map_err(Report::new),
            )
        });
        self.rhai_engine.register_fn("system_time", || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .map_err(|err| format!("{err:#}"))
        });

        let plugin = self.bare.clone();
        let config = self
            .evaluate_var(PLUGIN_CONFIG_CONST_NAME)
            .map_err(|err| format!("{err:#}"));
        let default_services = self
            .evaluate_optional_var(DEFAULT_SERVICES_CONST_NAME)
            .map_err(|err| format!("{err:#}"));
        self.rhai_engine.register_fn("default_init", move || {
            let config = config
                .clone()
                .map_err(<String as Into<rhai::EvalAltResult>>::into)?;
            let default_services = default_services
                .clone()
                .map_err(<String as Into<rhai::EvalAltResult>>::into)?
                .unwrap_or_default();
            into_rhai_result(plugin.default_init(config, default_services))
        });
    }

    fn call_fn<P: rhai::FuncArgs, R: Clone + Send + Sync + 'static>(
        &self,
        name: &str,
        args: P,
    ) -> Result<R> {
        let mut scope = build_scope();
        self.rhai_engine
            .call_fn::<R>(&mut scope, &self.bare.ast, name, args)
            .with_context(|| format!("Rhai function '{name}' returned error"))
    }

    fn evaluate_var<T: for<'a> Deserialize<'a>>(&self, name: &str) -> Result<T> {
        let mut scope = build_scope();
        self.rhai_engine
            .run_ast_with_scope(&mut scope, &self.bare.ast)?;
        if let Some(dynamic) = scope.get(name) {
            let value: T = from_dynamic(dynamic)
                .with_context(|| format!("Invalid Rhai script - failed to deserialize {name}"))?;
            Ok(value)
        } else {
            Err(anyhow!("Invalid Rhai script - missing {name} constant"))
        }
    }

    fn evaluate_optional_var<T: for<'a> Deserialize<'a>>(&self, name: &str) -> Result<Option<T>> {
        let mut scope = build_scope();
        self.rhai_engine
            .run_ast_with_scope(&mut scope, &self.bare.ast)?;
        if let Some(dynamic) = scope.get(name) {
            let value: T = from_dynamic(dynamic)
                .with_context(|| format!("Invalid Rhai script - failed to deserialize {name}"))?;
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }
}

impl<E: Engine + Sync + Send + 'static> BarePlugin<E> {
    fn run_actions(&self, actions: Option<Actions>, needs: Vec<String>) -> Result<Vec<String>> {
        if let Some(actions) = actions {
            for command in actions.commands {
                let resp = self.babel_engine.run_sh(&command, None)?;
                if resp.exit_code != 0 {
                    bail!("init command '{command}' failed with exit_code: {resp:?}")
                }
            }
            self.run_jobs(Some(actions.jobs), needs)
        } else {
            Ok(needs)
        }
    }

    fn run_jobs(&self, jobs: Option<Vec<Job>>, needs: Vec<String>) -> Result<Vec<String>> {
        if let Some(jobs) = jobs {
            let mut started_jobs = vec![];
            for mut job in jobs {
                let name = job.name.clone();
                if let Some(job_needs) = job.needs.as_mut() {
                    job_needs.append(&mut needs.clone())
                } else {
                    job.needs = Some(needs.clone());
                }
                self.create_and_start_job(&name, plugin_config::build_job_config(job))?;
                started_jobs.push(name);
            }
            Ok(if started_jobs.is_empty() {
                needs
            } else {
                started_jobs
            })
        } else {
            Ok(needs)
        }
    }

    fn start_default_services(&self, default_services: Vec<DefaultService>) -> Result<()> {
        for service in default_services {
            self.start_default_service(service)?;
        }
        Ok(())
    }

    fn start_default_service(&self, service: DefaultService) -> Result<()> {
        self.create_and_start_job(
            &service.name.clone(),
            plugin_config::build_service_job_config(
                Service {
                    name: service.name,
                    run_sh: service.run_sh,
                    restart_config: service.restart_config,
                    shutdown_timeout_secs: service.shutdown_timeout_secs,
                    shutdown_signal: service.shutdown_signal,
                    run_as: service.run_as,
                    use_blockchain_data: false,
                    log_buffer_capacity_mb: service.log_buffer_capacity_mb,
                },
                vec![],
            ),
        )?;
        Ok(())
    }

    fn start_services(&self, services: Vec<Service>, needs: Vec<String>) -> Result<()> {
        for service in services {
            let name = service.name.clone();
            self.create_and_start_job(
                &name,
                plugin_config::build_service_job_config(service, needs.clone()),
            )?;
        }
        Ok(())
    }

    fn create_and_start_job(&self, name: &str, config: JobConfig) -> Result<()> {
        self.babel_engine
            .create_job(name, config)
            .and_then(|_| self.babel_engine.start_job(name))
    }

    fn default_init(
        &self,
        config: PluginConfig,
        default_services: Vec<DefaultService>,
    ) -> Result<()> {
        if let Some(config_files) = config.config_files {
            for config_file in config_files {
                self.babel_engine.render_template(
                    &config_file.template,
                    &config_file.destination,
                    &serde_json::to_string(&config_file.params)?,
                )?;
            }
        }
        if !config.disable_default_services {
            self.start_default_services(default_services)?;
        }
        let mut services_needs = self.run_actions(config.init, vec![])?;
        if !self.babel_engine.is_download_completed()? {
            if self.babel_engine.has_blockchain_archive()? {
                self.create_and_start_job(
                    DOWNLOAD_JOB_NAME,
                    plugin_config::build_download_job_config(config.download, services_needs),
                )?;
                services_needs =
                    self.run_jobs(config.post_download, vec![DOWNLOAD_JOB_NAME.to_string()])?;
            } else if let Some(alternative_download) = config.alternative_download {
                self.create_and_start_job(
                    DOWNLOAD_JOB_NAME,
                    plugin_config::build_alternative_download_job_config(
                        alternative_download,
                        services_needs,
                    ),
                )?;
                services_needs =
                    self.run_jobs(config.post_download, vec![DOWNLOAD_JOB_NAME.to_string()])?;
            }
        }
        self.start_services(config.services, services_needs)?;
        if let Some(tasks) = config.scheduled {
            for task in tasks {
                self.babel_engine.add_task(
                    &task.name,
                    &task.schedule,
                    &task.function,
                    &task.param.unwrap_or_default(),
                )?;
            }
        }
        Ok(())
    }
}

impl<E: Engine + Sync + Send + 'static> Plugin for RhaiPlugin<E> {
    fn metadata(&self) -> Result<BlockchainMetadata> {
        find_metadata_in_ast(&self.bare.ast)
    }

    fn capabilities(&self) -> Vec<String> {
        let mut capabilities: Vec<_> = self
            .bare
            .ast
            .iter_functions()
            .map(|meta| meta.name.to_string())
            .collect();
        if self
            .evaluate_var::<PluginConfig>(PLUGIN_CONFIG_CONST_NAME)
            .is_ok()
        {
            if !capabilities.contains(&UPLOAD_JOB_NAME.to_string()) {
                capabilities.push(UPLOAD_JOB_NAME.to_string())
            }
            if !capabilities.contains(&INIT_FN_NAME.to_string()) {
                capabilities.push(INIT_FN_NAME.to_string())
            }
        }
        capabilities
    }

    fn init(&self) -> Result<()> {
        if let Some(init_meta) = self
            .bare
            .ast
            .iter_functions()
            .find(|meta| meta.name == INIT_FN_NAME)
        {
            if init_meta.params.is_empty() {
                self.call_fn(init_meta.name, ())
            } else {
                self.call_fn(init_meta.name, (Map::default(),))
            }
        } else {
            self.bare.default_init(
                self.evaluate_var(PLUGIN_CONFIG_CONST_NAME)?,
                self.evaluate_optional_var(DEFAULT_SERVICES_CONST_NAME)?
                    .unwrap_or_default(),
            )
        }
    }

    fn upload(&self) -> Result<()> {
        if let Some(fn_meta) = self
            .bare
            .ast
            .iter_functions()
            .find(|meta| meta.name == UPLOAD_JOB_NAME)
        {
            if fn_meta.params.is_empty() {
                self.call_fn(UPLOAD_JOB_NAME, ())
            } else {
                // keep backward compatibility with obsolete `upload(param)` functions
                self.call_fn(UPLOAD_JOB_NAME, ("",))
                    .map(|_ignored: String| ())
            }
        } else {
            let mut config: PluginConfig = self.evaluate_var(PLUGIN_CONFIG_CONST_NAME)?;
            config
                .services
                .retain(|service| service.use_blockchain_data);
            for service in &config.services {
                self.bare.babel_engine.stop_job(&service.name)?;
            }
            let pre_upload_jobs = self.bare.run_actions(config.pre_upload, vec![])?;
            self.bare.create_and_start_job(
                UPLOAD_JOB_NAME,
                plugin_config::build_upload_job_config(config.upload, pre_upload_jobs),
            )?;
            let post_upload_jobs = self
                .bare
                .run_jobs(config.post_upload, vec![UPLOAD_JOB_NAME.to_string()])?;
            self.bare
                .start_services(config.services, post_upload_jobs)?;
            Ok(())
        }
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
        let jobs = self.bare.babel_engine.get_jobs()?;
        let check_job_status = |name: &str, expected_status: JobStatus| {
            jobs.get(name)
                .map(|info| info.status == expected_status)
                .unwrap_or(false)
        };
        if check_job_status(UPLOAD_JOB_NAME, JobStatus::Running) {
            Ok(ApplicationStatus::Uploading)
        } else if check_job_status(DOWNLOAD_JOB_NAME, JobStatus::Running) {
            Ok(ApplicationStatus::Downloading)
        } else if self
            .evaluate_var::<PluginConfig>(PLUGIN_CONFIG_CONST_NAME)
            .is_ok_and(|config| {
                config
                    .services
                    .iter()
                    .any(|service| check_job_status(&service.name, JobStatus::Pending))
            })
        {
            Ok(ApplicationStatus::Initializing)
        } else {
            Ok(from_dynamic(
                &self.call_fn::<_, Dynamic>("application_status", ())?,
            )?)
        }
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

    fn call_custom_method(&self, name: &str, param: &str) -> Result<String> {
        self.call_fn(name, (param.to_string(),))
    }
}

fn into_rhai_result<T>(result: Result<T>) -> std::result::Result<T, Box<rhai::EvalAltResult>> {
    Ok(result.map_err(|err| <String as Into<rhai::EvalAltResult>>::into(format!("{err:#}")))?)
}

/// Helper structure that represents `JrpcRequest` from Rhai script perspective.
/// It allows any `Dynamic` object to be set as params. Then `rhai_plugin` takes care of json
/// serialization of `Map` or `Array`, since `babel_engine` expect params to be already
/// serialized to json string.
/// It also adds backward compatibility for headers.
#[derive(Deserialize)]
pub struct BareJrpcRequest {
    pub host: String,
    pub method: String,
    pub params: Option<Dynamic>,
    pub headers: Option<Dynamic>,
}

impl TryInto<JrpcRequest> for BareJrpcRequest {
    type Error = Error;

    fn try_into(self) -> std::result::Result<JrpcRequest, Self::Error> {
        let params = match self.params {
            Some(value) => {
                if value.is_map() || value.is_array() {
                    Some(serde_json::to_string(&value)?)
                } else {
                    bail!("unsupported jrpc params type")
                }
            }
            None => None,
        };
        let headers = match self.headers {
            Some(value) => {
                if value.is_array() {
                    Some(from_dynamic::<Vec<(String, String)>>(&value)?)
                } else if value.is_map() {
                    Some(
                        from_dynamic::<HashMap<String, String>>(&value)?
                            .into_iter()
                            .collect(),
                    )
                } else {
                    bail!("unsupported jrpc headers type")
                }
            }
            None => None,
        };
        Ok(JrpcRequest {
            host: self.host,
            method: self.method,
            params,
            headers,
        })
    }
}

/// Backward compatibility structure that represents `RestRequest` from Rhai script perspective.
#[derive(Deserialize)]
pub struct BareRestRequest {
    pub url: String,
    pub headers: Option<Dynamic>,
}

impl TryInto<RestRequest> for BareRestRequest {
    type Error = Error;

    fn try_into(self) -> std::result::Result<RestRequest, Self::Error> {
        let headers = match self.headers {
            Some(value) => {
                if value.is_array() {
                    Some(from_dynamic::<Vec<(String, String)>>(&value)?)
                } else if value.is_map() {
                    Some(
                        from_dynamic::<HashMap<String, String>>(&value)?
                            .into_iter()
                            .collect(),
                    )
                } else {
                    bail!("unsupported jrpc headers type")
                }
            }
            None => None,
        };
        Ok(RestRequest {
            url: self.url,
            headers,
        })
    }
}

/// Http response.
#[derive(Serialize, Clone)]
pub struct DressedHttpResponse {
    /// Http status code.
    pub status_code: u16,
    /// Response body as text.
    pub body: String,
}

impl DressedHttpResponse {
    fn get_status_code(&mut self) -> i64 {
        self.status_code as i64
    }
    fn get_body(&mut self) -> String {
        self.body.clone()
    }

    pub fn expect_with(
        &mut self,
        check: FnPtr,
    ) -> std::result::Result<Map, Box<rhai::EvalAltResult>> {
        let engine = rhai::Engine::new();
        into_rhai_result(
            if check.call(&engine, &AST::empty(), (self.status_code as i64,))? {
                Ok(engine.parse_json(&self.body, true)?)
            } else {
                Err(anyhow!("unexpected status_code: {}", self.status_code))
            },
        )
    }
    pub fn expect(&mut self, expected: i64) -> std::result::Result<Map, Box<rhai::EvalAltResult>> {
        into_rhai_result(if self.status_code as i64 == expected {
            Ok(rhai::Engine::new().parse_json(&self.body, true)?)
        } else {
            Err(anyhow!("unexpected status_code: {}", self.status_code))
        })
    }
}

impl From<HttpResponse> for DressedHttpResponse {
    fn from(value: HttpResponse) -> Self {
        Self {
            status_code: value.status_code,
            body: value.body,
        }
    }
}

/// Sh script response.
#[derive(Serialize, Clone)]
pub struct DressedShResponse {
    /// script exit code
    pub exit_code: i32,
    /// stdout
    pub stdout: String,
    /// stderr
    pub stderr: String,
}

impl DressedShResponse {
    fn get_exit_code(&mut self) -> i64 {
        self.exit_code as i64
    }
    fn get_stdout(&mut self) -> String {
        self.stdout.clone()
    }
    fn get_stderr(&mut self) -> String {
        self.stderr.clone()
    }

    pub fn unwrap(&mut self) -> std::result::Result<String, Box<rhai::EvalAltResult>> {
        into_rhai_result(if self.exit_code == 0 {
            Ok(self.stdout.clone())
        } else {
            Err(anyhow!("unexpected exit_code: {}", self.exit_code))
        })
    }
}

impl From<ShResponse> for DressedShResponse {
    fn from(value: ShResponse) -> Self {
        Self {
            exit_code: value.exit_code,
            stdout: value.stdout,
            stderr: value.stderr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{
        HttpResponse, JobConfig, JobInfo, JobStatus, JobType, JrpcRequest, NodeEnv, RestRequest,
        RestartConfig, RestartPolicy, ShResponse,
    };
    use crate::metadata::{
        firewall, BabelConfig, NetConfiguration, NetType, RamdiskConfiguration, Requirements,
    };
    use crate::plugin_config::{AlternativeDownload, Job};
    use eyre::bail;
    use mockall::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    mock! {
        #[derive(Debug)]
        pub BabelEngine {}

        impl Engine for BabelEngine {
            fn create_job(&self, job_name: &str, job_config: JobConfig) -> Result<()>;
            fn start_job(&self, job_name: &str) -> Result<()>;
            fn stop_job(&self, job_name: &str) -> Result<()>;
            fn job_info(&self, job_name: &str) -> Result<engine::JobInfo>;
            fn get_jobs(&self) -> Result<engine::JobsInfo>;
            fn run_jrpc(&self, req: JrpcRequest, timeout: Option<Duration>) -> Result<HttpResponse>;
            fn run_rest(&self, req: RestRequest, timeout: Option<Duration>) -> Result<HttpResponse>;
            fn run_sh(&self, body: &str, timeout: Option<Duration>) -> Result<ShResponse>;
            fn sanitize_sh_param(&self, param: &str) -> Result<String>;
            fn render_template(
                &self,
                template: &Path,
                destination: &Path,
                params: &str,
            ) -> Result<()>;
            fn node_params(&self) -> HashMap<String, String>;
            fn node_env(&self) -> NodeEnv;
            fn save_data(&self, value: &str) -> Result<()>;
            fn load_data(&self) -> Result<String>;
            fn log(&self, level: Level, message: &str);
            fn add_task(
                &self,
                task_name: &str,
                schedule: &str,
                function_name: &str,
                function_param: &str,
            ) -> Result<()>;
            fn delete_task(&self, task_name: &str) -> Result<()>;
            fn is_download_completed(&self) -> Result<bool>;
            fn has_blockchain_archive(&self) -> Result<bool>;
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
        Ok(())
    }

    #[test]
    fn test_call_build_in_functions() -> Result<()> {
        let script = r#"
        fn init() {
            save_data("init");
        }

        fn upload() {
            save_data("upload");
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
"#;
        let mut babel = MockBabelEngine::new();
        babel
            .expect_save_data()
            .with(predicate::eq(INIT_FN_NAME))
            .return_once(|_| Ok(()));
        babel
            .expect_save_data()
            .with(predicate::eq("upload"))
            .return_once(|_| Ok(()));
        babel
            .expect_get_jobs()
            .return_once(|| Ok(HashMap::default()));
        let plugin = RhaiPlugin::new(script, babel)?.clone(); // call clone() to make sure it works as well
        plugin.init()?;
        plugin.upload()?;
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
        Ok(())
    }

    #[test]
    fn test_call_custom_method_call_engine() -> Result<()> {
        let script = r#"
    fn custom_method(param) {
        debug("debug message");
        info("info message");
        warn("warn message");
        error("error message");
        let out = parse_json(param).a;
        start_job("test_job_name", #{
            job_type: #{
                run_sh: "job body",
            },
            restart: #{
                "on_failure": #{
                    max_retries: 3,
                    backoff_timeout_ms: 1000,
                    backoff_base_ms: 500,
                },
            },
            needs: ["needed"],
            run_as: "some_user",
        });
        create_job("download_job_name", #{
             job_type: #{
                download: #{},
            },
            restart: "never",
        });
        start_job("download_job_name");
        stop_job("test_job_name");
        out += "|" + job_info("test_job_name");
        out += "|" + get_jobs();
        out += "|" + run_jrpc(#{host: "host", method: "method", headers: [["custom_header", "header value"]]}).expect(200).key;
        out += "|" + run_jrpc(#{host: "host", method: "method", params: #{"chain": "x"}}, 1).expect(|code| code >= 200).param;
        out += "|" + run_jrpc(#{host: "host", method: "method", params: ["positional", "args", "array"]}, 1).body;
        let http_out = run_rest(#{url: "url"});
        out += "|" + http_out.body;
        out += "|" + http_out.status_code;
        out += "|" + run_rest(#{url: "url", headers: [["another-header", "another value"]]}, 2).body;
        out += "|" + run_sh("body").unwrap();
        let sh_out = run_sh("body", 3);
        out += "|" + sh_out.stderr;
        out += "|" + sh_out.exit_code;
        out += "|" + sanitize_sh_param("sh param");
        out += "|" + parse_hex("0xff").to_string();
        render_template("/template/path", "output/path.cfg", #{ PARAM1: "Value I"});
        out += "|" + node_params().to_json();
        out += "|" + node_env().node_id;
        save_data("some plugin data");
        out += "|" + load_data();
        out
    }
"#;
        let mut babel = MockBabelEngine::new();
        babel
            .expect_log()
            .with(
                predicate::eq(Level::DEBUG),
                predicate::eq("\"debug message\""),
            )
            .return_once(|_, _| ());
        babel
            .expect_log()
            .with(predicate::eq(Level::INFO), predicate::eq("info message"))
            .return_once(|_, _| ());
        babel
            .expect_log()
            .with(predicate::eq(Level::WARN), predicate::eq("warn message"))
            .return_once(|_, _| ());
        babel
            .expect_log()
            .with(predicate::eq(Level::ERROR), predicate::eq("error message"))
            .return_once(|_, _| ());
        babel
            .expect_create_job()
            .with(
                predicate::eq("test_job_name"),
                predicate::eq(JobConfig {
                    job_type: JobType::RunSh("job body".to_string()),
                    restart: RestartPolicy::OnFailure(RestartConfig {
                        backoff_timeout_ms: 1000,
                        backoff_base_ms: 500,
                        max_retries: Some(3),
                    }),
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec!["needed".to_string()]),
                    run_as: Some("some_user".to_string()),
                    log_buffer_capacity_mb: None,
                }),
            )
            .return_once(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("test_job_name"))
            .return_once(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("download_job_name"),
                predicate::eq(JobConfig {
                    job_type: JobType::Download {
                        destination: None,
                        max_connections: None,
                        max_runners: None,
                    },
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: None,
                    run_as: None,
                    log_buffer_capacity_mb: None,
                }),
            )
            .return_once(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("download_job_name"))
            .return_once(|_| Ok(()));
        babel
            .expect_stop_job()
            .with(predicate::eq("test_job_name"))
            .return_once(|_| Ok(()));
        babel
            .expect_job_info()
            .with(predicate::eq("test_job_name"))
            .return_once(|_| {
                Ok(JobInfo {
                    status: JobStatus::Finished {
                        exit_code: Some(1),
                        message: "error msg".to_string(),
                    },
                    progress: None,
                    restart_count: 0,
                    logs: vec![],
                    upgrade_blocking: false,
                })
            });
        babel.expect_get_jobs().return_once(|| {
            Ok(HashMap::from_iter([(
                "custom_name".to_string(),
                JobInfo {
                    status: JobStatus::Running,
                    progress: Default::default(),
                    restart_count: 0,
                    logs: vec![],
                    upgrade_blocking: true,
                },
            )]))
        });
        babel
            .expect_run_jrpc()
            .with(
                predicate::eq(JrpcRequest {
                    host: "host".to_string(),
                    method: "method".to_string(),
                    params: None,
                    headers: Some(vec![(
                        "custom_header".to_string(),
                        "header value".to_string(),
                    )]),
                }),
                predicate::eq(None),
            )
            .return_once(|_, _| {
                Ok(HttpResponse {
                    status_code: 200,
                    body: r#"{"key": "jrpc_response"}"#.to_string(),
                })
            });
        babel
            .expect_run_jrpc()
            .with(
                predicate::eq(JrpcRequest {
                    host: "host".to_string(),
                    method: "method".to_string(),
                    params: Some(r#"{"chain":"x"}"#.to_string()),
                    headers: None,
                }),
                predicate::eq(Some(Duration::from_secs(1))),
            )
            .return_once(|_, _| {
                Ok(HttpResponse {
                    status_code: 204,
                    body: r#"{"param":"jrpc_with_map_and_timeout_response"}"#.to_string(),
                })
            });
        babel
            .expect_run_jrpc()
            .with(
                predicate::eq(JrpcRequest {
                    host: "host".to_string(),
                    method: "method".to_string(),
                    params: Some(r#"["positional","args","array"]"#.to_string()),
                    headers: None,
                }),
                predicate::eq(Some(Duration::from_secs(1))),
            )
            .return_once(|_, _| {
                Ok(HttpResponse {
                    status_code: 200,
                    body: "jrpc_with_array_and_timeout_response".to_string(),
                })
            });
        babel
            .expect_run_rest()
            .with(
                predicate::eq(RestRequest {
                    url: "url".to_string(),
                    headers: None,
                }),
                predicate::eq(None),
            )
            .return_once(|_, _| {
                Ok(HttpResponse {
                    status_code: 200,
                    body: "rest_response".to_string(),
                })
            });
        babel
            .expect_run_rest()
            .with(
                predicate::eq(RestRequest {
                    url: "url".to_string(),
                    headers: Some(vec![(
                        "another-header".to_string(),
                        "another value".to_string(),
                    )]),
                }),
                predicate::eq(Some(Duration::from_secs(2))),
            )
            .return_once(|_, _| {
                Ok(HttpResponse {
                    status_code: 204,
                    body: "rest_with_timeout_response".to_string(),
                })
            });
        babel
            .expect_run_sh()
            .with(predicate::eq("body"), predicate::eq(None))
            .return_once(|_, _| {
                Ok(ShResponse {
                    exit_code: 0,
                    stdout: "sh_response".to_string(),
                    stderr: "".to_string(),
                })
            });
        babel
            .expect_run_sh()
            .with(
                predicate::eq("body"),
                predicate::eq(Some(Duration::from_secs(3))),
            )
            .return_once(|_, _| {
                Ok(ShResponse {
                    exit_code: -1,
                    stdout: "".to_string(),
                    stderr: "sh_with_timeout_err".to_string(),
                })
            });
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
        babel.expect_node_env().return_once(|| NodeEnv {
            node_id: "node_id".to_string(),
            node_name: "".to_string(),
            node_version: "".to_string(),
            blockchain_type: "".to_string(),
            node_type: "".to_string(),
            node_ip: "".to_string(),
            node_gateway: "".to_string(),
            dev_mode: false,
            bv_host_id: "".to_string(),
            bv_host_name: "".to_string(),
            bv_api_url: "".to_string(),
            org_id: "".to_string(),
        });
        babel
            .expect_save_data()
            .with(predicate::eq("some plugin data"))
            .return_once(|_| Ok(()));
        babel
            .expect_load_data()
            .return_once(|| Ok("loaded data".to_string()));

        let plugin = RhaiPlugin::new(script, babel)?;
        assert_eq!(
            r#"json_as_param|#{"logs": [], "progress": (), "restart_count": 0, "status": #{"finished": #{"exit_code": 1, "message": "error msg"}}, "upgrade_blocking": false}|#{"custom_name": #{"logs": [], "progress": (), "restart_count": 0, "status": "running", "upgrade_blocking": true}}|jrpc_response|jrpc_with_map_and_timeout_response|jrpc_with_array_and_timeout_response|rest_response|200|rest_with_timeout_response|sh_response|sh_with_timeout_err|-1|sh_sanitized|255|{"key_A":"value_A"}|node_id|loaded data"#,
            plugin.call_custom_method("custom_method", r#"{"a":"json_as_param"}"#)?
        );
        Ok(())
    }

    #[test]
    fn test_babel_version() -> Result<()> {
        let script = r#"const BABEL_VERSION = "777.777.777";"#;
        assert_eq!(
            format!(
                "Required minimum babel version is `777.777.777`, running is `{}`",
                env!("CARGO_PKG_VERSION")
            ),
            RhaiPlugin::new(script, MockBabelEngine::new())
                .unwrap_err()
                .to_string()
        );
        Ok(())
    }

    #[test]
    fn test_application_status() -> Result<()> {
        let script = r#"
            const PLUGIN_CONFIG = #{
                services: [
                    #{
                        name: "blockchain_service_a",
                        run_sh: `echo A`,
                    },
                    #{
                        name: "blockchain_service_b",
                        run_sh: `echo B`,
                    }
                ],
            };

            fn application_status() {
                "broadcasting"
            }
            "#;
        let mut babel = MockBabelEngine::new();
        babel.expect_get_jobs().once().returning(|| {
            Ok(HashMap::from_iter([(
                "download".to_string(),
                JobInfo {
                    status: JobStatus::Running,
                    progress: Default::default(),
                    restart_count: 0,
                    logs: vec![],
                    upgrade_blocking: true,
                },
            )]))
        });
        babel.expect_get_jobs().once().returning(|| {
            Ok(HashMap::from_iter([
                (
                    "blockchain_service_a".to_string(),
                    JobInfo {
                        status: JobStatus::Running,
                        progress: Default::default(),
                        restart_count: 0,
                        logs: vec![],
                        upgrade_blocking: true,
                    },
                ),
                (
                    "blockchain_service_b".to_string(),
                    JobInfo {
                        status: JobStatus::Pending,
                        progress: Default::default(),
                        restart_count: 0,
                        logs: vec![],
                        upgrade_blocking: true,
                    },
                ),
            ]))
        });
        babel.expect_get_jobs().once().returning(|| {
            Ok(HashMap::from_iter([(
                UPLOAD_JOB_NAME.to_string(),
                JobInfo {
                    status: JobStatus::Running,
                    progress: Default::default(),
                    restart_count: 0,
                    logs: vec![],
                    upgrade_blocking: true,
                },
            )]))
        });
        babel
            .expect_get_jobs()
            .once()
            .returning(|| Ok(HashMap::default()));

        let plugin = RhaiPlugin::new(script, babel)?;
        assert_eq!(ApplicationStatus::Downloading, plugin.application_status()?);
        assert_eq!(
            ApplicationStatus::Initializing,
            plugin.application_status()?
        );
        assert_eq!(ApplicationStatus::Uploading, plugin.application_status()?);
        assert_eq!(
            ApplicationStatus::Broadcasting,
            plugin.application_status()?
        );
        Ok(())
    }

    #[test]
    fn test_run_actions_without_jobs() -> Result<()> {
        let mut babel = MockBabelEngine::new();
        babel
            .expect_run_sh()
            .with(predicate::eq("echo action_cmd"), predicate::eq(None))
            .return_once(|_, _| {
                Ok(ShResponse {
                    exit_code: 0,
                    stdout: "".to_string(),
                    stderr: "".to_string(),
                })
            });
        let plugin = RhaiPlugin::new("", babel)?;
        assert_eq!(
            vec!["some_job".to_string()],
            plugin
                .bare
                .run_actions(
                    Some(Actions {
                        commands: vec!["echo action_cmd".to_string()],
                        jobs: vec![]
                    }),
                    vec!["some_job".to_string()]
                )
                .unwrap()
        );
        Ok(())
    }

    #[test]
    fn test_default_upload() -> Result<()> {
        let script = r#"
            const PLUGIN_CONFIG = #{
                services: [
                    #{
                        name: "blockchain_service",
                        run_sh: `echo A`,
                        run_as: "some_user",
                    },
                    #{
                        name: "non_blockchain_service",
                        run_sh: `echo B`,
                        use_blockchain_data: false,
                    },
                ],
                pre_upload: #{
                    commands: [
                        `echo pre_upload_cmd`,
                    ],
                    jobs: [
                        #{
                            name: "pre_upload_job",
                            run_sh: `echo pre_upload_job`,
                            needs: ["some"],
                        }
                    ]
                },
                post_upload: [
                    #{
                        name: "post_upload_job",
                        run_sh: `echo post_upload_job`,
                        needs: ["some"],
                    }
                ],
            };
            "#;
        let mut babel = MockBabelEngine::new();
        babel
            .expect_run_sh()
            .with(predicate::eq("echo pre_upload_cmd"), predicate::eq(None))
            .return_once(|_, _| {
                Ok(ShResponse {
                    exit_code: 0,
                    stdout: "".to_string(),
                    stderr: "".to_string(),
                })
            });
        babel
            .expect_stop_job()
            .with(predicate::eq("blockchain_service"))
            .return_once(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("pre_upload_job"),
                predicate::eq(plugin_config::build_job_config(Job {
                    name: "pre_upload_job".to_string(),
                    run_sh: "echo pre_upload_job".to_string(),
                    restart: None,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec!["some".to_string()]),
                    run_as: None,
                    log_buffer_capacity_mb: None,
                })),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("pre_upload_job"))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq(UPLOAD_JOB_NAME),
                predicate::eq(plugin_config::build_upload_job_config(
                    None,
                    vec!["pre_upload_job".to_string()],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq(UPLOAD_JOB_NAME))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("post_upload_job"),
                predicate::eq(plugin_config::build_job_config(Job {
                    name: "post_upload_job".to_string(),
                    run_sh: "echo post_upload_job".to_string(),
                    restart: None,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec!["some".to_string(), UPLOAD_JOB_NAME.to_string()]),
                    run_as: None,
                    log_buffer_capacity_mb: None,
                })),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("post_upload_job"))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("blockchain_service"),
                predicate::eq(plugin_config::build_service_job_config(
                    Service {
                        name: "blockchain_service".to_string(),
                        run_sh: "echo A".to_string(),
                        restart_config: None,
                        shutdown_timeout_secs: None,
                        shutdown_signal: None,
                        run_as: Some("some_user".to_string()),
                        use_blockchain_data: true,
                        log_buffer_capacity_mb: None,
                    },
                    vec!["post_upload_job".to_string()],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("blockchain_service"))
            .once()
            .returning(|_| Ok(()));

        let plugin = RhaiPlugin::new(script, babel)?;
        assert!(plugin.capabilities().iter().any(|v| v == "upload"));
        plugin.upload().unwrap();
        Ok(())
    }

    #[test]
    fn test_default_init() -> Result<()> {
        let script = r#"
            const PLUGIN_CONFIG = #{
                config_files: [
                    #{
                        template: "/var/lib/babel/config.template",
                        destination: "/etc/service.config",
                        params: #{
                            node_name: node_env().node_name
                        },
                    },
                ],
                init: #{
                    commands: [
                        `echo init_cmd`,
                    ],
                    jobs: [
                        #{
                            name: "init_job",
                            run_sh: `echo init_job`,
                        }
                    ]
                },
                alternative_download: #{
                    run_sh: `alternative download`,
                },
                post_download: [
                    #{
                        name: "post_download_job",
                        run_sh: `echo post_download_job`,
                        needs: ["some"],
                    }
                ],
                services: [
                    #{
                        name: "blockchain_service",
                        run_sh: `echo A`,
                    },
                    #{
                        name: "non_blockchain_service",
                        run_sh: `echo B`,
                        use_blockchain_data: false,
                    },
                ],
                scheduled: [
                    #{
                        name: "some_task",
                        schedule: "* * * * * * *",
                        function: "fn_name",
                        param: "param_value",
                    }
                ],
                disable_default_services: false,
            };
            const DEFAULT_SERVICES = [
                #{
                    name: "nginx",
                    run_sh: `nginx with params`,
                },
            ];
            "#;
        let mut babel = MockBabelEngine::new();
        babel.expect_node_env().returning(|| NodeEnv {
            node_id: "node_id".to_string(),
            node_name: "node name".to_string(),
            node_version: "".to_string(),
            blockchain_type: "".to_string(),
            node_type: "".to_string(),
            node_ip: "".to_string(),
            node_gateway: "".to_string(),
            dev_mode: false,
            bv_host_id: "".to_string(),
            bv_host_name: "".to_string(),
            bv_api_url: "".to_string(),
            org_id: "".to_string(),
        });
        babel
            .expect_render_template()
            .with(
                predicate::eq(PathBuf::from("/var/lib/babel/config.template")),
                predicate::eq(PathBuf::from("/etc/service.config")),
                predicate::eq(r#"{"node_name":"node name"}"#),
            )
            .once()
            .returning(|_, _, _| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("nginx"),
                predicate::eq(plugin_config::build_service_job_config(
                    Service {
                        name: "nginx".to_string(),
                        run_sh: "nginx with params".to_string(),
                        restart_config: None,
                        shutdown_timeout_secs: None,
                        shutdown_signal: None,
                        run_as: None,
                        use_blockchain_data: false,
                        log_buffer_capacity_mb: None,
                    },
                    vec![],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("nginx"))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_run_sh()
            .with(predicate::eq("echo init_cmd"), predicate::eq(None))
            .return_once(|_, _| {
                Ok(ShResponse {
                    exit_code: 0,
                    stdout: "".to_string(),
                    stderr: "".to_string(),
                })
            });
        babel
            .expect_create_job()
            .with(
                predicate::eq("init_job"),
                predicate::eq(plugin_config::build_job_config(Job {
                    name: "init_job".to_string(),
                    run_sh: "echo init_job".to_string(),
                    restart: None,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec![]),
                    run_as: None,
                    log_buffer_capacity_mb: None,
                })),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("init_job"))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_is_download_completed()
            .once()
            .returning(|| Ok(false));
        babel
            .expect_has_blockchain_archive()
            .once()
            .returning(|| Ok(false));
        babel
            .expect_create_job()
            .with(
                predicate::eq(DOWNLOAD_JOB_NAME),
                predicate::eq(plugin_config::build_alternative_download_job_config(
                    AlternativeDownload {
                        run_sh: "alternative download".to_string(),
                        restart_config: None,
                        run_as: None,
                        log_buffer_capacity_mb: None,
                    },
                    vec!["init_job".to_string()],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq(DOWNLOAD_JOB_NAME))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("post_download_job"),
                predicate::eq(plugin_config::build_job_config(Job {
                    name: "post_download_job".to_string(),
                    run_sh: "echo post_download_job".to_string(),
                    restart: None,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: Some(vec!["some".to_string(), DOWNLOAD_JOB_NAME.to_string()]),
                    run_as: None,
                    log_buffer_capacity_mb: None,
                })),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("post_download_job"))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("blockchain_service"),
                predicate::eq(plugin_config::build_service_job_config(
                    Service {
                        name: "blockchain_service".to_string(),
                        run_sh: "echo A".to_string(),
                        restart_config: None,
                        shutdown_timeout_secs: None,
                        shutdown_signal: None,
                        run_as: None,
                        use_blockchain_data: true,
                        log_buffer_capacity_mb: None,
                    },
                    vec!["post_download_job".to_string()],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("blockchain_service"))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("non_blockchain_service"),
                predicate::eq(plugin_config::build_service_job_config(
                    Service {
                        name: "non_blockchain_service".to_string(),
                        run_sh: "echo B".to_string(),
                        restart_config: None,
                        shutdown_timeout_secs: None,
                        shutdown_signal: None,
                        run_as: None,
                        use_blockchain_data: false,
                        log_buffer_capacity_mb: None,
                    },
                    vec![],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("non_blockchain_service"))
            .once()
            .returning(|_| Ok(()));

        babel
            .expect_add_task()
            .with(
                predicate::eq("some_task"),
                predicate::eq("* * * * * * *"),
                predicate::eq("fn_name"),
                predicate::eq("param_value"),
            )
            .once()
            .returning(|_, _, _, _| Ok(()));
        let plugin = RhaiPlugin::new(script, babel)?;
        assert!(plugin.capabilities().iter().any(|v| v == "init"));
        plugin.init().unwrap();
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
        assert_eq!(
            "Rhai function 'custom_method_with_exception' returned error",
            plugin
                .call_custom_method("custom_method_with_exception", "")
                .unwrap_err()
                .to_string()
        );
        assert!(format!(
            "{:#}",
            plugin
                .call_custom_method("custom_failing_method", "")
                .unwrap_err()
        )
        .starts_with(
            "Rhai function 'custom_failing_method' returned error: Runtime error: some Rust error"
        ));
        assert_eq!(
            "Rhai function 'no_method' returned error: Function not found: no_method",
            format!(
                "{:#}",
                plugin.call_custom_method("no_method", "").unwrap_err()
            )
        );
        assert_eq!(
            "Invalid Rhai script - failed to deserialize METADATA",
            plugin.metadata().unwrap_err().to_string()
        );
        Ok(())
    }

    #[test]
    fn test_metadata() -> Result<()> {
        let meta_rhai = r#"
const METADATA = #{
    // comments are allowed
    kernel: "5.10.174-build.1+fc.ufw",
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
        log_buffer_capacity_mb: 128,
        swap_size_mb: 1024,
        ramdisks: [
            #{
                ram_disk_mount_point: "/mnt/ramdisk",
                ram_disk_size_mb: 512,
            },
        ],
    },
    firewall: #{
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
};
fn any_function() {}
"#;
        let meta_rust = BlockchainMetadata {
            kernel: "5.10.174-build.1+fc.ufw".to_string(),
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
                swap_size_mb: 1024,
                swap_file_location: "/swapfile".to_string(),
                ramdisks: Some(vec![RamdiskConfiguration {
                    ram_disk_mount_point: "/mnt/ramdisk".to_string(),
                    ram_disk_size_mb: 512,
                }]),
            },
            firewall: firewall::Config {
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
        };
        let plugin = RhaiPlugin::new(meta_rhai, MockBabelEngine::new())?;
        assert_eq!(meta_rust, plugin.metadata()?);
        Ok(())
    }
}
