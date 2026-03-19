#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// 这里先定义“接近数据库表结构”的记录类型。
// 当前实现仍以内存服务为主，但这些结构能提前稳定网关层的数据边界。
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
