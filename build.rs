fn main() {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile(
            &[
                "proto/blockjoy/api/v1/command_flow.proto",
                "proto/blockjoy/api/v1/command.proto",
                "proto/blockjoy/api/v1/host_service.proto",
                "proto/blockjoy/api/v1/host.proto",
                "proto/blockjoy/api/v1/messages.proto",
                "proto/blockjoy/api/v1/node_types.proto",
                "proto/blockjoy/api/v1/node.proto",
            ],
            &["proto/blockjoy/api/v1"],
        )
        .unwrap();
}
