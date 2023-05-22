use crate::src::utils::{test_env, test_env::TestEnv};
use anyhow::Result;
use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::{config::Config, node_data::NodeImage, services::cookbook::CookbookService};
use bv_utils::run_flag::RunFlag;
use predicates::prelude::*;
use std::path::Path;

#[test]
fn test_bvup_unknown_provision_token() {
    let tmp_dir = TempDir::new().unwrap();

    let provision_token = "NOT_FOUND";
    let (ifa, _ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let url = "http://localhost:8080";
    let mqtt = "mqtt://localhost:1883";

    let mut cmd = Command::cargo_bin("bvup").unwrap();
    cmd.args([provision_token, "--skip-download"])
        .args(["--ifa", ifa])
        .args(["--api", url])
        .args(["--keys", url])
        .args(["--registry", url])
        .args(["--mqtt", mqtt])
        .env("BV_ROOT", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Record not found"));
}

#[tokio::test]
async fn test_bv_chain_list_form_cookbook() {
    test_env::bv_run(&["chain", "list", "testing", "validator"], "", None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn test_bv_cmd_cookbook_download() -> Result<()> {
    let mut test_env = TestEnv::new_with_api_config(Config::load(Path::new("/")).await?).await?;
    test_env.run_blockvisord(RunFlag::default()).await?;

    let folder = CookbookService::get_image_download_folder_path(
        &test_env.bv_root,
        &NodeImage {
            protocol: "testing".to_string(),
            node_type: "validator".to_string(),
            node_version: "0.0.3".to_string(),
        },
    );
    let _ = tokio::fs::remove_dir_all(&folder).await;

    println!("create a node");
    let (vm_id, _) = test_env.create_node("testing/validator/0.0.3", "216.18.214.195");
    println!("create vm_id: {vm_id}");

    println!("delete node");
    test_env.bv_run(&["node", "delete", &vm_id], "Deleted node");

    assert!(Path::new(&folder.join("kernel")).exists());
    assert!(Path::new(&folder.join("os.img")).exists());
    assert!(Path::new(&folder.join("babel.rhai")).exists());
    Ok(())
}
