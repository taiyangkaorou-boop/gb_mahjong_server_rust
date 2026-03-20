use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    time::{sleep, Duration},
};
use tracing::{debug, info, warn};

use crate::{
    db::{MatchEventWriter, NewMatchEventRecord},
    engine::RuleEngineHandle,
    proto::{
        client::{
            player_action_request, server_frame, ActionBroadcast, ActionDetail, ActionPrompt,
            ActionRejected, ClaimWindow, DiscardAdded, DrawDetail, FanDetail, FinalStanding,
            GamePhase, GameSnapshot, JoinRoomRequest, JoinRoomResponse, MatchSettlement, Meld,
            MeldAdded, PlayerActionRequest, PlayerConnectionChanged, PlayerState, PlayerStatus,
            PromptOption, ReadyRequest, RejectCode, ResolvedClaim, ResultingEffect,
            ResumeSessionRequest, RevealedHand, RoomConfig, RoundPlayerResult, RoundSettlement,
            Seat, SelfHandState, ServerFrame, SettlementFlag, SyncReason, SyncState, Tile,
            TurnAdvanced,
        },
        engine::{self as engine_proto, candidate_action},
    },
};

const ROOM_COMMAND_BUFFER: usize = 256;
const RECONNECT_GRACE_MS: u64 = 30_000;
const INITIAL_SCORE: i64 = 25_000;
const DEAD_WALL_TILE_COUNT: usize = 14;
const MATCH_TOTAL_HANDS: u32 = 4;
const SEAT_ORDER: [Seat; 4] = [Seat::East, Seat::South, Seat::West, Seat::North];

// 牌墙生成器允许测试注入固定顺序，便于稳定复现摸牌和抢操作场景。
type WallFactory = Arc<dyn Fn(&RoomConfig, &str, u32) -> Vec<Tile> + Send + Sync>;

pub type ConnectionSender = mpsc::UnboundedSender<ServerFrame>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomRuntimeMode {
    // Production 模式走完整的规则校验、广播和事件持久化路径。
    Production,
    // LoadTest 模式只保留房间状态机本体，用于进程内压测。
    LoadTest,
}

impl RoomRuntimeMode {
    fn emits_network_frames(self) -> bool {
        matches!(self, Self::Production)
    }

