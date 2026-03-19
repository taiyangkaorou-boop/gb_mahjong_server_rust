use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let proto_files = ["proto/client_server.proto", "proto/internal_engine.proto"];
    let tracked_proto_files = [
        "proto/client_server.proto",
        "proto/client/common/enums.proto",
        "proto/client/common/types.proto",
        "proto/client/actions.proto",
        "proto/client/events.proto",
        "proto/client/settlement.proto",
        "proto/client/session.proto",
        "proto/internal_engine.proto",
        "proto/engine/common/enums.proto",
        "proto/engine/common/types.proto",
        "proto/engine/validation.proto",
        "proto/engine/scoring.proto",
        "proto/engine/service.proto",
    ];

    for proto_file in tracked_proto_files {
        println!("cargo:rerun-if-changed={proto_file}");
    }

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&proto_files, &["proto"])?;

    Ok(())
}
