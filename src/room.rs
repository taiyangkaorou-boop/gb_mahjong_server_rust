use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::{mpsc, RwLock},
    time::{sleep, Duration},
};
use tracing::{debug, info, warn};

use crate::{
    engine::RuleEngineHandle,
    proto::{
        client::{
            player_action_request, server_frame, ActionBroadcast, ActionDetail, ActionPrompt,
            ActionRejected, ClaimWindow, DiscardAdded, DrawDetail, FanDetail, GamePhase,
            GameSnapshot, JoinRoomRequest, JoinRoomResponse, Meld, MeldAdded, PlayerActionRequest,
            PlayerConnectionChanged, PlayerState, PlayerStatus, PromptOption, ReadyRequest,
            RejectCode, ResolvedClaim, ResultingEffect, ResumeSessionRequest, RevealedHand,
            RoomConfig, RoundPlayerResult, RoundSettlement, Seat, SelfHandState, ServerFrame,
            SyncReason, SyncState, Tile, TurnAdvanced,
        },
        engine::{self as engine_proto, candidate_action},
    },
};

const ROOM_COMMAND_BUFFER: usize = 256;
const RECONNECT_GRACE_MS: u64 = 30_000;
const INITIAL_SCORE: i64 = 25_000;
const DEAD_WALL_TILE_COUNT: usize = 14;
const SEAT_ORDER: [Seat; 4] = [Seat::East, Seat::South, Seat::West, Seat::North];

// 牌墙生成器允许测试注入固定顺序，便于稳定复现摸牌和抢操作场景。
type WallFactory = Arc<dyn Fn(&RoomConfig, &str, u32) -> Vec<Tile> + Send + Sync>;

pub type ConnectionSender = mpsc::UnboundedSender<ServerFrame>;

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
    RemovePlayer {
        // 供 HTTP 大厅的 leave/kick/disband 流程在 waiting 阶段回收玩家占位。
        user_id: String,
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

#[derive(Clone, Copy)]
enum DrawSource {
    LiveWall,
    DeadWall,
}

async fn run_room_task(mut room_state: RoomState, mut command_rx: mpsc::Receiver<RoomCommand>) {
    info!(room_id = %room_state.room_id, "room task started");

    // 同一个房间的所有 Join/Ready/Action 都在这里按顺序串行处理，
    // 这是整个项目避免 Arc<RwLock<RoomState>> 并发竞态的核心。
    while let Some(command) = command_rx.recv().await {
        room_state.handle_command(command).await;
    }

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

// 纯辅助函数单独放在这里，避免把结构体方法区继续拉长。
include!("room/helpers.rs");

#[cfg(test)]
include!("room/tests.rs");
