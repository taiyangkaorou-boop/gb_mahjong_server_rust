use serde::{Deserialize, Serialize};
use serde_json::Value;

// 这里定义“接近数据库表结构”的记录类型。
// 服务层和 PostgreSQL 仓储共享这些数据边界，避免重复拼装 JSON。

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRecord {
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
    pub room: RoomRecord,
    pub members: Vec<RoomMemberRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchStatusRecord {
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchRecord {
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
