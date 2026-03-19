use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{db::RoomMemberRecord, proto::client::Seat};

// 大厅服务负责“房间创建、成员管理、房主权限、开局前生命周期”，
// 不直接处理正式对局中的抢操作和摸打状态。
const ROOM_CODE_LEN: usize = 6;
const SEAT_ORDER: [Seat; 4] = [Seat::East, Seat::South, Seat::West, Seat::North];

#[derive(Clone, Debug, Serialize)]
pub struct LobbyRoomConfig {
    pub ruleset_id: String,
    pub seat_count: u32,
    pub enable_flower_tiles: bool,
    pub enable_robbing_kong: bool,
    pub enable_kong_draw: bool,
    pub reconnect_grace_seconds: u32,
    pub action_timeout_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LobbyRoomStatus {
    Waiting,
    // Active 表示大厅已经锁定并进入正式对局，不再接受新成员加入或踢人。
    Active,
    #[allow(dead_code)]
    Closed,
}

#[derive(Clone, Debug)]
pub struct LobbyMember {
    // 大厅成员只保存 lobby 阶段的状态。
    // 真实手牌、副露、弃牌仍然全部属于 room task。
    pub user_id: String,
    pub display_name: String,
    pub seat: Seat,
    pub joined_at_unix_ms: u64,
    pub ready: bool,
    pub kicked_at_unix_ms: Option<u64>,
    pub left_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct LobbyRoom {
    // LobbyRoom 是“开局前房间”的权威状态，
    // 一旦房间 Active，正式牌局权威就切到 RoomManager/RoomState。
    pub room_id: String,
    pub room_code: String,
    pub owner_user_id: String,
    pub status: LobbyRoomStatus,
    pub config: LobbyRoomConfig,
    pub current_match_id: Option<String>,
    pub members: Vec<LobbyMember>,
    #[allow(dead_code)]
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LobbyMemberView {
    // 这是 HTTP 对外返回的大厅成员快照，不暴露内部时间戳和历史轨迹字段。
    pub user_id: String,
    pub display_name: String,
    pub seat: String,
    pub ready: bool,
    pub is_owner: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LobbyRoomView {
    pub room_id: String,
    pub room_code: String,
    pub owner_user_id: String,
    pub status: String,
    pub current_match_id: Option<String>,
    pub config: LobbyRoomConfig,
    pub members: Vec<LobbyMemberView>,
}

#[derive(Clone, Debug)]
pub struct LobbyJoinAccess {
    // WS JoinRoom 成功前，需要先从大厅拿到“这个用户是否允许进该房间”的授权结果。
    pub user_id: String,
    pub seat: Seat,
}

#[derive(Debug)]
pub enum LobbyError {
    AlreadyInRoom,
    RoomNotFound,
    RoomFull,
    RoomAlreadyActive,
    NotRoomOwner,
    CannotKickSelf,
    NotRoomMember,
    InvalidRoomCode,
}

#[derive(Default)]
struct LobbyStore {
    // 第一版按单机优先实现，所以大厅状态先走内存索引。
    // 后续需要多节点时，再把这层替换为数据库/Redis。
    rooms_by_id: HashMap<String, LobbyRoom>,
    room_id_by_code: HashMap<String, String>,
    room_id_by_user: HashMap<String, String>,
}

#[derive(Clone)]
pub struct LobbyService {
    store: Arc<RwLock<LobbyStore>>,
}

impl Default for LobbyService {
    fn default() -> Self {
        Self {
            store: Arc::new(RwLock::new(LobbyStore::default())),
        }
    }
}

impl LobbyService {
    pub async fn create_room(
        &self,
        user_id: &str,
        display_name: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        // 一个用户同一时刻只能属于一个 lobby/match 上下文，
        // 这里先在大厅层做最小隔离。
        let mut store = self.store.write().await;
        if store.room_id_by_user.contains_key(user_id) {
            return Err(LobbyError::AlreadyInRoom);
        }

        let room_id = format!("room_{}", Uuid::new_v4().simple());
        let room_code = next_room_code(&store.room_id_by_code);
        let created_at_unix_ms = unix_time_ms();
        let config = default_lobby_room_config();
        let room = LobbyRoom {
            room_id: room_id.clone(),
            room_code: room_code.clone(),
            owner_user_id: user_id.to_owned(),
            status: LobbyRoomStatus::Waiting,
            config: config.clone(),
            current_match_id: None,
            members: vec![LobbyMember {
                user_id: user_id.to_owned(),
                display_name: display_name.to_owned(),
                seat: Seat::East,
                joined_at_unix_ms: created_at_unix_ms,
                ready: false,
                kicked_at_unix_ms: None,
                left_at_unix_ms: None,
            }],
            created_at_unix_ms,
        };

        store
            .room_id_by_user
            .insert(user_id.to_owned(), room_id.clone());
        store.room_id_by_code.insert(room_code, room_id.clone());
        store.rooms_by_id.insert(room_id.clone(), room.clone());

        Ok(to_room_view(&room))
    }

    pub async fn join_room(
        &self,
        user_id: &str,
        display_name: &str,
        room_code: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        // 加房阶段完成 3 件事：
        // 1. 通过邀请码找房
        // 2. 检查房间仍在 waiting
        // 3. 分配一个空座位
        let mut store = self.store.write().await;
        if store.room_id_by_user.contains_key(user_id) {
            return Err(LobbyError::AlreadyInRoom);
        }

        let Some(room_id) = store
            .room_id_by_code
            .get(&room_code.to_ascii_uppercase())
            .cloned()
        else {
            return Err(LobbyError::InvalidRoomCode);
        };
        let Some(room) = store.rooms_by_id.get_mut(&room_id) else {
            return Err(LobbyError::RoomNotFound);
        };
        if room.status != LobbyRoomStatus::Waiting {
            return Err(LobbyError::RoomAlreadyActive);
        }
        if room.members.len() >= room.config.seat_count as usize {
            return Err(LobbyError::RoomFull);
        }

        let used_seats: HashSet<Seat> = room.members.iter().map(|member| member.seat).collect();
        let seat = SEAT_ORDER
            .into_iter()
            .find(|seat| !used_seats.contains(seat))
            .ok_or(LobbyError::RoomFull)?;

        room.members.push(LobbyMember {
            user_id: user_id.to_owned(),
            display_name: display_name.to_owned(),
            seat,
            joined_at_unix_ms: unix_time_ms(),
            ready: false,
            kicked_at_unix_ms: None,
            left_at_unix_ms: None,
        });
        let snapshot = room.clone();
        store.room_id_by_user.insert(user_id.to_owned(), room_id);
        Ok(to_room_view(&snapshot))
    }

    pub async fn get_room_for_user(
        &self,
        user_id: &str,
        room_id: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        // 大厅详情只允许房间成员自己查看，避免邀请码房间被随意探测。
        let store = self.store.read().await;
        let Some(room) = store.rooms_by_id.get(room_id) else {
            return Err(LobbyError::RoomNotFound);
        };
        if !room.members.iter().any(|member| member.user_id == user_id) {
            return Err(LobbyError::NotRoomMember);
        }

        Ok(to_room_view(room))
    }

    pub async fn leave_room(
        &self,
        user_id: &str,
        room_id: &str,
    ) -> Result<Option<LobbyRoomView>, LobbyError> {
        // 房主离开时自动转移所有权；房间清空时自动解散。
        let mut store = self.store.write().await;
        store.room_id_by_user.remove(user_id);
        let outcome = {
            let Some(room) = store.rooms_by_id.get_mut(room_id) else {
                return Err(LobbyError::RoomNotFound);
            };
            if room.status != LobbyRoomStatus::Waiting {
                return Err(LobbyError::RoomAlreadyActive);
            }

            let Some(member_index) = room
                .members
                .iter()
                .position(|member| member.user_id == user_id)
            else {
                return Err(LobbyError::NotRoomMember);
            };
            let was_owner = room.owner_user_id == user_id;
            room.members.remove(member_index);

            if room.members.is_empty() {
                Some((room.room_code.clone(), None))
            } else {
                if was_owner {
                    room.members.sort_by_key(|member| member.joined_at_unix_ms);
                    room.owner_user_id = room.members[0].user_id.clone();
                }
                Some((String::new(), Some(to_room_view(room))))
            }
        };

        let Some((room_code, room_view)) = outcome else {
            return Ok(None);
        };
        if room_view.is_none() {
            store.rooms_by_id.remove(room_id);
            store.room_id_by_code.remove(&room_code);
        }

        Ok(room_view)
    }

    pub async fn kick_member(
        &self,
        owner_user_id: &str,
        room_id: &str,
        target_user_id: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        // 踢人只允许发生在 waiting 阶段，且房主不能踢自己。
        if owner_user_id == target_user_id {
            return Err(LobbyError::CannotKickSelf);
        }

        let mut store = self.store.write().await;
        store.room_id_by_user.remove(target_user_id);
        let snapshot = {
            let Some(room) = store.rooms_by_id.get_mut(room_id) else {
                return Err(LobbyError::RoomNotFound);
            };
            if room.status != LobbyRoomStatus::Waiting {
                return Err(LobbyError::RoomAlreadyActive);
            }
            if room.owner_user_id != owner_user_id {
                return Err(LobbyError::NotRoomOwner);
            }

            let Some(member_index) = room
                .members
                .iter()
                .position(|member| member.user_id == target_user_id)
            else {
                return Err(LobbyError::NotRoomMember);
            };

            room.members.remove(member_index);
            to_room_view(room)
        };

        Ok(snapshot)
    }

    pub async fn disband_room(&self, owner_user_id: &str, room_id: &str) -> Result<(), LobbyError> {
        // 解散房间会一次性清理 room 与 member 反向索引。
        let mut store = self.store.write().await;
        let Some(room) = store.rooms_by_id.get(room_id).cloned() else {
            return Err(LobbyError::RoomNotFound);
        };
        if room.status != LobbyRoomStatus::Waiting {
            return Err(LobbyError::RoomAlreadyActive);
        }
        if room.owner_user_id != owner_user_id {
            return Err(LobbyError::NotRoomOwner);
        }

        for member in &room.members {
            store.room_id_by_user.remove(&member.user_id);
        }
        store.room_id_by_code.remove(&room.room_code);
        store.rooms_by_id.remove(room_id);
        Ok(())
    }

    pub async fn authorize_room_entry(
        &self,
        room_id: &str,
        user_id: &str,
    ) -> Result<LobbyJoinAccess, LobbyError> {
        // WebSocket 进房前的授权检查。
        // 大厅层负责回答“这个用户是不是这个房间的合法成员，以及应该坐哪一位”。
        let store = self.store.read().await;
        let Some(room) = store.rooms_by_id.get(room_id) else {
            return Err(LobbyError::RoomNotFound);
        };
        let Some(member) = room.members.iter().find(|member| member.user_id == user_id) else {
            return Err(LobbyError::NotRoomMember);
        };

        Ok(LobbyJoinAccess {
            user_id: member.user_id.clone(),
            seat: member.seat,
        })
    }

    pub async fn set_member_ready(
        &self,
        room_id: &str,
        user_id: &str,
        ready: bool,
    ) -> Result<bool, LobbyError> {
        // 当 4 人都 ready 时，把 lobby 状态切到 active。
        // 真实发牌和正式对局仍由 room task 接手。
        let mut store = self.store.write().await;
        let Some(room) = store.rooms_by_id.get_mut(room_id) else {
            return Err(LobbyError::RoomNotFound);
        };
        if room.status != LobbyRoomStatus::Waiting {
            return Ok(false);
        }
        let Some(member) = room
            .members
            .iter_mut()
            .find(|member| member.user_id == user_id)
        else {
            return Err(LobbyError::NotRoomMember);
        };
        member.ready = ready;

        let all_ready = room.members.len() == room.config.seat_count as usize
            && room.members.iter().all(|member| member.ready);
        if all_ready {
            room.status = LobbyRoomStatus::Active;
            room.current_match_id = Some(format!("match-{}", room.room_id));
        }

        Ok(all_ready)
    }

    pub async fn snapshot_room_member_records(&self, room_id: &str) -> Vec<RoomMemberRecord> {
        // 供 HTTP 解散房间等流程快速获取成员列表使用。
        let store = self.store.read().await;
        let Some(room) = store.rooms_by_id.get(room_id) else {
            return Vec::new();
        };
        room.members
            .iter()
            .map(|member| RoomMemberRecord {
                room_id: room.room_id.clone(),
                user_id: member.user_id.clone(),
                display_name: member.display_name.clone(),
                seat: member.seat.as_str_name().to_owned(),
                joined_at_unix_ms: member.joined_at_unix_ms,
                ready: member.ready,
                kicked_at_unix_ms: member.kicked_at_unix_ms,
                left_at_unix_ms: member.left_at_unix_ms,
            })
            .collect()
    }
}

pub fn default_lobby_room_config() -> LobbyRoomConfig {
    // 这里先与 RoomState 默认配置保持一致，避免大厅配置和对局配置漂移。
    LobbyRoomConfig {
        ruleset_id: "gb_mahjong_cn_v1".to_owned(),
        seat_count: 4,
        enable_flower_tiles: true,
        enable_robbing_kong: true,
        enable_kong_draw: true,
        reconnect_grace_seconds: 30,
        action_timeout_ms: 15_000,
    }
}

fn next_room_code(existing: &HashMap<String, String>) -> String {
    // 私密房第一版采用短邀请码，优先考虑“好输入”，不追求密码学不可猜测性。
    loop {
        let candidate = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(ROOM_CODE_LEN)
            .collect::<String>()
            .to_ascii_uppercase();
        if !existing.contains_key(&candidate) {
            return candidate;
        }
    }
}

fn to_room_view(room: &LobbyRoom) -> LobbyRoomView {
    // 对外 JSON 视图不暴露内部索引和时间戳，只返回 UI 真正需要的大厅快照。
    LobbyRoomView {
        room_id: room.room_id.clone(),
        room_code: room.room_code.clone(),
        owner_user_id: room.owner_user_id.clone(),
        status: match room.status {
            LobbyRoomStatus::Waiting => "waiting",
            LobbyRoomStatus::Active => "active",
            LobbyRoomStatus::Closed => "closed",
        }
        .to_owned(),
        current_match_id: room.current_match_id.clone(),
        config: room.config.clone(),
        members: room
            .members
            .iter()
            .map(|member| LobbyMemberView {
                user_id: member.user_id.clone(),
                display_name: member.display_name.clone(),
                seat: member.seat.as_str_name().to_owned(),
                ready: member.ready,
                is_owner: member.user_id == room.owner_user_id,
            })
            .collect(),
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
