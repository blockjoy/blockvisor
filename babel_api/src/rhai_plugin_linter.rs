use crate::{
    engine::{
        Engine, HttpResponse, JobConfig, JobInfo, JobStatus, JobsInfo, JrpcRequest, NodeEnv,
        RestRequest, ShResponse,
    },
    plugin::Plugin,
    plugin_config::PluginConfig,
    rhai_plugin::{RhaiPlugin, PLUGIN_CONFIG_FN_NAME},
};
use eyre::{anyhow, bail};
use std::collections::HashSet;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tracing::Level;

pub fn check(
    plugin_path: PathBuf,
    node_env: NodeEnv,
    node_properties: HashMap<String, String>,
) -> eyre::Result<()> {
    let mut warnings = vec![];
    let defined_properties = node_properties.keys().cloned().collect::<HashSet<_>>();
    let plugin_dir = plugin_path
        .parent()
        .ok_or(anyhow!("invalid plugin parent dir"))?
        .to_path_buf();
    let mut rhai_plugin = RhaiPlugin::from_file(
        plugin_path,
        LinterEngine {
            node_properties,
            node_env,
        },
    )?;
    rhai_plugin.init()?;
    if rhai_plugin.bare.plugin_config.is_none() {
        warnings.push(format!(
            "Deprecated API used: missing {PLUGIN_CONFIG_FN_NAME} function"
        ));
    }
    warnings.extend(
        find_undefined_properties(&defined_properties, &plugin_dir)?
            .into_iter()
            .map(|property| {
                format!(
                    "{}:{}:{}: '{}' not defined in babel.yaml",
                    property
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    property.pos.line().unwrap_or_default(),
                    property.pos.position().unwrap_or_default(),
                    property.name
                )
            }),
    );
    if warnings.is_empty() {
        Ok(())
    } else {
        bail!("{warnings:?}")
    }
}

#[derive(Eq, PartialEq, Hash)]
struct UndefinedProperty {
    path: PathBuf,
    pos: rhai::Position,
    name: String,
}

fn find_undefined_properties(
    defined_properties: &HashSet<String>,
    plugin_dir: &Path,
) -> eyre::Result<HashSet<UndefinedProperty>> {
    let mut undefined_properties = HashSet::<UndefinedProperty>::new();
    let mut engine = rhai::Engine::new();
    engine.set_max_expr_depths(64, 32);
    for entry in walkdir::WalkDir::new(plugin_dir) {
        let entry = entry?;
        let path = entry.path();
        let mut check_ast_node = |nodes: &[rhai::ASTNode<'_>]| -> bool {
            undefined_properties.extend(find_used_properties(nodes).filter_map(|(pos, name)| {
                if defined_properties.contains(name.as_str()) {
                    None
                } else {
                    Some(UndefinedProperty {
                        path: path.to_path_buf(),
                        pos: pos.to_owned(),
                        name: name.to_string(),
                    })
                }
            }));
            true
        };
        if path.extension().is_some_and(|ext| ext == "rhai") {
            let ast = engine.compile_file(path.to_path_buf())?;
            ast.walk(&mut check_ast_node);
        }
    }
    Ok(undefined_properties)
}

fn find_used_properties<'a>(
    nodes: &'a [rhai::ASTNode<'_>],
) -> impl Iterator<Item = (&'a rhai::Position, &'a rhai::ImmutableString)> + 'a {
    nodes.iter().filter_map(|node| {
        if let rhai::ASTNode::Expr(rhai::Expr::Index(expr, _, _))
        | rhai::ASTNode::Expr(rhai::Expr::Dot(expr, _, _)) = node
        {
            if let rhai::BinaryExpr {
                lhs: rhai::Expr::FnCall(fn_call, _),
                rhs,
            } = expr.as_ref()
            {
                if fn_call.name == "node_params" {
                    if let rhai::Expr::StringConstant(property, pos) = rhs {
                        return Some((pos, property));
                    } else if let rhai::Expr::Property(property, pos) = rhs {
                        return Some((pos, &property.2));
                    }
                }
            }
        }
        None
    })
}

struct LinterEngine {
    node_properties: HashMap<String, String>,
    node_env: NodeEnv,
}

