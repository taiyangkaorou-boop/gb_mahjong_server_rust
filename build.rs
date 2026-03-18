use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let proto_files = ["proto/client_server.proto", "proto/internal_engine.proto"];

    for proto_file in proto_files {
        println!("cargo:rerun-if-changed={proto_file}");
    }

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&proto_files, &["proto"])?;

    Ok(())
}
