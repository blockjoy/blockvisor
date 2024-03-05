use babel_api::engine::{
    Engine, HttpResponse, JobConfig, JobInfo, JobsInfo, JrpcRequest, RestRequest, ShResponse,
};
use eyre::Result;
use mockall::*;
use std::{collections::HashMap, path::Path, time::Duration};

mock! {
    pub BabelEngine {}

    impl Engine for BabelEngine {
        fn create_job(&self, job_name: &str, job_config: JobConfig) -> Result<()>;
        fn start_job(&self, job_name: &str) -> Result<()>;
        fn stop_job(&self, job_name: &str) -> Result<()>;
        fn job_info(&self, job_name: &str) -> Result<JobInfo>;
        fn get_jobs(&self) -> Result<JobsInfo>;
        fn run_jrpc(&self, req: JrpcRequest, timeout: Option<Duration>) -> Result<HttpResponse>;
        fn run_rest(&self, req: RestRequest, timeout: Option<Duration>) -> Result<HttpResponse>;
        fn run_sh(&self, body: &str, timeout: Option<Duration>) -> Result<ShResponse>;
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
        fn log(&self, level: tracing::Level, message: &str);
    }
}
