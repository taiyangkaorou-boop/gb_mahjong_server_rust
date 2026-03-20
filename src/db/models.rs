use serde::{Deserialize, Serialize};
use serde_json::Value;

// 这里定义“接近数据库表结构”的记录类型。
// 服务层和 PostgreSQL 仓储共享这些数据边界，避免重复拼装 JSON。

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRecord {
    // UserRecord 对应 users 表中的账户主记录。
    // 当前第一版只有 guest 用户，但这里预留了 auth_provider/auth_subject 供后续扩展。
    pub user_id: String,
    pub display_name: String,
    pub auth_provider: String,
    pub auth_subject: Option<String>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub last_seen_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSessionRecord {
    // session_token_hash 对应 SQL 中的 user_sessions 表，避免把明文 token 放进持久层。
    pub session_id: String,
    pub user_id: String,
    pub session_token_hash: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub revoked_at_unix_ms: Option<u64>,
    pub last_seen_at_unix_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomStatusRecord {
    // Waiting/Active/Closed 和 rooms.status 一一对应。
    // 这里单独建 enum，避免服务层四处直接比较裸字符串。
    Waiting,
    Active,
    Closed,
}

impl RoomStatusRecord {
    pub fn as_db_str(self) -> &'static str {
        match self {
            RoomStatusRecord::Waiting => "waiting",
            RoomStatusRecord::Active => "active",
            RoomStatusRecord::Closed => "closed",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "waiting" => Some(Self::Waiting),
            "active" => Some(Self::Active),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomConfigRecord {
    // config_snapshot 会被整体落成 JSONB。
    // 这里保持字段是显式结构体，避免业务层直接操作无类型 JSON。
    pub ruleset_id: String,
    pub seat_count: u32,
    pub enable_flower_tiles: bool,
    pub enable_robbing_kong: bool,
    pub enable_kong_draw: bool,
    pub reconnect_grace_seconds: u32,
    pub action_timeout_ms: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomRecord {
    // RoomRecord 对应大厅阶段的 rooms 表，不代表正式对局 match。
    pub room_id: String,
    pub room_code: String,
    pub owner_user_id: String,
    pub status: RoomStatusRecord,
    pub config_snapshot: RoomConfigRecord,
    pub current_match_id: Option<String>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomMemberRecord {
    // 这里保存的是大厅成员快照，不包含真实手牌等对局私有信息。
    pub room_id: String,
    pub user_id: String,
    pub display_name: String,
    pub seat: String,
    pub joined_at_unix_ms: u64,
    pub ready: bool,
    pub kicked_at_unix_ms: Option<u64>,
    pub left_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomSnapshotRecord {
    // RoomSnapshotRecord 是大厅层最常见的读模型：
    // 一个房间主记录 + 当前仍有效的成员列表。
    pub room: RoomRecord,
    pub members: Vec<RoomMemberRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchStatusRecord {
    // MatchStatusRecord 对应 matches.status，用于区分未开始、进行中和已结束对局。
    Waiting,
    Active,
    Finished,
    Aborted,
}

impl MatchStatusRecord {
    pub fn as_db_str(self) -> &'static str {
        match self {
            MatchStatusRecord::Waiting => "waiting",
            MatchStatusRecord::Active => "active",
            MatchStatusRecord::Finished => "finished",
            MatchStatusRecord::Aborted => "aborted",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "waiting" => Some(Self::Waiting),
            "active" => Some(Self::Active),
            "finished" => Some(Self::Finished),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchRecord {
    // MatchRecord 对应正式对局聚合根。
    // 它保存的是"当前对局摘要"，细粒度过程仍然在 match_events 里。
    pub match_id: String,
    pub room_id: String,
    pub status: MatchStatusRecord,
    pub ruleset_id: String,
    pub config_snapshot: Value,
    pub seating_snapshot: Value,
    pub current_round_id: Option<String>,
    pub prevailing_wind: Option<String>,
    pub dealer_seat: Option<String>,
    pub started_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: Option<u64>,
    pub created_by_user_id: Option<String>,
    pub winner_user_id: Option<String>,
    pub last_event_seq: i64,
    pub final_result: Option<Value>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchEventRecord {
    // 这是从 match_events 表读出来的完整事件。
    // event_payload 保留 JSONB 原始内容，供回放和审计链路消费。
    pub event_id: i64,
    pub match_id: String,
    pub round_id: Option<String>,
    pub event_seq: i64,
    pub event_type: String,
    pub actor_user_id: Option<String>,
    pub actor_seat: Option<String>,
    pub request_id: Option<String>,
    pub correlation_id: Option<String>,
    pub causation_event_seq: Option<i64>,
    pub event_payload: Value,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewMatchEventRecord {
    // 这是写入 match_events 前的输入结构。
    // event_id / created_at 由数据库生成，所以这里不出现。
    pub match_id: String,
    pub round_id: Option<String>,
    pub event_seq: i64,
    pub event_type: String,
    pub actor_user_id: Option<String>,
    pub actor_seat: Option<String>,
    pub request_id: Option<String>,
    pub correlation_id: Option<String>,
    pub causation_event_seq: Option<i64>,
    pub event_payload: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayEventKind {
    // ReplayEventKind 把底层 event_type 归并成前端/工具更容易消费的时间线类别。
    RoundStarted,
    ActionBroadcast,
    ClaimWindowOpened,
    ClaimWindowResolved,
    RoundSettlement,
    MatchSettlement,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayEntryRecord {
    // ReplayEntryRecord 是回放时间线中的最小单位。
    // 它保留原始 payload，同时提供一个经过整理的 summary，便于回放 UI 和离线分析。
    pub event_seq: i64,
    pub round_id: Option<String>,
    pub event_type: String,
    pub event_kind: ReplayEventKind,
    pub actor_user_id: Option<String>,
    pub actor_seat: Option<String>,
    pub created_at_unix_ms: u64,
    pub invalid_payload: bool,
    pub summary: Value,
    pub raw_event_payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRoundRecord {
    // ReplayRoundRecord 把同一手牌的事件组织到一起，方便前端按 round_id 做逐手回放。
    pub round_id: String,
    pub hand_number: Option<u32>,
    pub prevailing_wind: Option<String>,
    pub dealer_seat: Option<String>,
    pub start_event_seq: Option<i64>,
    pub settlement_event_seq: Option<i64>,
    pub entries: Vec<ReplayEntryRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayWarningRecord {
    // 第一版 replay 选择“单条坏事件降级为 warning”，
    // 而不是让整个回放接口 500。
    pub event_seq: Option<i64>,
    pub event_type: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchReplayRecord {
    // MatchReplayRecord 是 /replay 接口的主读模型。
    // rounds 保存逐手时间线，terminal_entry 单独承载整场结算事件。
    pub match_record: MatchRecord,
    pub rounds: Vec<ReplayRoundRecord>,
    pub terminal_entry: Option<ReplayEntryRecord>,
    pub warnings: Vec<ReplayWarningRecord>,
}
