fn main() {
    if let Err(e) = tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(
            &[
                // Cookbook API
                "cookbook.proto",
                // Blockjoy API
                "blockjoy/v1/auth.proto",
                "blockjoy/v1/blockchain.proto",
                "blockjoy/v1/command.proto",
                "blockjoy/v1/discovery.proto",
                "blockjoy/v1/host_provision.proto",
                "blockjoy/v1/host.proto",
                "blockjoy/v1/key_file.proto",
                "blockjoy/v1/metrics.proto",
                "blockjoy/v1/mqtt.proto",
                "blockjoy/v1/user.proto",
                // Internal API
                "blockvisor_service.proto",
            ],
            &[
                "proto/",
                "data/proto/blockjoy/blockvisor/v1",
                "data/proto/blockjoy/api/v1/babel",
            ],
        )
    {
        eprintln!("Building protos failed with:\n{e}");
        std::process::exit(1);
    }
}
