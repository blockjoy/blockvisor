use assert_cmd::Command;
use assert_fs::TempDir;
use blockvisord::hosts::get_host_info;
use httpmock::{Method::POST, MockServer};
use serde_json::json;

#[test]
#[ignore]
fn test_cmd_bv_start_no_init() {
    let tmp_dir = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("start")
        .env("HOME", tmp_dir.as_os_str())
        .assert()
        .failure()
        .stderr("Error: Host is not registered, please run `init` first\n");
}

#[tokio::test]
#[ignore]
async fn test_cmd_bv_init_mock_api() {
    let tmp_dir = TempDir::new().unwrap();
    let mock_server = MockServer::start();

    // inputs
    let otp = "OTP";
    let (ifa, ip) = &local_ip_address::list_afinet_netifas().unwrap()[0];
    let ip = ip.to_string();
    let info = get_host_info();
    let name = info.name.unwrap();
    let version = env!("CARGO_PKG_VERSION").to_string();

    // outputs
    let secret_token = "secret_token";
    let host_id = "eb4e20fc-2b4a-4d0c-811f-48abcf12b89b";

    mock_server.mock(|when, then| {
        when.method(POST)
            .path(format!("/host_provisions/{otp}/hosts"))
            .header("Content-Type", "application/json")
            .json_body(json!({
                "org_id": null,
                "name": name,
                "version": version,
                "location": null,
                "cpu_count": info.cpu_count,
                "mem_size": info.mem_size,
                "disk_size": info.disk_size,
                "os": info.os,
                "os_version": info.os_version,
                "ip_addr": ip,
                "val_ip_addrs": null,
            }));
        then.status(200)
            .header("Content-Type", "application/json")
            .json_body(json!({
                "id": host_id,
                "token": secret_token,
                "org_id": null,
                "name": name,
                "version": version,
                "location": null,
                "cpu_count": info.cpu_count,
                "mem_size": info.mem_size,
                "disk_size": info.disk_size,
                "os": info.os,
                "os_version": info.os_version,
                "ip_addr": ip,
                "val_ip_addrs": null,
                "created_at": "2019-08-24T14:15:22Z",
                "status": "online",
                "validators": [],
            }));
    });
    let url = &mock_server.base_url();

    let mut cmd = Command::cargo_bin("bv").unwrap();
    cmd.arg("init")
        .args(&["--otp", otp])
        .args(&["--ifa", ifa])
        .args(&["--url", url])
        .env("HOME", tmp_dir.as_os_str())
        .assert()
        .success();
}
