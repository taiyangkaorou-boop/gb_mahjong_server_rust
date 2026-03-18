# 国标麻将 Web 游戏系统

本项目是一款支持 4 人实时对战的国标麻将（GB Mahjong）Web 游戏，采用服务端权威架构。

- 前端：现代 Web 前端，通过 WebSocket 收发 Protobuf 二进制帧
- 游戏服务：Rust（`axum + tokio + tonic`）
- 规则引擎：C++ gRPC 服务，封装 `zheng-fan/GB-Mahjong`
- 数据库：PostgreSQL，基于 `JSONB` 做 Event Sourcing

## 当前状态

当前仓库已经完成并落地以下内容：

- Task 1：`client_server.proto` 与 `internal_engine.proto`
- Task 2：`users / matches / match_events` 的 PostgreSQL DDL
- Task 3：Rust 服务工程初始化、`axum` 路由、WebSocket Protobuf 编解码
- Task 4：`RoomManager + tokio::spawn + mpsc` 的房间串行事件循环骨架
- Task 5：Rust 侧 `tonic` gRPC Client 接入
- Task 6：C++ gRPC Server skeleton 与 `GB-Mahjong` 初步集成

当前代码已经推到了以“骨架可运行”为目标的阶段：

- Rust 单测可运行
- C++ 规则引擎可构建
- GitHub SSH 推送链路已打通

## 阶段计划总览

### Phase 1：协议定义

目标：统一 C/S 与 Rust/C++ 之间的消息模型。

已完成内容：

- 定义客户端协议 `proto/client_server.proto`
- 定义内部规则引擎协议 `proto/internal_engine.proto`
- 统一座位、牌、动作、结算、同步快照等核心消息

### Phase 2：数据存储与回放基础

目标：为对局记录、回放、审计提供事件溯源基础。

已完成内容：

- 设计 `users`
- 设计 `matches`
- 设计 `match_events`
- 使用 `JSONB` 存储事件载荷，便于回放和协议映射

### Phase 3：Rust 服务初始化

目标：搭起 Rust 服务入口与 WebSocket 网关。

已完成内容：

- 配置 `Cargo.toml`
- 配置 `build.rs` 生成 Protobuf 代码
- 建立 `axum` 路由
- 实现 WebSocket 二进制帧读取与 Protobuf 解码
- 完成基础健康检查与单测

### Phase 4：房间并发核心

目标：用房间独占任务模型解决麻将抢操作并发问题。

已完成内容：

- 每个房间对应一个独立 `tokio::spawn` 任务
- 通过 `mpsc` 将请求送入房间循环
- 房间状态不通过 `Arc<RwLock<RoomState>>` 跨线程共享
- WebSocket 网关与房间任务已经接通

### Phase 5：规则引擎调用链

目标：让 Rust 房间任务能异步调用 C++ 规则引擎。

已完成内容：

- 实现 `RuleEngineHandle`
- 接通 `ValidateAction`
- 接通 `CalculateScore` 调用骨架
- Rust 侧 mock 能覆盖 gRPC 调用路径

### Phase 6：C++ 规则引擎服务

目标：把 `GB-Mahjong` 包装成可独立部署的无状态微服务。

已完成内容：

- 实现 C++ gRPC Server skeleton
- 实现 `RuleEngineService`
- 接入 `GB-Mahjong` 上游源码
- 支持和牌判定与番型计算的初步路径

当前边界：

- 吃、碰、杠等一般动作的完整合法性窗口裁决仍由 Rust 房间状态机负责
- C++ 引擎目前更适合处理和牌与算番，不负责并发抢操作优先级

### Phase 7：规则闭环与单手牌核心流

目标：把当前“骨架可运行”推进到“单手牌核心流程可闭环”。

这是当前阶段的主任务，尚未完全实现。

#### 7.1 房间运行时状态升级

需要补齐：

- `RoundRuntimeState`
- `PlayerRoundState`
- `ActiveClaimWindowState`
- `ResolvedActionRecord`

需要让房间状态真实持有：

- 牌墙
- 死牌墙
- 玩家暗手
- 摸牌态
- 副露
- 弃牌河
- 花牌
- 当前抢操作窗口
- 最近一次已落地动作

#### 7.2 单手牌生命周期打通

需要实现：

- 发牌
- 庄家起手多摸一张
- 补花 / 补牌
- 摸牌
- 打牌
- 吃
- 碰
- 明杠
- 补杠
- 自摸胡
- 点和
- 单手牌结算

当前范围只覆盖“完整走完一手牌 round”，不做整场东南圈推进。

#### 7.3 Rust -> C++ 真实规则载荷补齐

当前最大的真实性缺口在这里。

需要让 `ValidateActionRequest` 与 `CalculateScoreRequest` 真正带上：

- 玩家公开状态
- 玩家暗手
- 最近动作
- 抢操作窗口上下文
- 终局手牌快照
- 赢家 / 放铳者 / 和牌牌张

当前仓库里这部分仍存在占位数据，因此它是下一阶段最优先的工程任务。

#### 7.4 抢操作窗口与裁决规则

当前已确定的服务端裁决规则如下：

- 优先级：`胡 > 杠/碰 > 吃`
- 吃仅允许下家
- 同优先级按离来源座位最近者优先
- 本阶段不做一炮多响
- 不采用“先到先得”

Rust 房间任务需要在同一窗口内缓存响应，并统一裁决。

#### 7.5 客户端同步语义做实

需要保证：

- `SyncState` 对自己展示完整暗手
- 对他人只展示张数、副露、弃牌、花牌
- `ActionBroadcast` 在摸牌时按接收者隐藏牌张
- `ActionPrompt` 只发给有资格响应的座位
- 重连后可完整恢复自己的对局视图

## 下一步开发重点

下一步建议严格按下面顺序推进：

1. 重构 `src/room.rs`，补齐真实单手牌运行时状态
2. 让 `ValidateActionRequest` 从真实房间状态构造，不再发送空集合占位
3. 实现打牌后抢操作窗口的开启、缓存、裁决与关闭
4. 接通 `CalculateScore` 到 `RoundSettlement`
5. 为发牌、打牌、抢碰/吃、胡牌结算补单测

## 代码结构

### Rust 服务

- `src/main.rs`：服务入口
- `src/app.rs`：应用状态与路由装配
- `src/ws.rs`：WebSocket 网关
- `src/room.rs`：房间任务与状态机核心
- `src/engine.rs`：规则引擎客户端封装
- `src/proto.rs`：Protobuf 模块导出

### 协议

- `proto/client_server.proto`
- `proto/internal_engine.proto`

### 数据库

- `sql/001_init_schema.sql`

### C++ 规则引擎

- `cpp_engine/src/main.cc`
- `cpp_engine/src/rule_engine_service.*`
- `cpp_engine/src/gb_mahjong_adapter.*`

## 构建与验证

### Rust

```bash
cargo test
```

### C++

```bash
cmake -S cpp_engine -B /tmp/gb_mahjong_cpp_engine_build
cmake --build /tmp/gb_mahjong_cpp_engine_build -j
```

## 说明

当前仓库已经具备“基础架构可运行、协议已固定、规则引擎已可构建”的基础，但还没有进入“完整单手牌对局闭环”的最终状态。

因此，这个 README 既是当前实现说明，也是后续开发计划书。
