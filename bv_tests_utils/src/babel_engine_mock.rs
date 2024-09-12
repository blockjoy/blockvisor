use babel_api::engine::{
    Engine, HttpResponse, JobConfig, JobInfo, JobsInfo, JrpcRequest, NodeEnv, RestRequest,
    ShResponse,
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
            destination: &Path,
            params: &str,
        ) -> Result<()>;
        fn node_params(&self) -> HashMap<String, String>;
        fn node_env(&self) -> NodeEnv;
        fn save_data(&self, value: &str) -> Result<()>;
        fn load_data(&self) -> Result<String>;
        fn log(&self, level: tracing::Level, message: &str);
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
        fn get_secret(&self, name: &str) -> Result<Option<Vec<u8>>>;
        fn put_secret(&self, name: &str, value: Vec<u8>) -> Result<()>;
        fn file_read(&self, path: &Path) -> Result<Vec<u8>>;
        fn file_write(&self, path: &Path, content: Vec<u8>) -> Result<()>;
    }
}
