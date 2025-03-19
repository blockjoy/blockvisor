use crate::plugin_config::{
    AuxService, COLD_INIT_JOB_NAME, DOWNLOADING_STATE_NAME, STARTING_STATE_NAME,
    UPLOADING_STATE_NAME,
};
use crate::{
    engine::{Engine, HttpResponse, JobConfig, JobStatus, JrpcRequest, RestRequest, ShResponse},
    plugin::{NodeHealth, Plugin, ProtocolStatus},
    plugin_config::{
        self, Actions, ConfigFile, Job, PluginConfig, Service, DOWNLOAD_JOB_NAME, UPLOAD_JOB_NAME,
    },
};
use eyre::{anyhow, bail, Context, Error, Report, Result};
use rhai::module_resolvers::FileModuleResolver;
use rhai::{
    self,
    serde::{from_dynamic, to_dynamic},
    Dynamic, FnPtr, Map, AST,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::{path::Path, sync::Arc, time::Duration};
use tracing::Level;

pub const PLUGIN_CONFIG_CONST_NAME: &str = "PLUGIN_CONFIG";
pub const PLUGIN_CONFIG_FN_NAME: &str = "plugin_config";
const INIT_FN_NAME: &str = "init";
const PROTOCOL_STATUS_FN_NAME: &str = "protocol_status";
const BASE_CONFIG_CONST_NAME: &str = "BASE_CONFIG";
const BASE_CONFIG_FN_NAME: &str = "base_config";

#[derive(Debug)]
pub struct RhaiPlugin<E> {
    pub(crate) bare: BarePlugin<E>,
    rhai_engine: rhai::Engine,
}

impl<E: Engine + Sync + Send + 'static> Clone for RhaiPlugin<E> {
    fn clone(&self) -> Self {
        let mut rhai_engine = Self::new_rhai_engine(self.bare.babel_engine.clone());
        if let Some(plugin_dir) = &self.bare.plugin_path {
            rhai_engine.set_module_resolver(FileModuleResolver::new_with_path(plugin_dir));
        }

        let mut clone = Self {
            bare: self.bare.clone(),
            rhai_engine,
        };
        clone.register_defaults();
        clone
    }
}

#[derive(Debug)]
pub(crate) struct BarePlugin<E> {
    pub(crate) plugin_path: Option<PathBuf>,
    pub(crate) babel_engine: Arc<E>,
    pub(crate) ast: AST,
    pub(crate) plugin_config: Option<PluginConfig>,
}

impl<E: Engine + Sync + Send + 'static> Clone for BarePlugin<E> {
    fn clone(&self) -> Self {
        Self {
            plugin_path: self.plugin_path.clone(),
            babel_engine: self.babel_engine.clone(),
            ast: self.ast.clone(),
            plugin_config: self.plugin_config.clone(),
        }
    }
}

fn new_rhai_engine() -> rhai::Engine {
    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(64, 32);
    engine
}