    fn persists_events(self) -> bool {
        matches!(self, Self::Production)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoadTestWinnerPolicy {
    Random,
}

#[derive(Clone, Debug)]
pub(crate) struct LoadTestRoomSpec {
    // 压测房间需要带上稳定标识，便于汇总结果和按 seed 复现问题。
    pub(crate) room_id: String,
    pub(crate) room_index: u64,
    pub(crate) player_base_index: u64,
    // 每手牌最多随机执行多少次出牌，超出后强制随机指定赢家收口。
    pub(crate) max_actions: u32,
    // 当前压测房间要完整跑多少手。东风场默认就是 4 手。
    pub(crate) hands_per_match: u32,
    pub(crate) seed: u64,
    pub(crate) winner_policy: LoadTestWinnerPolicy,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoadTestRoomResult {
    pub room_id: String,
    pub room_index: u64,
    pub success: bool,
    pub actions_executed: u32,
    pub hands_completed: u32,
    pub winner_seat: Option<String>,
    pub failure_reason: Option<String>,
}

#[derive(Clone)]
pub struct AuthorizedJoin {
    // JoinRoom 不再信任客户端自报 user_id/display_name，
    // 而是由网关在鉴权和大厅授权后下发一个服务端确认过的入房身份。
    pub user_id: String,
    pub display_name: String,
    pub seat: Seat,
}

#[derive(Clone)]
pub struct ConnectionHandle {
    connection_id: String,
    outbound_tx: ConnectionSender,
}

impl ConnectionHandle {
    pub fn new(connection_id: String, outbound_tx: ConnectionSender) -> Self {
        Self {
            connection_id,
            outbound_tx,
        }
    }

    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    pub fn send_frame(&self, frame: ServerFrame) {
        // 连接失效时这里只记日志，不让单个连接问题拖垮房间任务。
        if let Err(error) = self.outbound_tx.send(frame) {
            warn!(
                connection_id = %self.connection_id,
                ?error,
                "failed to enqueue frame for websocket connection"
            );
        }
    }
}

#[derive(Clone)]
pub struct RoomManager {
    // 管理器只保存 sender 注册表，不直接持有某个房间的可变状态。
    rooms: Arc<RwLock<HashMap<String, RoomHandle>>>,
    rule_engine: RuleEngineHandle,
    wall_factory: WallFactory,
    match_event_writer: MatchEventWriter,
    runtime_mode: RoomRuntimeMode,
}

impl Default for RoomManager {
    fn default() -> Self {
        Self::with_rule_engine(RuleEngineHandle::default())
    }
}

#[derive(Clone)]
struct RoomHandle {
    command_tx: mpsc::Sender<RoomCommand>,
}

// 房间内所有状态变更都必须先变成命令，再进入串行事件循环。
enum RoomCommand {
    Join {
        request_id: String,
        connection: ConnectionHandle,
        request: JoinRoomRequest,
        authorized_join: AuthorizedJoin,
    },
    Ready {
        request_id: String,
        connection_id: String,
        request: ReadyRequest,
    },
    ResumeSession {
        request_id: String,
        connection: ConnectionHandle,
        request: ResumeSessionRequest,
    },
    PlayerAction {
        request_id: String,
        connection_id: String,
        request: PlayerActionRequest,
    },
    ClaimWindowTimeout {
        action_window_id: u64,
    },
    Disconnect {
        connection_id: String,
    },
    AdvanceAfterRoundSettlement {
        // 结算后延迟推进下一手，避免客户端还没消费 RoundSettlement 就立刻被下一手 Sync 覆盖。
        settled_hand_number: u32,
    },
    RemovePlayer {
        // 供 HTTP 大厅的 leave/kick/disband 流程在 waiting 阶段回收玩家占位。
        user_id: String,
    },
    RunLoadTestRoom {
        spec: LoadTestRoomSpec,
        result_tx: oneshot::Sender<LoadTestRoomResult>,
    },
}

#[derive(Clone, Default)]
struct PlayerRoundState {
    // drawn_tile 单独存放，便于明确区分“摸切”和“手切”。
    concealed_tiles: Vec<Tile>,
    drawn_tile: Option<Tile>,
    melds: Vec<Meld>,
    discards: Vec<crate::proto::client::DiscardTile>,
    flowers: Vec<Tile>,
    replacement_draw_count: u32,
}

struct RoundRuntimeState {
    // live/dead wall 分离后，补花、杠后补牌、海底牌判断都更直观。
    live_wall: VecDeque<Tile>,
    dead_wall: VecDeque<Tile>,
    active_claim_window: Option<ActiveClaimWindowState>,
    last_resolved_action: Option<ResolvedActionRecord>,
    turn_index: u32,
    replacement_draw_pending: bool,
}

#[derive(Clone)]
struct ActiveClaimWindowState {
    // source_event_seq 把窗口绑定到某个公开动作，避免客户端对过期窗口误操作。
    action_window_id: u64,
    source_event_seq: u64,
    source_seat: Seat,
    target_tile: Tile,
    trigger_action_kind: crate::proto::client::ActionKind,
    eligible_seats: Vec<Seat>,
    options_by_seat: HashMap<Seat, Vec<PromptOption>>,
    responded_seats: HashMap<Seat, ClaimResponse>,
    deadline_unix_ms: u64,
}

#[derive(Clone)]
enum ClaimResponse {
    // 这里只记录玩家提交了什么，真正优先级裁决统一在 resolve_claim_window_if_ready 中进行。
    Pass,
    Claim(crate::proto::client::ClaimAction),
    DeclareWin(crate::proto::client::DeclareWinAction),
}

struct ResolvedActionRecord {
    // 最近动作会被投影到规则引擎 RecentActionContext，也用于后续广播/结算补上下文。
    event_seq: u64,
    actor_seat: Seat,
    action_kind: crate::proto::client::ActionKind,
    tile: Option<Tile>,
    replacement_draw: bool,
    rob_kong_candidate: bool,
}

struct DrawOutcome {
    tile: Tile,
    replacement_draw: bool,
}

#[derive(Clone, Default)]
struct MatchPlayerStats {
    // 整场统计只保留排行榜和结算需要的数据，
    // 不在 match 级状态里重复存每手细节。
    rounds_won: u32,
    self_draw_wins: u32,
}

struct MatchRuntimeState {
    // 当前先实现简化东风场，共 4 手。
    total_hands: u32,
    completed_hands: u32,
    player_stats: HashMap<Seat, MatchPlayerStats>,
    last_round_winner: Option<Seat>,
    last_round_self_draw: bool,
    last_match_settlement: Option<MatchSettlementSummary>,
}

struct RoundTransition {
    // RoundTransition 只表达“结算之后该怎么走”，
    // 让单手牌结算逻辑和整场编排逻辑保持解耦。
    next_prevailing_wind: Seat,
    next_dealer_seat: Seat,
    next_hand_number: u32,
    next_dealer_streak: u32,
    match_finished: bool,
}

#[derive(Clone)]
struct MatchSettlementSummary {
    // 整场已经结束时，重连玩家仍然需要拿到最终排名。
    event_seq: u64,
    standings: Vec<FinalStanding>,
    finished_at_unix_ms: u64,
}

#[derive(Clone, Copy)]
enum DrawSource {
    LiveWall,
    DeadWall,
}

async fn run_room_task(
    rooms: Arc<RwLock<HashMap<String, RoomHandle>>>,
    mut room_state: RoomState,
    mut command_rx: mpsc::Receiver<RoomCommand>,
) {
    info!(room_id = %room_state.room_id, "room task started");

    // 同一个房间的所有 Join/Ready/Action 都在这里按顺序串行处理，
    // 这是整个项目避免 Arc<RwLock<RoomState>> 并发竞态的核心。
    while let Some(command) = command_rx.recv().await {
        room_state.handle_command(command).await;
        if room_state.should_shutdown {
            break;
        }
    }

    rooms.write().await.remove(&room_state.room_id);
    info!(room_id = %room_state.room_id, "room task stopped");
}

struct RoomState {
    // RoomState 被单个 room task 独占持有，外部只能通过消息与它交互。
    rule_engine: RuleEngineHandle,
    room_id: String,
    match_id: String,
    room_config: RoomConfig,
    wall_factory: WallFactory,
    room_command_tx: mpsc::Sender<RoomCommand>,
    match_event_writer: MatchEventWriter,
    runtime_mode: RoomRuntimeMode,
    should_shutdown: bool,
    phase: GamePhase,
    prevailing_wind: Seat,
    dealer_seat: Seat,
    current_turn_seat: Seat,
    hand_number: u32,
    dealer_streak: u32,
    wall_tiles_remaining: u32,
    dead_wall_tiles_remaining: u32,
    next_event_seq: u64,
    next_action_window_id: u64,
    players_by_seat: HashMap<Seat, PlayerSlot>,
    player_round_state: HashMap<Seat, PlayerRoundState>,
    round_runtime: Option<RoundRuntimeState>,
    match_runtime: MatchRuntimeState,
    user_to_seat: HashMap<String, Seat>,
    connection_to_user: HashMap<String, String>,
}

#[derive(Debug)]
struct ActionValidationError {
    reject_code: RejectCode,
    message: String,
}

impl ActionValidationError {
    fn new(reject_code: RejectCode, message: impl Into<String>) -> Self {
        Self {
            reject_code,
            message: message.into(),
        }
    }
}

struct PlayerSlot {
    // 连接信息与座位元数据放这里；对局中的牌面信息放在 PlayerRoundState。
    user_id: String,
    display_name: String,
    seat: Seat,
    status: PlayerStatus,
    score: i64,
    connected: bool,
    auto_play_enabled: bool,
    connection_id: Option<String>,
    outbound_tx: Option<ConnectionSender>,
    resume_token: String,
}

// 房间模块的具体实现按职责拆到分片里，根文件只保留核心类型定义和总装配。
// `include!` 放在模块级别，这样拆出来的方法仍然属于当前 room 模块，
// 不会引入额外的可见性改造。
include!("room/manager.rs");
include!("room/state_handlers.rs");
include!("room/state_engine.rs");
include!("room/state_broadcast.rs");
include!("room/state_match.rs");
include!("room/state_load_test.rs");

// 纯辅助函数单独放在这里，避免把结构体方法区继续拉长。
include!("room/helpers.rs");

#[cfg(test)]
include!("room/tests.rs");
