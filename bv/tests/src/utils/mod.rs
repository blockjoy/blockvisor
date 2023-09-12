pub mod rbac;
pub mod stub_server;
pub mod test_env;
pub mod token;

use assert_cmd::Command;
use predicates::prelude::*;

pub fn execute_sql(connection_str: &str, query: &str) {
    Command::new("docker")
        .args([
            "compose",
            "exec",
            "-T",
            "database",
            "psql",
            connection_str,
            "-c",
            query,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("INSERT"));
}
