pub mod client {
    // 入口 proto 负责 import 各个子文件，这里只保留稳定的模块名给 Rust 代码引用。
    // 由 build.rs 统一从 proto/client_server.proto 生成。
    tonic::include_proto!("gb_mahjong.client.v1");
}

pub mod engine {
    // 规则引擎协议同理，目录结构变化不会影响业务代码中的 use 路径。
    // 由 build.rs 统一从 proto/internal_engine.proto 生成。
    tonic::include_proto!("gb_mahjong.engine.v1");
}