impl<E: Engine + Sync + Send + 'static> RhaiPlugin<E> {
    pub fn from_str(script: &str, babel_engine: E) -> Result<Self> {
        let babel_engine = Arc::new(babel_engine);
        let rhai_engine = Self::new_rhai_engine(babel_engine.clone());

        // compile script to AST
        let ast = rhai_engine
            .compile(script)
            .with_context(|| "Rhai syntax error")?;
        Self::new(ast, babel_engine, rhai_engine, None)
    }

    pub fn from_file(plugin_path: PathBuf, babel_engine: E) -> Result<Self> {
        let babel_engine = Arc::new(babel_engine);
        let mut rhai_engine = Self::new_rhai_engine(babel_engine.clone());
        let plugin_dir = plugin_path
            .parent()
            .ok_or(anyhow!("invalid plugin parent dir"))?
            .to_path_buf();
        rhai_engine.set_module_resolver(FileModuleResolver::new_with_path(&plugin_dir));

        // compile script to AST
        let ast = rhai_engine
            .compile_file(plugin_path)
            .with_context(|| "Rhai syntax error")?;
        Self::new(ast, babel_engine, rhai_engine, Some(plugin_dir))
    }

    fn new(
        ast: AST,
        babel_engine: Arc<E>,
        rhai_engine: rhai::Engine,
        plugin_path: Option<PathBuf>,
    ) -> Result<Self> {
        let plugin_config = babel_engine.load_config().ok();
        let mut plugin = RhaiPlugin {
            bare: BarePlugin {
                plugin_path,
                babel_engine,
                ast,
                plugin_config,
            },
            rhai_engine,
        };
        plugin.register_defaults();
        Ok(plugin)
    }

    /// register all Babel engine methods
    fn new_rhai_engine(engine: Arc<E>) -> rhai::Engine {
        let mut rhai_engine = new_rhai_engine();
        let babel_engine = engine.clone();
        rhai_engine.register_fn("create_job", move |job_name: &str, job_config: Dynamic| {
            into_rhai_result(babel_engine.create_job(job_name, from_dynamic(&job_config)?))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("start_job", move |job_name: &str| {
            into_rhai_result(babel_engine.start_job(job_name))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("start_job", move |job_name: &str, job_config: Dynamic| {
            into_rhai_result(
                babel_engine
                    .create_job(job_name, from_dynamic(&job_config)?)
                    .and_then(|_| babel_engine.start_job(job_name)),
            )
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("stop_job", move |job_name: &str| {
            into_rhai_result(babel_engine.stop_job(job_name))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("job_info", move |job_name: &str| {
            to_dynamic(into_rhai_result(babel_engine.job_info(job_name))?)
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("get_jobs", move || {
            to_dynamic(into_rhai_result(babel_engine.get_jobs())?)
        });
        rhai_engine
            .register_type_with_name::<DressedHttpResponse>("DressedHttpResponse")
            .register_get("body", DressedHttpResponse::get_body)
            .register_get("status_code", DressedHttpResponse::get_status_code)
            .register_fn("expect", DressedHttpResponse::expect)
            .register_fn("expect", DressedHttpResponse::expect_with);
        let babel_engine = engine.clone();
        rhai_engine.register_fn("run_jrpc", move |req: Dynamic, timeout: i64| {
            let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
            let req = into_rhai_result(from_dynamic::<BareJrpcRequest>(&req)?.try_into())?;
            into_rhai_result(
                babel_engine
                    .run_jrpc(req, Some(Duration::from_secs(timeout)))
                    .map(DressedHttpResponse::from),
            )
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("run_jrpc", move |req: Dynamic| {
            let req = into_rhai_result(from_dynamic::<BareJrpcRequest>(&req)?.try_into())?;
            into_rhai_result(
                babel_engine
                    .run_jrpc(req, None)
                    .map(DressedHttpResponse::from),
            )
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("run_rest", move |req: Dynamic, timeout: i64| {
            let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
            let req = into_rhai_result(from_dynamic::<BareRestRequest>(&req)?.try_into())?;
            into_rhai_result(
                babel_engine
                    .run_rest(req, Some(Duration::from_secs(timeout)))
                    .map(DressedHttpResponse::from),
            )
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("run_rest", move |req: Dynamic| {
            let req = into_rhai_result(from_dynamic::<BareRestRequest>(&req)?.try_into())?;
            into_rhai_result(
                babel_engine
                    .run_rest(req, None)
                    .map(DressedHttpResponse::from),
            )
        });
        rhai_engine
            .register_type_with_name::<DressedShResponse>("DressedShResponse")
            .register_get("exit_code", DressedShResponse::get_exit_code)
            .register_get("stdout", DressedShResponse::get_stdout)
            .register_get("stderr", DressedShResponse::get_stderr)
            .register_fn("unwrap", DressedShResponse::unwrap);
        let babel_engine = engine.clone();
        rhai_engine.register_fn("run_sh", move |body: &str, timeout: i64| {
            let timeout = into_rhai_result(timeout.try_into().map_err(Error::new))?;
            into_rhai_result(
                babel_engine
                    .run_sh(body, Some(Duration::from_secs(timeout)))
                    .map(DressedShResponse::from),
            )
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("run_sh", move |body: &str| {
            into_rhai_result(babel_engine.run_sh(body, None).map(DressedShResponse::from))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("sanitize_sh_param", move |param: &str| {
            into_rhai_result(babel_engine.sanitize_sh_param(param))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn(
            "render_template",
            move |template: &str, destination: &str, params: Map| {
                into_rhai_result(babel_engine.render_template(
                    Path::new(&template.to_string()),
                    Path::new(&destination.to_string()),
                    &rhai::format_map_as_json(&params),
                ))
            },
        );
        let babel_engine = engine.clone();
        rhai_engine.register_fn("node_params", move || {
            Map::from_iter(
                babel_engine
                    .node_params()
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into())),
            )
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("node_env", move || to_dynamic(babel_engine.node_env()));
        let babel_engine = engine.clone();
        rhai_engine.register_fn("save_data", move |value: &str| {
            into_rhai_result(babel_engine.save_data(value))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("load_data", move || {
            into_rhai_result(babel_engine.load_data())
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("put_secret", move |name: &str, value: Vec<u8>| {
            into_rhai_result(babel_engine.put_secret(name, value))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn(
            "get_secret",
            move |name: &str| -> std::result::Result<Vec<u8>, Box<rhai::EvalAltResult>> {
                if let Some(value) = into_rhai_result(babel_engine.get_secret(name))? {
                    Ok(value)
                } else {
                    Err("not_found".into())
                }
            },
        );
        let babel_engine = engine.clone();
        rhai_engine.register_fn("file_write", move |path: &str, content: Vec<u8>| {
            into_rhai_result(babel_engine.file_write(Path::new(&path.to_string()), content))
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn(
            "file_read",
            move |path: &str| -> std::result::Result<Vec<u8>, Box<rhai::EvalAltResult>> {
                into_rhai_result(babel_engine.file_read(Path::new(&path.to_string())))
            },
        );
        let babel_engine = engine.clone();
        rhai_engine.register_fn(
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
        let babel_engine = engine.clone();
        rhai_engine.register_fn(
            "add_task",
            move |task_name: &str, schedule: &str, function_name: &str| {
                into_rhai_result(babel_engine.add_task(task_name, schedule, function_name, ""))
            },
        );
        let babel_engine = engine.clone();
        rhai_engine.register_fn("delete_task", move |task_name: &str| {
            into_rhai_result(babel_engine.delete_task(task_name))
        });
        let babel_engine = engine.clone();
        rhai_engine.on_debug(move |msg, _, _| {
            babel_engine.log(Level::DEBUG, msg);
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("info", move |msg: &str| {
            babel_engine.log(Level::INFO, msg);
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("warn", move |msg: &str| {
            babel_engine.log(Level::WARN, msg);
        });
        let babel_engine = engine.clone();
        rhai_engine.register_fn("error", move |msg: &str| {
            babel_engine.log(Level::ERROR, msg);
        });

        // register other utils
        rhai_engine.register_fn("parse_json", |json: &str| {
            rhai::Engine::new().parse_json(json, true)
        });
        rhai_engine.register_fn("parse_hex", |hex: &str| {
            into_rhai_result(
                i64::from_str_radix(hex.strip_prefix("0x").unwrap_or(hex), 16).map_err(Report::new),
            )
        });
        rhai_engine.register_fn("system_time", || chrono::Utc::now().timestamp());
        rhai_engine.register_fn("parse_rfc3339", |value: &str| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|dt| dt.timestamp())
                .map_err(|err| format!("{err:#}"))
        });
        rhai_engine.register_fn("parse_rfc2822(", |value: &str| {
            chrono::DateTime::parse_from_rfc2822(value)
                .map(|dt| dt.timestamp())
                .map_err(|err| format!("{err:#}"))
        });
        rhai_engine.register_fn("parse_time", |value: &str, fmt: &str| {
            chrono::DateTime::parse_from_str(value, fmt)
                .map(|dt| dt.timestamp())
                .map_err(|err| format!("{err:#}"))
        });
        rhai_engine
    }

    fn register_defaults(&mut self) {
        let bare = self.bare.clone();
        self.rhai_engine.register_fn("default_init", move || {
            into_rhai_result(bare.default_init())
        });
        let bare = self.bare.clone();
        self.rhai_engine.register_fn("default_upload", move || {
            into_rhai_result(bare.default_upload())
        });
    }

    fn call_fn<P: rhai::FuncArgs, R: Clone + Send + Sync + 'static>(
        &self,
        name: &str,
        args: P,
    ) -> Result<R> {
        let mut scope = self.build_scope();
        self.rhai_engine
            .call_fn::<R>(&mut scope, &self.bare.ast, name, args)
            .with_context(|| format!("Rhai function '{name}' returned error"))
    }

    fn build_scope(&self) -> rhai::Scope<'static> {
        let mut scope = rhai::Scope::new();
        // LEGACY node support - remove once all nodes upgraded
        scope.push(
            "BLOCKCHAIN_DATA_PATH",
            self.bare.babel_engine.node_env().protocol_data_path,
        );
        scope
    }

    fn get_config<T: DeserializeOwned>(
        &self,
        scope: &rhai::Scope<'_>,
        config_fn_name: &str,
        config_const_name: &str,
    ) -> Result<Option<T>> {
        let dynamic = if self
            .bare
            .ast
            .iter_functions()
            .any(|meta| meta.name == config_fn_name && meta.params.is_empty())
        {
            Some(self.call_fn::<_, Dynamic>(config_fn_name, ())?)
        } else {
            None
        };
        // LEGACY left for backward compatibility - remove once all nodes upgraded
        let dynamic = dynamic.as_ref().or(scope.get(config_const_name));

        if let Some(dynamic) = dynamic {
            let value: T = from_dynamic(dynamic).with_context(|| {
                format!("Invalid Rhai script - failed to deserialize {config_fn_name}")
            })?;
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

    fn render_configs_files(&self, config_files: Option<Vec<ConfigFile>>) -> Result<()> {
        if let Some(config_files) = config_files {
            for config_file in config_files {
                self.babel_engine.render_template(
                    &config_file.template,
                    &config_file.destination,
                    &serde_json::to_string(&config_file.params)?,
                )?;
            }
        }
        Ok(())
    }

    fn start_aux_services(&self, services: Option<Vec<AuxService>>) -> Result<()> {
        if let Some(services) = services {
            for service in services {
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
                            use_protocol_data: false,
                            log_buffer_capacity_mb: service.log_buffer_capacity_mb,
                            log_timestamp: service.log_timestamp,
                        },
                        vec![],
                        vec![],
                    ),
                )?;
            }
        }
        Ok(())
    }

    fn start_services(
        &self,
        services: Vec<Service>,
        needs: Vec<String>,
        wait_for: Vec<String>,
    ) -> Result<()> {
        for service in services {
            let name = service.name.clone();
            self.create_and_start_job(
                &name,
                plugin_config::build_service_job_config(service, needs.clone(), wait_for.clone()),
            )?;
        }
        Ok(())
    }

    fn create_and_start_job(&self, name: &str, config: JobConfig) -> Result<()> {
        self.babel_engine
            .create_job(name, config)
            .and_then(|_| self.babel_engine.start_job(name))
    }

    fn default_init(&self) -> Result<()> {
        let Some(config) = self.plugin_config.clone() else {
            bail!("Missing {PLUGIN_CONFIG_FN_NAME} function")
        };
        self.babel_engine.stop_all_jobs()?;
        self.render_configs_files(config.config_files)?;
        self.start_aux_services(config.aux_services)?;
        let mut services_needs = self.run_actions(config.init, vec![])?;
        if self.babel_engine.protocol_data_stamp()?.is_none() {
            if self.babel_engine.has_protocol_archive()? {
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
            } else if let Some(cold_init) = config.cold_init {
                self.create_and_start_job(
                    COLD_INIT_JOB_NAME,
                    plugin_config::build_cold_init_job_config(cold_init, services_needs),
                )?;
                services_needs = vec![COLD_INIT_JOB_NAME.to_string()];
            }
        }
        self.start_services(config.services, services_needs, Default::default())?;
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

    fn default_upload(&self) -> Result<()> {
        let Some(mut config) = self.plugin_config.clone() else {
            bail!("Missing {PLUGIN_CONFIG_FN_NAME} function")
        };

        config.services.retain(|service| service.use_protocol_data);
        for service in &config.services {
            self.babel_engine.stop_job(&service.name)?;
        }
        let pre_upload_jobs = self.run_actions(config.pre_upload, vec![])?;
        self.create_and_start_job(
            UPLOAD_JOB_NAME,
            plugin_config::build_upload_job_config(config.upload, pre_upload_jobs),
        )?;
        let post_upload_jobs =
            self.run_jobs(config.post_upload, vec![UPLOAD_JOB_NAME.to_string()])?;
        self.start_services(config.services, Default::default(), post_upload_jobs)?;
        Ok(())
    }
}

impl<E: Engine + Sync + Send + 'static> Plugin for RhaiPlugin<E> {
    fn capabilities(&self) -> Vec<String> {
        let mut capabilities: Vec<_> = self
            .bare
            .ast
            .iter_functions()
            .map(|meta| meta.name.to_string())
            .collect();

        if self.bare.plugin_config.is_some() {
            if !capabilities.contains(&UPLOAD_JOB_NAME.to_string()) {
                capabilities.push(UPLOAD_JOB_NAME.to_string())
            }
            if !capabilities.contains(&INIT_FN_NAME.to_string()) {
                capabilities.push(INIT_FN_NAME.to_string())
            }
        }
        capabilities
    }

    fn init(&mut self) -> Result<()> {
        self.reload_plugin_config()?;

        if let Some(init_meta) = self
            .bare
            .ast
            .iter_functions()
            .find(|meta| meta.name == INIT_FN_NAME)
        {
            if init_meta.params.is_empty() {
                self.call_fn(INIT_FN_NAME, ())
            } else {
                // LEGACY node support - remove once all nodes upgraded
                // keep backward compatibility with obsolete `upload(param)` functions
                self.call_fn(INIT_FN_NAME, (Map::default(),))
            }
        } else {
            self.bare.default_init()
        }
    }

    fn reload_plugin_config(&mut self) -> Result<()> {
        let mut scope = self.build_scope();
        self.rhai_engine
            .run_ast_with_scope(&mut scope, &self.bare.ast)?;
        self.bare.plugin_config =
            self.get_config(&scope, PLUGIN_CONFIG_FN_NAME, PLUGIN_CONFIG_CONST_NAME)?;

        // LEGACY left for backward compatibility - remove once all nodes upgraded
        #[derive(Clone, Debug, Serialize, Deserialize)]
        pub struct BaseConfig {
            pub config_files: Option<Vec<ConfigFile>>,
            pub services: Option<Vec<AuxService>>,
        }
        let base_config =
            self.get_config::<BaseConfig>(&scope, BASE_CONFIG_FN_NAME, BASE_CONFIG_CONST_NAME)?;

        if let Some(plugin_config) = &mut self.bare.plugin_config {
            if let Some(base_config) = base_config {
                plugin_config.config_files =
                    match (plugin_config.config_files.take(), base_config.config_files) {
                        (Some(mut a), Some(mut b)) => {
                            a.append(&mut b);
                            Some(a)
                        }
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    };
                plugin_config.aux_services = base_config.services;
            }

            plugin_config.validate()?;
            self.bare.babel_engine.save_config(plugin_config)?;
        }
        Ok(())
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
                // LEGACY node support - remove once all nodes upgraded
                // keep backward compatibility with obsolete `upload(param)` functions
                self.call_fn(UPLOAD_JOB_NAME, ("",))
                    .map(|_ignored: String| ())
            }
        } else {
            self.bare.default_upload()
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

    fn protocol_status(&self) -> Result<ProtocolStatus> {
        let jobs = self.bare.babel_engine.get_jobs()?;
        let check_job_status = |name: &str, check_status: fn(&JobStatus) -> bool| {
            jobs.get(name)
                .map(|info| check_status(&info.status))
                .unwrap_or(false)
        };
        if check_job_status(UPLOAD_JOB_NAME, |status| *status == JobStatus::Running) {
            Ok(ProtocolStatus {
                state: UPLOADING_STATE_NAME.to_owned(),
                health: NodeHealth::Neutral,
            })
        } else if check_job_status(DOWNLOAD_JOB_NAME, |status| *status == JobStatus::Running) {
            Ok(ProtocolStatus {
                state: DOWNLOADING_STATE_NAME.to_owned(),
                health: NodeHealth::Neutral,
            })
        } else if self.bare.plugin_config.as_ref().is_some_and(|config| {
            config.services.iter().any(|service| {
                check_job_status(&service.name, |status| {
                    matches!(status, JobStatus::Pending { .. })
                })
            })
        }) {
            Ok(ProtocolStatus {
                state: STARTING_STATE_NAME.to_owned(),
                health: NodeHealth::Neutral,
            })
        } else {
            // LEGACY node support - remove once all nodes upgraded
            if !self.bare.ast.iter_functions().any(|function| {
                function.name == PROTOCOL_STATUS_FN_NAME && function.params.is_empty()
            }) && self
                .bare
                .ast
                .iter_functions()
                .any(|function| function.name == "application_status" && function.params.is_empty())
            {
                Ok(ProtocolStatus {
                    state: self.call_fn::<_, String>("application_status", ())?,
                    health: NodeHealth::Neutral,
                })
            } else {
                Ok(from_dynamic(
                    &self.call_fn::<_, Dynamic>(PROTOCOL_STATUS_FN_NAME, ())?,
                )?)
            }
        }
    }

    fn call_custom_method(&self, name: &str, param: &str) -> Result<String> {
        if self
            .bare
            .ast
            .iter_functions()
            .any(|meta| meta.name == name && meta.params.len() == 1)
        {
            self.call_fn(name, (param.to_string(),))
        } else if param.is_empty()
            && self
                .bare
                .ast
                .iter_functions()
                .any(|meta| meta.name == name && meta.params.is_empty())
        {
            self.call_fn(name, ())
        } else {
            bail!("no matching method '{name}' found")
        }
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
        self, HttpResponse, JobConfig, JobInfo, JobStatus, JobType, JrpcRequest, NodeEnv,
        RestRequest, RestartConfig, RestartPolicy, ShResponse,
    };
    use crate::plugin::NodeHealth;
    use crate::plugin_config::{AlternativeDownload, Job};
    use eyre::bail;
    use mockall::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    mock! {
        #[derive(Debug)]
        pub BabelEngine {}

        impl Engine for BabelEngine {
            fn create_job(&self, job_name: &str, job_config: JobConfig) -> Result<()>;
            fn start_job(&self, job_name: &str) -> Result<()>;
            fn stop_job(&self, job_name: &str) -> Result<()>;
            fn stop_all_jobs(&self) -> Result<()>;
            fn cleanup_job(&self, job_name: &str) -> Result<()>;
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
            fn save_config(&self, value: &PluginConfig) -> Result<()>;
            fn load_config(&self) -> Result<PluginConfig>;
            fn log(&self, level: Level, message: &str);
            fn add_task(
                &self,
                task_name: &str,
                schedule: &str,
                function_name: &str,
                function_param: &str,
            ) -> Result<()>;
            fn delete_task(&self, task_name: &str) -> Result<()>;
            fn protocol_data_stamp(&self) -> Result<Option<SystemTime>>;
            fn has_protocol_archive(&self) -> Result<bool>;
            fn get_secret(&self, name: &str) -> Result<Option<Vec<u8>>>;
            fn put_secret(&self, name: &str, value: Vec<u8>) -> Result<()>;
            fn file_read(&self, path: &Path) -> Result<Vec<u8>>;
            fn file_write(&self, path: &Path, content: Vec<u8>) -> Result<()>;
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

        let mut babel = MockBabelEngine::new();
        babel
            .expect_load_config()
            .returning(|| Ok(Default::default()));
        let plugin = RhaiPlugin::from_str(script, babel)?;
        let mut expected_capabilities = vec![
            "init".to_string(),
            "upload".to_string(),
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

        fn protocol_status() {
            #{state: "delinquent", health: "unhealthy"}
        }
"#;
        let mut babel = MockBabelEngine::new();
        babel.expect_node_env().returning(Default::default);
        babel.expect_load_config().returning(|| bail!("no config"));
        babel.expect_load_config().returning(|| bail!("no config"));
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
        let mut plugin = RhaiPlugin::from_str(script, babel)?.clone(); // call clone() to make sure it works as well
        plugin.init()?;
        plugin.upload()?;
        assert_eq!(77, plugin.height()?);
        assert_eq!(18, plugin.block_age()?);
        assert_eq!("block name", &plugin.name()?);
        assert_eq!("node address", &plugin.address()?);
        assert!(plugin.consensus()?);
        assert_eq!(
            ProtocolStatus {
                state: "delinquent".to_string(),
                health: NodeHealth::Unhealthy,
            },
            plugin.protocol_status()?
        );
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
        file_write("/file/path", "file content".to_blob());
        out += "|" + file_read("/file/path");
        put_secret("secret_key", "some plugin secret".to_blob());
        try {
            get_secret("secret_key_0")
        } catch (err) {
            if err == "not_found" {
                out += "|" + get_secret("secret_key").as_string();
            }
        }
        out
    }
"#;
        let mut babel = MockBabelEngine::new();
        babel.expect_load_config().returning(|| bail!("no config"));
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
                    wait_for: None,
                    run_as: Some("some_user".to_string()),
                    log_buffer_capacity_mb: None,
                    log_timestamp: None,
                    use_protocol_data: None,
                    one_time: None,
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
                        max_connections: None,
                        max_runners: None,
                    },
                    restart: RestartPolicy::Never,
                    shutdown_timeout_secs: None,
                    shutdown_signal: None,
                    needs: None,
                    wait_for: None,
                    run_as: None,
                    log_buffer_capacity_mb: None,
                    log_timestamp: None,
                    use_protocol_data: None,
                    one_time: None,
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
        babel.expect_node_env().times(2).returning(|| NodeEnv {
            node_id: "node_id".to_string(),
            ..Default::default()
        });
        babel
            .expect_save_data()
            .with(predicate::eq("some plugin data"))
            .return_once(|_| Ok(()));
        babel
            .expect_load_data()
            .return_once(|| Ok("loaded data".to_string()));
        babel
            .expect_file_write()
            .with(
                predicate::eq(Path::new("/file/path")),
                predicate::eq("file content".as_bytes().to_vec()),
            )
            .return_once(|_, _| Ok(()));
        babel
            .expect_file_read()
            .with(predicate::eq(Path::new("/file/path")))
            .return_once(|_| Ok("file content".as_bytes().to_vec()));
        babel
            .expect_put_secret()
            .with(
                predicate::eq("secret_key"),
                predicate::eq("some plugin secret".as_bytes().to_vec()),
            )
            .return_once(|_, _| Ok(()));
        babel
            .expect_get_secret()
            .with(predicate::eq("secret_key_0"))
            .return_once(|_| Ok(None));
        babel
            .expect_get_secret()
            .with(predicate::eq("secret_key"))
            .return_once(|_| Ok(Some("psecret".as_bytes().to_vec())));

        let plugin = RhaiPlugin::from_str(script, babel).unwrap();
        assert_eq!(
            r#"json_as_param|#{"logs": [], "progress": (), "restart_count": 0, "status": #{"finished": #{"exit_code": 1, "message": "error msg"}}, "upgrade_blocking": false}|#{"custom_name": #{"logs": [], "progress": (), "restart_count": 0, "status": "running", "upgrade_blocking": true}}|jrpc_response|jrpc_with_map_and_timeout_response|jrpc_with_array_and_timeout_response|rest_response|200|rest_with_timeout_response|sh_response|sh_with_timeout_err|-1|sh_sanitized|255|{"key_A":"value_A"}|node_id|loaded data|file content|psecret"#,
            plugin
                .call_custom_method("custom_method", r#"{"a":"json_as_param"}"#)
                .unwrap()
        );
        Ok(())
    }

    #[test]
    fn test_protocol_status() -> Result<()> {
        let script = r#"
            fn plugin_config() {#{
                services: [
                    #{
                        name: "protocol_service_a",
                        run_sh: `echo A`,
                    },
                    #{
                        name: "protocol_service_b",
                        run_sh: `echo B`,
                    }
                ],
            }}

            fn init() {}

            fn protocol_status() {
                #{state: "delinquent", health: "unhealthy"}
            }
            "#;
        let mut babel = MockBabelEngine::new();
        babel.expect_load_config().returning(|| bail!("no config"));
        babel.expect_save_config().once().returning(|_| Ok(()));
        babel.expect_node_env().returning(Default::default);
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
                    "protocol_service_a".to_string(),
                    JobInfo {
                        status: JobStatus::Running,
                        progress: Default::default(),
                        restart_count: 0,
                        logs: vec![],
                        upgrade_blocking: true,
                    },
                ),
                (
                    "protocol_service_b".to_string(),
                    JobInfo {
                        status: JobStatus::Pending {
                            waiting_for: vec![],
                        },
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

        let mut plugin = RhaiPlugin::from_str(script, babel)?;
        plugin.init()?;
        assert_eq!(
            ProtocolStatus {
                state: DOWNLOADING_STATE_NAME.to_owned(),
                health: NodeHealth::Neutral
            },
            plugin.protocol_status()?
        );
        assert_eq!(
            ProtocolStatus {
                state: STARTING_STATE_NAME.to_owned(),
                health: NodeHealth::Neutral
            },
            plugin.protocol_status()?
        );
        assert_eq!(
            ProtocolStatus {
                state: UPLOADING_STATE_NAME.to_owned(),
                health: NodeHealth::Neutral
            },
            plugin.protocol_status()?
        );
        assert_eq!(
            ProtocolStatus {
                state: "delinquent".to_string(),
                health: NodeHealth::Unhealthy
            },
            plugin.protocol_status()?
        );
        Ok(())
    }

    #[test]
    fn test_run_actions_without_jobs() -> Result<()> {
        let mut babel = MockBabelEngine::new();
        babel.expect_load_config().returning(|| bail!("no config"));
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
        let plugin = RhaiPlugin::from_str("", babel)?;
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
            fn plugin_config() {#{
                services: [
                    #{
                        name: "protocol_service",
                        run_sh: `echo A`,
                        run_as: "some_user",
                    },
                    #{
                        name: "non_protocol_service",
                        run_sh: `echo B`,
                        use_protocol_data: false,
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
                        use_protocol_data: true,
                    }
                ],
            }}
            fn init() {}
            "#;
        let mut babel = MockBabelEngine::new();
        babel.expect_load_config().returning(|| bail!("no config"));
        babel.expect_save_config().once().returning(|_| Ok(()));
        babel.expect_node_env().returning(Default::default);
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
            .with(predicate::eq("protocol_service"))
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
                    log_timestamp: None,
                    use_protocol_data: None,
                    one_time: None,
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
                    log_timestamp: None,
                    use_protocol_data: Some(true),
                    one_time: None,
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
                predicate::eq("protocol_service"),
                predicate::eq(plugin_config::build_service_job_config(
                    Service {
                        name: "protocol_service".to_string(),
                        run_sh: "echo A".to_string(),
                        restart_config: None,
                        shutdown_timeout_secs: None,
                        shutdown_signal: None,
                        run_as: Some("some_user".to_string()),
                        use_protocol_data: true,
                        log_buffer_capacity_mb: None,
                        log_timestamp: None,
                    },
                    vec![],
                    vec!["post_upload_job".to_string()],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("protocol_service"))
            .once()
            .returning(|_| Ok(()));

        let mut plugin = RhaiPlugin::from_str(script, babel)?;
        plugin.init()?;
        assert!(plugin.capabilities().iter().any(|v| v == "upload"));
        plugin.upload().unwrap();
        Ok(())
    }

    #[test]
    fn test_default_init() -> Result<()> {
        let script = r#"
            fn plugin_config() {#{
                config_files: [
                    #{
                        template: "/var/lib/babel/templates/config.template",
                        destination: "/etc/service.config",
                        params: #{
                            node_name: node_env().node_name
                        },
                    },
                    #{
                        template: "/var/lib/babel/templates/base_config.template",
                        destination: "/etc/default_service.config",
                        params: #{
                            node_id: node_env().node_id
                        },
                    },
                ],
                aux_services: [
                    #{
                        name: "nginx",
                        run_sh: `nginx with params`,
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
                            one_time: true
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
                        name: "protocol_service",
                        run_sh: `echo A`,
                    },
                    #{
                        name: "non_protocol_service",
                        run_sh: `echo B`,
                        use_protocol_data: false,
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
            }}
            "#;
        let mut babel = MockBabelEngine::new();
        babel.expect_load_config().returning(|| bail!("no config"));
        babel.expect_save_config().once().returning(|_| Ok(()));
        babel.expect_node_env().returning(|| NodeEnv {
            node_id: "node-id".to_string(),
            node_name: "node name".to_string(),
            ..Default::default()
        });
        babel.expect_stop_all_jobs().once().returning(|| Ok(()));
        babel
            .expect_render_template()
            .with(
                predicate::eq(PathBuf::from(
                    "/var/lib/babel/templates/base_config.template",
                )),
                predicate::eq(PathBuf::from("/etc/default_service.config")),
                predicate::eq(r#"{"node_id":"node-id"}"#),
            )
            .once()
            .returning(|_, _, _| Ok(()));
        babel
            .expect_render_template()
            .with(
                predicate::eq(PathBuf::from("/var/lib/babel/templates/config.template")),
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
                        use_protocol_data: false,
                        log_buffer_capacity_mb: None,
                        log_timestamp: None,
                    },
                    vec![],
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
                    log_timestamp: None,
                    use_protocol_data: None,
                    one_time: Some(true),
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
            .expect_protocol_data_stamp()
            .once()
            .returning(|| Ok(None));
        babel
            .expect_has_protocol_archive()
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
                        log_timestamp: None,
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
                    log_timestamp: None,
                    use_protocol_data: None,
                    one_time: None,
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
                predicate::eq("protocol_service"),
                predicate::eq(plugin_config::build_service_job_config(
                    Service {
                        name: "protocol_service".to_string(),
                        run_sh: "echo A".to_string(),
                        restart_config: None,
                        shutdown_timeout_secs: None,
                        shutdown_signal: None,
                        run_as: None,
                        use_protocol_data: true,
                        log_buffer_capacity_mb: None,
                        log_timestamp: None,
                    },
                    vec!["post_download_job".to_string()],
                    vec![],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("protocol_service"))
            .once()
            .returning(|_| Ok(()));
        babel
            .expect_create_job()
            .with(
                predicate::eq("non_protocol_service"),
                predicate::eq(plugin_config::build_service_job_config(
                    Service {
                        name: "non_protocol_service".to_string(),
                        run_sh: "echo B".to_string(),
                        restart_config: None,
                        shutdown_timeout_secs: None,
                        shutdown_signal: None,
                        run_as: None,
                        use_protocol_data: false,
                        log_buffer_capacity_mb: None,
                        log_timestamp: None,
                    },
                    vec![],
                    vec![],
                )),
            )
            .once()
            .returning(|_, _| Ok(()));
        babel
            .expect_start_job()
            .with(predicate::eq("non_protocol_service"))
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
        let mut plugin = RhaiPlugin::from_str(script, babel)?;
        plugin.init().unwrap();
        assert!(plugin.capabilities().iter().any(|v| v == "init"));
        Ok(())
    }

    #[test]
    fn test_errors_handling() -> Result<()> {
        let script = r#"
            fn plugin_config() {#{
                services: []
            }}

            fn custom_method_with_exception(param) {
                throw "some Rhai exception";
                ""
            }

            fn custom_failing_method(param) {
                load_data()
            }
        "#;
        let mut babel = MockBabelEngine::new();
        babel.expect_load_config().returning(|| bail!("no config"));
        babel
            .expect_save_config()
            .once()
            .returning(|_| bail!("failed to save plugin config"));
        babel.expect_node_env().returning(Default::default);
        babel
            .expect_load_data()
            .return_once(|| bail!("some Rust error"));
        let mut plugin = RhaiPlugin::from_str(script, babel)?;
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
            "no matching method 'no_method' found",
            format!(
                "{:#}",
                plugin.call_custom_method("no_method", "").unwrap_err()
            )
        );
        assert_eq!(
            "failed to save plugin config",
            format!("{:#}", plugin.reload_plugin_config().unwrap_err())
        );
        Ok(())
    }
}
