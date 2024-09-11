use crate::engine::JobStatus;
use crate::rhai_plugin::PLUGIN_CONFIG_CONST_NAME;
use crate::{
    engine::{
        Engine, HttpResponse, JobConfig, JobInfo, JobsInfo, JrpcRequest, NodeEnv, RestRequest,
        ShResponse,
    },
    rhai_plugin::RhaiPlugin,
};
use eyre::bail;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tracing::Level;

pub fn check(script: &str, node_properties: HashMap<String, String>) -> eyre::Result<()> {
    let mut warnings = vec![];
    let rhai_plugin = RhaiPlugin::new(script, LinterEngine { node_properties })?;
    if rhai_plugin.bare.plugin_config.is_none() {
        warnings.push(format!(
            "Deprecated API used: missing {PLUGIN_CONFIG_CONST_NAME}"
        ));
    }
    // TODO define more checks
    if warnings.is_empty() {
        Ok(())
    } else {
        bail!("{warnings:?}")
    }
}

struct LinterEngine {
    node_properties: HashMap<String, String>,
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

    fn job_info(&self, _job_name: &str) -> eyre::Result<JobInfo> {
        Ok(JobInfo {
            status: JobStatus::Pending,
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
                status: JobStatus::Pending,
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
        Default::default()
    }

    fn save_data(&self, _value: &str) -> eyre::Result<()> {
        Ok(())
    }

    fn load_data(&self) -> eyre::Result<String> {
        Ok("".to_string())
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

    fn is_download_completed(&self) -> eyre::Result<bool> {
        Ok(false)
    }

    fn has_blockchain_archive(&self) -> eyre::Result<bool> {
        Ok(true)
    }

    fn get_secret(&self, _name: &str) -> eyre::Result<Option<Vec<u8>>> {
        Ok(Default::default())
    }

    fn put_secret(&self, _name: &str, _value: &[u8]) -> eyre::Result<()> {
        Ok(())
    }
}
