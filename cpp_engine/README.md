# 国标麻将 C++ 规则引擎骨架

这个目录包含 Task 6 对应的 C++ gRPC 规则引擎微服务骨架。它的职责是作为
Rust 房间服务后的无状态计算节点，接收 `ValidateAction` 和 `CalculateScore`
请求，并在后续接入 `zheng-fan/GB-Mahjong` 后完成动作合法性校验与算番结算。

## 目录结构

- `CMakeLists.txt`：从 `../proto/internal_engine.proto` 生成 C++ Protobuf 与 gRPC 代码，并定义构建目标
- `src/main.cc`：同步 gRPC Server 启动入口
- `src/rule_engine_service.*`：gRPC 传输层，实现 `RuleEngineService`
- `src/gb_mahjong_adapter.*`：协议对象与底层麻将库之间的适配层，后续接入 `zheng-fan/GB-Mahjong` 的主要位置
- `scripts/install_deps_ubuntu.sh`：Ubuntu 依赖安装脚本

## 构建依赖

这个子项目需要预先安装 CMake、Protobuf、gRPC C++ 开发包，以及对应的
`protoc` / `grpc_cpp_plugin` 工具。

Ubuntu 推荐直接安装以下包：

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  cmake \
  pkg-config \
  libprotobuf-dev \
  protobuf-compiler \
  libgrpc++-dev \
  protobuf-compiler-grpc
```

也可以直接使用仓库内脚本：

```bash
bash cpp_engine/scripts/install_deps_ubuntu.sh
```

## 编译

```bash
cmake -S cpp_engine -B cpp_engine/build
cmake --build cpp_engine/build -j
```

生成的可执行文件为：

```bash
./cpp_engine/build/gb_mahjong_rule_engine_server
```

## 运行

默认监听地址为 `0.0.0.0:50051`，可以通过环境变量覆盖：

```bash
GB_MAHJONG_ENGINE_HOST=0.0.0.0 \
GB_MAHJONG_ENGINE_PORT=50051 \
./cpp_engine/build/gb_mahjong_rule_engine_server
```

## 当前实现状态

当前骨架已经具备以下能力：

- 能启动 gRPC Server 并注册 `RuleEngineService`
- 能接收 `ValidateAction` / `CalculateScore` 请求
- 能完成基础请求形状校验
- 已将 `GB-Mahjong` 上游源码 vendoring 到 `third_party/GB-Mahjong`
- `ValidateAction` 中的和牌路径已经接入 `GB-Mahjong::JudgeHu / CountFan`
- `CalculateScore` 已经接入 `GB-Mahjong::CountFan` 生成真实番数与番种明细
- 已将“协议层”和“麻将规则实现层”拆开，便于后续替换成真实算番逻辑

当前还没有完成的部分：

- 吃、碰、杠这类一般动作仍然没有直接调用上游库做窗口合法性校验
- 抢操作优先级、并发裁决、过手胡等仍然应由 Rust 房间状态机处理
- `CalculateScore` 的分数增减公式目前仍是基于 `base_points * total_fan` 的服务端占位实现
- Rust 侧传给 C++ 的上下文目前还不够完整，后续要补齐真实手牌、副露、弃牌与抢操作窗口信息

## 后续接入建议

建议下一步按下面顺序继续：

1. 在 `GbMahjongAdapter` 中把 `ValidateActionRequest` / `CalculateScoreRequest`
   转换为 `zheng-fan/GB-Mahjong` 所需的数据结构。
2. 调用真实的合法性校验与番型计算逻辑。
3. 将底层结果映射回 `ValidateActionResponse` / `CalculateScoreResponse`。
4. 为关键牌型、抢杠胡、杠上开花、自摸、点和等路径补充集成测试。
