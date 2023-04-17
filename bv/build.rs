fn main() {
    if let Err(e) = tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(
            &[
                // Cookbook API
                "cookbook.proto",
                // Blockjoy API
                "v1/authentication.proto",
                "v1/billing.proto",
                "v1/blockchain.proto",
                "v1/command.proto",
                "v1/discovery.proto",
                "v1/host_provision.proto",
                "v1/host.proto",
                "v1/invitation.proto",
                "v1/key_file.proto",
                "v1/metrics.proto",
                "v1/mqtt.proto",
                "v1/node.proto",
                "v1/organization.proto",
                "v1/user.proto",
                // Internal API
                "blockvisor_service.proto",
            ],
            &[
                "proto/public",
                "data/proto/blockjoy/blockvisor/v1",
                "data/proto/blockjoy/api/v1/babel",
            ],
        )
    {
        eprintln!("Building protos failed with:\n{e}");
        std::process::exit(1);
    }
}