impl Engine for LinterEngine {
    fn create_job(&self, _job_name: &str, _job_config: JobConfig) -> eyre::Result<()> {
        Ok(())
    }

    fn start_job(&self, _job_name: &str) -> eyre::Result<()> {
        Ok(())
    }

    fn stop_job(&self, _job_name: &str) -> eyre::Result<()> {
        Ok(())
    }

    fn stop_all_jobs(&self) -> eyre::Result<()> {
        Ok(())
    }

    fn cleanup_job(&self, _job_name: &str) -> eyre::Result<()> {
        Ok(())
    }

    fn job_info(&self, _job_name: &str) -> eyre::Result<JobInfo> {
        Ok(JobInfo {
            status: JobStatus::Pending {
                waiting_for: vec![],
            },
            timestamp: SystemTime::now(),
            progress: None,
            restart_count: 0,
            logs: vec![],
            upgrade_blocking: false,
        })
    }

    fn get_jobs(&self) -> eyre::Result<JobsInfo> {
        Ok(HashMap::from_iter([(
            "dummy_job".to_string(),
            JobInfo {
                status: JobStatus::Pending {
                    waiting_for: vec![],
                },
                timestamp: SystemTime::now(),
                progress: None,
                restart_count: 0,
                logs: vec![],
                upgrade_blocking: false,
            },
        )]))
    }

    fn run_jrpc(
        &self,
        _req: JrpcRequest,
        _timeout: Option<Duration>,
    ) -> eyre::Result<HttpResponse> {
        Ok(HttpResponse {
            status_code: 0,
            body: "".to_string(),
        })
    }

    fn run_rest(
        &self,
        _req: RestRequest,
        _timeout: Option<Duration>,
    ) -> eyre::Result<HttpResponse> {
        Ok(HttpResponse {
            status_code: 0,
            body: "".to_string(),
        })
    }

    fn run_sh(&self, _body: &str, _timeout: Option<Duration>) -> eyre::Result<ShResponse> {
        Ok(ShResponse {
            exit_code: 0,
            stdout: "".to_string(),
            stderr: "".to_string(),
        })
    }

    fn sanitize_sh_param(&self, param: &str) -> eyre::Result<String> {
        Ok(param.to_string())
    }

    fn render_template(
        &self,
        _template: &Path,
        _destination: &Path,
        _params: &str,
    ) -> eyre::Result<()> {
        Ok(())
    }

    fn node_params(&self) -> HashMap<String, String> {
        self.node_properties.clone()
    }

    fn node_env(&self) -> NodeEnv {
        self.node_env.clone()
    }

    fn save_data(&self, _value: &str) -> eyre::Result<()> {
        Ok(())
    }

    fn load_data(&self) -> eyre::Result<String> {
        Ok("".to_string())
    }

    fn save_config(&self, _value: &PluginConfig) -> eyre::Result<()> {
        Ok(())
    }

    fn load_config(&self) -> eyre::Result<PluginConfig> {
        Ok(Default::default())
    }

    fn log(&self, _level: Level, _message: &str) {}

    fn add_task(
        &self,
        _task_name: &str,
        _schedule: &str,
        _function_name: &str,
        _function_param: &str,
    ) -> eyre::Result<()> {
        Ok(())
    }

    fn delete_task(&self, _task_name: &str) -> eyre::Result<()> {
        Ok(())
    }

    fn protocol_data_stamp(&self) -> eyre::Result<Option<SystemTime>> {
        Ok(None)
    }

    fn has_protocol_archive(&self) -> eyre::Result<bool> {
        Ok(true)
    }

    fn get_secret(&self, _name: &str) -> eyre::Result<Option<Vec<u8>>> {
        Ok(Some("secret value".as_bytes().to_vec()))
    }

    fn put_secret(&self, _name: &str, _value: Vec<u8>) -> eyre::Result<()> {
        Ok(())
    }

    fn file_read(&self, _path: &Path) -> eyre::Result<Vec<u8>> {
        Ok(Default::default())
    }

    fn file_write(&self, _path: &Path, _value: Vec<u8>) -> eyre::Result<()> {
        Ok(())
    }
}
