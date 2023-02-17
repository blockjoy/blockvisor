fn main() {
    if let Err(e) = tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(
            &[
                // Backend API
                "command.proto",
                "host_service.proto",
                "key_file_service.proto",
                "node_service.proto",
                // Cookbook API
                "cookbook.proto",
                // UI API (used in the tests)
                "authentication_service.proto",
                "billing_service.proto",
                "blockchain_service.proto",
                "command_service.proto",
                "dashboard_service.proto",
                "host_provision_service.proto",
                "fe_host_service.proto",
                "ui_node_service.proto",
                "organization_service.proto",
                "user_service.proto",
                // Internal API
                "blockvisor_service.proto",
                "metrics.proto",
            ],
            &[
                "proto/blockjoy/api/v1",
                "proto/blockjoy/api/ui_v1",
                "data/proto/blockjoy/blockvisor/v1",
                "data/proto/blockjoy/api/v1/babel",
            ],
        )
    {
        eprintln!("Building protos failed with:\n{e}");
        std::process::exit(1);
    }
}
