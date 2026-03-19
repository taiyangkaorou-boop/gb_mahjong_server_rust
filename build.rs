use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // build.rs 只负责协议代码生成，不参与运行时逻辑。
    // 这样协议目录怎么拆分，业务模块都只需要依赖稳定的 include_proto! 入口。
    // 仍然以两个根入口文件作为编译入口，
    // 这样 Rust 生成代码对外暴露的 package 和 include_proto! 路径都保持稳定。
    let proto_files = ["proto/client_server.proto", "proto/internal_engine.proto"];
    // 子 proto 变更也要触发重新生成代码，否则目录拆分后 Cargo 不会自动感知 import 变化。
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

    // include path 固定指向 proto 根目录，让 import public 的目录重构保持透明。
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&proto_files, &["proto"])?;

    Ok(())
}
