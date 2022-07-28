//! Build file generating gRPC stubs

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        // needed for integration tests
        .build_client(true)
        .compile(
            &[
                "../blockjoy/api/v1/command_flow.proto",
                "../blockjoy/api/v1/host.proto",
                "../blockjoy/api/v1/host_service.proto",
                "../blockjoy/api/v1/messages.proto",
                "../blockjoy/api/v1/node.proto",
            ],
            &["../blockjoy/api/v1"],
        )?;

    Ok(())
}
