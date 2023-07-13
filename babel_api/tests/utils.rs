use babel_api::engine::{
    Engine, HttpResponse, JobConfig, JobStatus, JrpcRequest, RestRequest, ShResponse,
};
use mockall::*;
use std::{collections::HashMap, path::Path, time::Duration};

mock! {
    pub BabelEngine {}

    impl Engine for BabelEngine {
        fn start_job(&self, job_name: &str, job_config: JobConfig) -> anyhow::Result<()>;
        fn stop_job(&self, job_name: &str) -> anyhow::Result<()>;
        fn job_status(&self, job_name: &str) -> anyhow::Result<JobStatus>;
        fn run_jrpc(&self, req: JrpcRequest, timeout: Option<Duration>) -> anyhow::Result<HttpResponse>;
        fn run_rest(&self, req: RestRequest, timeout: Option<Duration>) -> anyhow::Result<HttpResponse>;
        fn run_sh(&self, body: &str, timeout: Option<Duration>) -> anyhow::Result<ShResponse>;
        fn sanitize_sh_param(&self, param: &str) -> anyhow::Result<String>;
        fn render_template(
            &self,
            template: &Path,
            output: &Path,
            params: &str,
        ) -> anyhow::Result<()>;
        fn node_params(&self) -> HashMap<String, String>;
        fn save_data(&self, value: &str) -> anyhow::Result<()>;
        fn load_data(&self) -> anyhow::Result<String>;
        fn log(&self, level: tracing::log::Level, message: &str);
    }
}
