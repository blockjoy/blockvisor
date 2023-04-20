fn main() {
    if let Err(e) = tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(
            &[
                // Cookbook API
                "cookbook.proto",
                // Blockjoy API
                "authentication.proto",
                "blockchain.proto",
                "command.proto",
                "discovery.proto",
                "host_provision.proto",
                "host.proto",
                "key_file.proto",
                "metrics.proto",
                "mqtt.proto",
                // Internal API
                "blockvisor_service.proto",
            ],
            &[
                "proto/v1",
                "data/proto/blockjoy/blockvisor/v1",
                "data/proto/blockjoy/api/v1/babel",
            ],
        )
    {
        eprintln!("Building protos failed with:\n{e}");
        std::process::exit(1);
    }
}
