// PostgreSQL 仓储实现集中放在这个文件里。
// 这里的职责是把服务层 record 结构可靠地映射到 SQL 读写，
// 不参与房间状态机的业务裁决，也不生成任何实时广播。
use std::{collections::HashSet, env, time::Duration};

use anyhow::Context;
use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::{
    postgres::{PgPoolOptions, PgRow},
    PgPool, Row,
};
use tracing::info;
use uuid::Uuid;

use crate::{
    auth::{AuthError, AuthRepository, AuthenticatedUser},
    db::{
        MatchEventRecord, MatchEventRepository, MatchStatusRecord, NewMatchEventRecord,
        RoomConfigRecord, RoomMemberRecord, RoomRecord, RoomSnapshotRecord, RoomStatusRecord,
        UserRecord, UserSessionRecord,
    },
    lobby::{
        default_lobby_room_config, to_room_view, LobbyError, LobbyJoinAccess, LobbyMember,
        LobbyRepository, LobbyRoom, LobbyRoomConfig, LobbyRoomStatus, LobbyRoomView,
    },
    proto::client::Seat,
};

const LOBBY_SEAT_ORDER: [Seat; 4] = [Seat::East, Seat::South, Seat::West, Seat::North];

#[derive(Clone, Debug)]
pub struct DatabaseConfig {
    pub database_url: String,
    pub max_connections: u32,
    pub connect_timeout_secs: u64,
}

impl DatabaseConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        // 运行时把数据库连接信息统一收敛到环境变量，避免散落在各个服务构造里。
        let database_url = env::var("DATABASE_URL").context("read DATABASE_URL")?;
        let max_connections = env::var("GB_MAHJONG_DB_MAX_CONNECTIONS")
            .ok()
            .map(|value| value.parse::<u32>())
            .transpose()
            .with_context(|| "parse GB_MAHJONG_DB_MAX_CONNECTIONS")?
            .unwrap_or(10);
        let connect_timeout_secs = env::var("GB_MAHJONG_DB_CONNECT_TIMEOUT_SECS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .with_context(|| "parse GB_MAHJONG_DB_CONNECT_TIMEOUT_SECS")?
            .unwrap_or(5);

        Ok(Self {
            database_url,
            max_connections,
            connect_timeout_secs,
        })
    }
}

pub async fn connect_pg_pool(config: &DatabaseConfig) -> anyhow::Result<PgPool> {
    // 连接池由应用启动阶段一次性初始化，后续通过 AppState 共享给仓储层。
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.connect_timeout_secs))
        .connect(&config.database_url)
        .await
        .with_context(|| "connect to PostgreSQL")?;

    info!(max_connections = config.max_connections, "postgres pool initialized");
    Ok(pool)
}

#[derive(Clone)]
pub struct PostgresAuthRepository {
    pool: PgPool,
}

impl PostgresAuthRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AuthRepository for PostgresAuthRepository {
    async fn create_guest_session(
        &self,
        user: UserRecord,
        session: UserSessionRecord,
    ) -> Result<(), AuthError> {
        // 注册游客时，用户记录和首个会话必须落在同一个事务里，
        // 否则会出现 user 已创建但 session 缺失的半成功状态。
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| AuthError::Storage(error.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO users (
                user_id, display_name, auth_provider, auth_subject, created_at, updated_at, last_seen_at
            )
            VALUES (
                $1, $2, $3, $4,
                to_timestamp($5::double precision / 1000.0),
                to_timestamp($6::double precision / 1000.0),
                to_timestamp($7::double precision / 1000.0)
            )
            "#,
        )
        .bind(&user.user_id)
        .bind(&user.display_name)
        .bind(&user.auth_provider)
        .bind(&user.auth_subject)
        .bind(user.created_at_unix_ms as i64)
        .bind(user.updated_at_unix_ms as i64)
        .bind(user.last_seen_at_unix_ms.map(|value| value as i64))
        .execute(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO user_sessions (
                session_id, user_id, session_token_hash, issued_at, expires_at, revoked_at, last_seen_at
            )
            VALUES (
                $1, $2, $3,
                to_timestamp($4::double precision / 1000.0),
                to_timestamp($5::double precision / 1000.0),
                CASE WHEN $6 IS NULL THEN NULL ELSE to_timestamp($6::double precision / 1000.0) END,
                CASE WHEN $7 IS NULL THEN NULL ELSE to_timestamp($7::double precision / 1000.0) END
            )
            "#,
        )
        .bind(&session.session_id)
        .bind(&session.user_id)
        .bind(&session.session_token_hash)
        .bind(session.issued_at_unix_ms as i64)
        .bind(session.expires_at_unix_ms as i64)
        .bind(session.revoked_at_unix_ms.map(|value| value as i64))
        .bind(session.last_seen_at_unix_ms.map(|value| value as i64))
        .execute(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| AuthError::Storage(error.to_string()))?;

        Ok(())
    }

    async fn authenticate_session(
        &self,
        session_token_hash: &str,
        now_unix_ms: u64,
    ) -> Result<AuthenticatedUser, AuthError> {
        // 认证时顺手更新 last_seen_at，保证大厅和风控侧看到的是最新活跃时间。
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| AuthError::Storage(error.to_string()))?;

        let row = sqlx::query(
            r#"
            SELECT u.user_id, u.display_name, u.auth_provider, s.session_id
            FROM user_sessions s
            JOIN users u ON u.user_id = s.user_id
            WHERE s.session_token_hash = $1
              AND s.revoked_at IS NULL
              AND s.expires_at > to_timestamp($2::double precision / 1000.0)
            LIMIT 1
            "#,
        )
        .bind(session_token_hash)
        .bind(now_unix_ms as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?
        .ok_or(AuthError::InvalidSession)?;

        let user_id: String = row.get("user_id");
        let session_id: String = row.get("session_id");
        let display_name: String = row.get("display_name");
        let auth_provider: String = row.get("auth_provider");

        sqlx::query(
            r#"
            UPDATE user_sessions
            SET last_seen_at = to_timestamp($2::double precision / 1000.0)
            WHERE session_id = $1
            "#,
        )
        .bind(&session_id)
        .bind(now_unix_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?;

        sqlx::query(
            r#"
            UPDATE users
            SET last_seen_at = to_timestamp($2::double precision / 1000.0),
                updated_at = to_timestamp($2::double precision / 1000.0)
            WHERE user_id = $1
            "#,
        )
        .bind(&user_id)
        .bind(now_unix_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| AuthError::Storage(error.to_string()))?;

        Ok(AuthenticatedUser {
            user_id,
            display_name,
            auth_provider,
        })
    }

    async fn rotate_session(
        &self,
        old_session_token_hash: &str,
        mut new_session: UserSessionRecord,
        now_unix_ms: u64,
    ) -> Result<AuthenticatedUser, AuthError> {
        // refresh 语义不是“原地续期”，而是“旧 session 作废，新 session 生效”。
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| AuthError::Storage(error.to_string()))?;

        let row = sqlx::query(
            r#"
            SELECT u.user_id, u.display_name, u.auth_provider, s.session_id
            FROM user_sessions s
            JOIN users u ON u.user_id = s.user_id
            WHERE s.session_token_hash = $1
              AND s.revoked_at IS NULL
              AND s.expires_at > to_timestamp($2::double precision / 1000.0)
            LIMIT 1
            "#,
        )
        .bind(old_session_token_hash)
        .bind(now_unix_ms as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?
        .ok_or(AuthError::InvalidSession)?;

        let user_id: String = row.get("user_id");
        let session_id: String = row.get("session_id");
        let display_name: String = row.get("display_name");
        let auth_provider: String = row.get("auth_provider");

        sqlx::query(
            r#"
            UPDATE user_sessions
            SET revoked_at = to_timestamp($2::double precision / 1000.0),
                last_seen_at = to_timestamp($2::double precision / 1000.0)
            WHERE session_id = $1
            "#,
        )
        .bind(&session_id)
        .bind(now_unix_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?;

        new_session.user_id = user_id.clone();

        sqlx::query(
            r#"
            INSERT INTO user_sessions (
                session_id, user_id, session_token_hash, issued_at, expires_at, revoked_at, last_seen_at
            )
            VALUES (
                $1, $2, $3,
                to_timestamp($4::double precision / 1000.0),
                to_timestamp($5::double precision / 1000.0),
                NULL,
                CASE WHEN $6 IS NULL THEN NULL ELSE to_timestamp($6::double precision / 1000.0) END
            )
            "#,
        )
        .bind(&new_session.session_id)
        .bind(&new_session.user_id)
        .bind(&new_session.session_token_hash)
        .bind(new_session.issued_at_unix_ms as i64)
        .bind(new_session.expires_at_unix_ms as i64)
        .bind(new_session.last_seen_at_unix_ms.map(|value| value as i64))
        .execute(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?;

        sqlx::query(
            r#"
            UPDATE users
            SET last_seen_at = to_timestamp($2::double precision / 1000.0),
                updated_at = to_timestamp($2::double precision / 1000.0)
            WHERE user_id = $1
            "#,
        )
        .bind(&user_id)
        .bind(now_unix_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| AuthError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| AuthError::Storage(error.to_string()))?;

        Ok(AuthenticatedUser {
            user_id,
            display_name,
            auth_provider,
        })
    }
}

#[derive(Clone)]
pub struct PostgresLobbyRepository {
    pool: PgPool,
}

impl PostgresLobbyRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl LobbyRepository for PostgresLobbyRepository {
    async fn create_room(
        &self,
        user_id: &str,
        display_name: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        ensure_user_not_in_active_room(&mut tx, user_id).await?;

        let room_id = format!("room_{}", Uuid::new_v4().simple());
        let room_code = next_available_room_code(&mut tx).await?;
        let now_ms = unix_time_ms();
        let config = default_lobby_room_config();
        let config_record = room_config_to_record(&config);

        sqlx::query(
            r#"
            INSERT INTO rooms (
                room_id, room_code, owner_user_id, status, config_snapshot, current_match_id,
                created_at, updated_at
            )
            VALUES (
                $1, $2, $3, $4, $5, NULL,
                to_timestamp($6::double precision / 1000.0),
                to_timestamp($6::double precision / 1000.0)
            )
            "#,
        )
        .bind(&room_id)
        .bind(&room_code)
        .bind(user_id)
        .bind(RoomStatusRecord::Waiting.as_db_str())
        .bind(sqlx::types::Json(config_record))
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO room_members (
                room_id, user_id, seat, joined_at, ready, kicked_at, left_at
            )
            VALUES (
                $1, $2, $3, to_timestamp($4::double precision / 1000.0), false, NULL, NULL
            )
            "#,
        )
        .bind(&room_id)
        .bind(user_id)
        .bind(Seat::East.as_str_name())
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        self.joined_room_view_by_id(&room_id, display_name)
            .await
    }

    async fn join_room(
        &self,
        user_id: &str,
        display_name: &str,
        room_code: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        ensure_user_not_in_active_room(&mut tx, user_id).await?;

        let room = sqlx::query(
            r#"
            SELECT room_id, owner_user_id, status, current_match_id, config_snapshot, room_code,
                   CAST(EXTRACT(EPOCH FROM created_at) * 1000 AS BIGINT) AS created_at_unix_ms,
                   CAST(EXTRACT(EPOCH FROM updated_at) * 1000 AS BIGINT) AS updated_at_unix_ms
            FROM rooms
            WHERE room_code = $1
            FOR UPDATE
            "#,
        )
        .bind(room_code.to_ascii_uppercase())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?
        .ok_or(LobbyError::InvalidRoomCode)?;

        let status = parse_room_status(room.get::<&str, _>("status"))?;
        if status != LobbyRoomStatus::Waiting {
            return Err(LobbyError::RoomAlreadyActive);
        }

        let room_id: String = room.get("room_id");
        let members = fetch_live_room_members(&mut tx, &room_id).await?;
        let config = json_room_config_to_domain(
            room.get::<sqlx::types::Json<RoomConfigRecord>, _>("config_snapshot")
                .0,
        );
        if members.len() >= config.seat_count as usize {
            return Err(LobbyError::RoomFull);
        }

        let used_seats: HashSet<Seat> = members.iter().map(|member| parse_seat(&member.seat)).collect::<Result<_, _>>()?;
        let seat = LOBBY_SEAT_ORDER
            .into_iter()
            .find(|seat| !used_seats.contains(seat))
            .ok_or(LobbyError::RoomFull)?;

        sqlx::query(
            r#"
            INSERT INTO room_members (
                room_id, user_id, seat, joined_at, ready, kicked_at, left_at
            )
            VALUES (
                $1, $2, $3, to_timestamp($4::double precision / 1000.0), false, NULL, NULL
            )
            "#,
        )
        .bind(&room_id)
        .bind(user_id)
        .bind(seat.as_str_name())
        .bind(unix_time_ms() as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        self.joined_room_view_by_id(&room_id, display_name).await
    }

    async fn get_room_for_user(
        &self,
        user_id: &str,
        room_id: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        let snapshot = self.get_room_snapshot(room_id).await?;
        if !snapshot.members.iter().any(|member| member.user_id == user_id) {
            return Err(LobbyError::NotRoomMember);
        }

        Ok(to_room_view(&snapshot_to_domain(snapshot)?))
    }

    async fn leave_room(
        &self,
        user_id: &str,
        room_id: &str,
    ) -> Result<Option<LobbyRoomView>, LobbyError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        let snapshot = fetch_room_snapshot_for_update(&mut tx, room_id).await?;
        if snapshot.room.status != RoomStatusRecord::Waiting {
            return Err(LobbyError::RoomAlreadyActive);
        }
        if !snapshot.members.iter().any(|member| member.user_id == user_id) {
            return Err(LobbyError::NotRoomMember);
        }

        let now_ms = unix_time_ms();
        sqlx::query(
            r#"
            UPDATE room_members
            SET left_at = to_timestamp($3::double precision / 1000.0), ready = false
            WHERE room_id = $1 AND user_id = $2 AND kicked_at IS NULL AND left_at IS NULL
            "#,
        )
        .bind(room_id)
        .bind(user_id)
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?;

        let mut members = fetch_live_room_members(&mut tx, room_id).await?;
        if members.is_empty() {
            sqlx::query("DELETE FROM rooms WHERE room_id = $1")
                .bind(room_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| LobbyError::Storage(error.to_string()))?;
            tx.commit()
                .await
                .map_err(|error| LobbyError::Storage(error.to_string()))?;
            return Ok(None);
        }

        if snapshot.room.owner_user_id == user_id {
            members.sort_by_key(|member| member.joined_at_unix_ms);
            sqlx::query(
                r#"
                UPDATE rooms
                SET owner_user_id = $2
                WHERE room_id = $1
                "#,
            )
            .bind(room_id)
            .bind(&members[0].user_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        Ok(Some(to_room_view(&snapshot_to_domain(
            self.get_room_snapshot(room_id).await?,
        )?)))
    }

    async fn kick_member(
        &self,
        owner_user_id: &str,
        room_id: &str,
        target_user_id: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        if owner_user_id == target_user_id {
            return Err(LobbyError::CannotKickSelf);
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        let snapshot = fetch_room_snapshot_for_update(&mut tx, room_id).await?;
        if snapshot.room.status != RoomStatusRecord::Waiting {
            return Err(LobbyError::RoomAlreadyActive);
        }
        if snapshot.room.owner_user_id != owner_user_id {
            return Err(LobbyError::NotRoomOwner);
        }
        if !snapshot
            .members
            .iter()
            .any(|member| member.user_id == target_user_id)
        {
            return Err(LobbyError::NotRoomMember);
        }

        sqlx::query(
            r#"
            UPDATE room_members
            SET kicked_at = to_timestamp($3::double precision / 1000.0), ready = false
            WHERE room_id = $1 AND user_id = $2 AND kicked_at IS NULL AND left_at IS NULL
            "#,
        )
        .bind(room_id)
        .bind(target_user_id)
        .bind(unix_time_ms() as i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        Ok(to_room_view(&snapshot_to_domain(
            self.get_room_snapshot(room_id).await?,
        )?))
    }

    async fn disband_room(&self, owner_user_id: &str, room_id: &str) -> Result<(), LobbyError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        let snapshot = fetch_room_snapshot_for_update(&mut tx, room_id).await?;
        if snapshot.room.status != RoomStatusRecord::Waiting {
            return Err(LobbyError::RoomAlreadyActive);
        }
        if snapshot.room.owner_user_id != owner_user_id {
            return Err(LobbyError::NotRoomOwner);
        }

        sqlx::query("DELETE FROM rooms WHERE room_id = $1")
            .bind(room_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        Ok(())
    }

    async fn authorize_room_entry(
        &self,
        room_id: &str,
        user_id: &str,
    ) -> Result<LobbyJoinAccess, LobbyError> {
        let row = sqlx::query(
            r#"
            SELECT seat
            FROM room_members
            WHERE room_id = $1 AND user_id = $2 AND kicked_at IS NULL AND left_at IS NULL
            LIMIT 1
            "#,
        )
        .bind(room_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?
        .ok_or(LobbyError::NotRoomMember)?;

        Ok(LobbyJoinAccess {
            user_id: user_id.to_owned(),
            seat: parse_seat(row.get::<&str, _>("seat"))?,
        })
    }

    async fn set_member_ready(
        &self,
        room_id: &str,
        user_id: &str,
        ready: bool,
    ) -> Result<bool, LobbyError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        let snapshot = fetch_room_snapshot_for_update(&mut tx, room_id).await?;
        if snapshot.room.status != RoomStatusRecord::Waiting {
            return Ok(false);
        }
        if !snapshot.members.iter().any(|member| member.user_id == user_id) {
            return Err(LobbyError::NotRoomMember);
        }

        sqlx::query(
            r#"
            UPDATE room_members
            SET ready = $3
            WHERE room_id = $1 AND user_id = $2 AND kicked_at IS NULL AND left_at IS NULL
            "#,
        )
        .bind(room_id)
        .bind(user_id)
        .bind(ready)
        .execute(&mut *tx)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?;

        let members = fetch_live_room_members(&mut tx, room_id).await?;
        let config = snapshot.room.config_snapshot.clone();
        let ruleset_id = config.ruleset_id.clone();
        let all_ready = members.len() == config.seat_count as usize && members.iter().all(|member| member.ready);
        if all_ready {
            let match_id = format!("match-{room_id}");
            let seating_snapshot = build_seating_snapshot(&members);
            sqlx::query(
                r#"
                UPDATE rooms
                SET status = 'active', current_match_id = $2
                WHERE room_id = $1
                "#,
            )
            .bind(room_id)
            .bind(&match_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

            sqlx::query(
                r#"
                INSERT INTO matches (
                    match_id, room_id, status, ruleset_id, config_snapshot, seating_snapshot,
                    current_round_id, prevailing_wind, dealer_seat, started_at, created_by_user_id,
                    last_event_seq
                )
                VALUES (
                    $1, $2, 'active', $3, $4, $5, 'hand-1', 'SEAT_EAST', 'SEAT_EAST',
                    to_timestamp($6::double precision / 1000.0), $7, 0
                )
                ON CONFLICT (match_id) DO NOTHING
                "#,
            )
            .bind(&match_id)
            .bind(room_id)
            .bind(&ruleset_id)
            .bind(sqlx::types::Json(config))
            .bind(sqlx::types::Json(seating_snapshot))
            .bind(unix_time_ms() as i64)
            .bind(&snapshot.room.owner_user_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        Ok(all_ready)
    }

    async fn snapshot_room_member_records(&self, room_id: &str) -> Vec<RoomMemberRecord> {
        self.get_room_snapshot(room_id)
            .await
            .map(|snapshot| snapshot.members)
            .unwrap_or_default()
    }
}

impl PostgresLobbyRepository {
    async fn joined_room_view_by_id(
        &self,
        room_id: &str,
        display_name_fallback: &str,
    ) -> Result<LobbyRoomView, LobbyError> {
        let mut room = snapshot_to_domain(self.get_room_snapshot(room_id).await?)?;
        for member in &mut room.members {
            if member.display_name.is_empty() {
                member.display_name = display_name_fallback.to_owned();
            }
        }
        Ok(to_room_view(&room))
    }

    async fn get_room_snapshot(&self, room_id: &str) -> Result<RoomSnapshotRecord, LobbyError> {
        let room = sqlx::query(
            r#"
            SELECT room_id, room_code, owner_user_id, status, config_snapshot, current_match_id,
                   CAST(EXTRACT(EPOCH FROM created_at) * 1000 AS BIGINT) AS created_at_unix_ms,
                   CAST(EXTRACT(EPOCH FROM updated_at) * 1000 AS BIGINT) AS updated_at_unix_ms
            FROM rooms
            WHERE room_id = $1
            "#,
        )
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?
        .ok_or(LobbyError::RoomNotFound)?;

        let members = sqlx::query(
            r#"
            SELECT rm.room_id, rm.user_id, COALESCE(u.display_name, '') AS display_name, rm.seat,
                   CAST(EXTRACT(EPOCH FROM rm.joined_at) * 1000 AS BIGINT) AS joined_at_unix_ms,
                   rm.ready,
                   CASE WHEN rm.kicked_at IS NULL THEN NULL ELSE CAST(EXTRACT(EPOCH FROM rm.kicked_at) * 1000 AS BIGINT) END AS kicked_at_unix_ms,
                   CASE WHEN rm.left_at IS NULL THEN NULL ELSE CAST(EXTRACT(EPOCH FROM rm.left_at) * 1000 AS BIGINT) END AS left_at_unix_ms
            FROM room_members rm
            JOIN users u ON u.user_id = rm.user_id
            WHERE rm.room_id = $1 AND rm.kicked_at IS NULL AND rm.left_at IS NULL
            ORDER BY rm.joined_at ASC
            "#,
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| LobbyError::Storage(error.to_string()))?
        .into_iter()
        .map(room_member_from_row)
        .collect::<Result<Vec<_>, _>>()?;

        Ok(RoomSnapshotRecord {
            room: room_record_from_row(room)?,
            members,
        })
    }
}

#[derive(Clone)]
pub struct PostgresMatchEventRepository {
    pool: PgPool,
}

impl PostgresMatchEventRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MatchEventRepository for PostgresMatchEventRepository {
    async fn append_event(&self, event: NewMatchEventRecord) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await.context("begin match event tx")?;

        sqlx::query(
            r#"
            INSERT INTO match_events (
                match_id, round_id, event_seq, event_type, actor_user_id, actor_seat,
                request_id, correlation_id, causation_event_seq, event_payload
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(&event.match_id)
        .bind(&event.round_id)
        .bind(event.event_seq)
        .bind(&event.event_type)
        .bind(&event.actor_user_id)
        .bind(&event.actor_seat)
        .bind(&event.request_id)
        .bind(&event.correlation_id)
        .bind(event.causation_event_seq)
        .bind(sqlx::types::Json(event.event_payload.clone()))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("insert match event {}#{}", event.match_id, event.event_seq))?;

        let final_result = if event.event_type == "round_settlement" {
            Some(event.event_payload.clone())
        } else {
            None
        };
        let match_status = if event.event_type == "round_settlement" {
            MatchStatusRecord::Finished.as_db_str()
        } else {
            MatchStatusRecord::Active.as_db_str()
        };

        sqlx::query(
            r#"
            UPDATE matches
            SET last_event_seq = GREATEST(last_event_seq, $2),
                status = $3,
                ended_at = CASE WHEN $4::jsonb IS NULL THEN ended_at ELSE now() END,
                final_result = COALESCE($4::jsonb, final_result)
            WHERE match_id = $1
            "#,
        )
        .bind(&event.match_id)
        .bind(event.event_seq)
        .bind(match_status)
        .bind(final_result.map(sqlx::types::Json))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("update match aggregate {}", event.match_id))?;

        tx.commit().await.context("commit match event tx")?;
        Ok(())
    }

    async fn list_events(&self, match_id: &str) -> anyhow::Result<Vec<MatchEventRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT event_id, match_id, round_id, event_seq, event_type, actor_user_id, actor_seat,
                   request_id, correlation_id, causation_event_seq, event_payload,
                   CAST(EXTRACT(EPOCH FROM created_at) * 1000 AS BIGINT) AS created_at_unix_ms
            FROM match_events
            WHERE match_id = $1
            ORDER BY event_seq ASC
            "#,
        )
        .bind(match_id)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("list match events for {match_id}"))?;

        rows.into_iter().map(match_event_from_row).collect()
    }
}

async fn ensure_user_not_in_active_room(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: &str,
) -> Result<(), LobbyError> {
    let existing = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT 1
        FROM room_members rm
        JOIN rooms r ON r.room_id = rm.room_id
        WHERE rm.user_id = $1
          AND rm.kicked_at IS NULL
          AND rm.left_at IS NULL
          AND r.status IN ('waiting', 'active')
        LIMIT 1
        "#,
    )
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| LobbyError::Storage(error.to_string()))?;

    if existing.is_some() {
        return Err(LobbyError::AlreadyInRoom);
    }

    Ok(())
}

async fn next_available_room_code(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<String, LobbyError> {
    loop {
        let candidate = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(6)
            .collect::<String>()
            .to_ascii_uppercase();

        let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM rooms WHERE room_code = $1 LIMIT 1")
            .bind(&candidate)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| LobbyError::Storage(error.to_string()))?;

        if exists.is_none() {
            return Ok(candidate);
        }
    }
}

async fn fetch_room_snapshot_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room_id: &str,
) -> Result<RoomSnapshotRecord, LobbyError> {
    let row = sqlx::query(
        r#"
        SELECT room_id, room_code, owner_user_id, status, config_snapshot, current_match_id,
               CAST(EXTRACT(EPOCH FROM created_at) * 1000 AS BIGINT) AS created_at_unix_ms,
               CAST(EXTRACT(EPOCH FROM updated_at) * 1000 AS BIGINT) AS updated_at_unix_ms
        FROM rooms
        WHERE room_id = $1
        FOR UPDATE
        "#,
    )
    .bind(room_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| LobbyError::Storage(error.to_string()))?
    .ok_or(LobbyError::RoomNotFound)?;

    let members = fetch_live_room_members(tx, room_id).await?;
    Ok(RoomSnapshotRecord {
        room: room_record_from_row(row)?,
        members,
    })
}

async fn fetch_live_room_members(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room_id: &str,
) -> Result<Vec<RoomMemberRecord>, LobbyError> {
    sqlx::query(
        r#"
        SELECT rm.room_id, rm.user_id, COALESCE(u.display_name, '') AS display_name, rm.seat,
               CAST(EXTRACT(EPOCH FROM rm.joined_at) * 1000 AS BIGINT) AS joined_at_unix_ms,
               rm.ready,
               CASE WHEN rm.kicked_at IS NULL THEN NULL ELSE CAST(EXTRACT(EPOCH FROM rm.kicked_at) * 1000 AS BIGINT) END AS kicked_at_unix_ms,
               CASE WHEN rm.left_at IS NULL THEN NULL ELSE CAST(EXTRACT(EPOCH FROM rm.left_at) * 1000 AS BIGINT) END AS left_at_unix_ms
        FROM room_members rm
        JOIN users u ON u.user_id = rm.user_id
        WHERE rm.room_id = $1 AND rm.kicked_at IS NULL AND rm.left_at IS NULL
        ORDER BY rm.joined_at ASC
        FOR UPDATE
        "#,
    )
    .bind(room_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| LobbyError::Storage(error.to_string()))?
    .into_iter()
    .map(room_member_from_row)
    .collect()
}

fn room_record_from_row(row: PgRow) -> Result<RoomRecord, LobbyError> {
    Ok(RoomRecord {
        room_id: row.get("room_id"),
        room_code: row.get("room_code"),
        owner_user_id: row.get("owner_user_id"),
        status: RoomStatusRecord::from_db_str(row.get::<&str, _>("status"))
            .ok_or_else(|| LobbyError::Storage("invalid room status in database".to_owned()))?,
        config_snapshot: row
            .get::<sqlx::types::Json<RoomConfigRecord>, _>("config_snapshot")
            .0,
        current_match_id: row.get("current_match_id"),
        created_at_unix_ms: row.get::<i64, _>("created_at_unix_ms") as u64,
        updated_at_unix_ms: row.get::<i64, _>("updated_at_unix_ms") as u64,
    })
}

fn room_member_from_row(row: PgRow) -> Result<RoomMemberRecord, LobbyError> {
    Ok(RoomMemberRecord {
        room_id: row.get("room_id"),
        user_id: row.get("user_id"),
        display_name: row.get("display_name"),
        seat: row.get("seat"),
        joined_at_unix_ms: row.get::<i64, _>("joined_at_unix_ms") as u64,
        ready: row.get("ready"),
        kicked_at_unix_ms: row.try_get::<Option<i64>, _>("kicked_at_unix_ms").unwrap_or(None).map(|value| value as u64),
        left_at_unix_ms: row.try_get::<Option<i64>, _>("left_at_unix_ms").unwrap_or(None).map(|value| value as u64),
    })
}

fn match_event_from_row(row: PgRow) -> anyhow::Result<MatchEventRecord> {
    Ok(MatchEventRecord {
        event_id: row.get("event_id"),
        match_id: row.get("match_id"),
        round_id: row.get("round_id"),
        event_seq: row.get("event_seq"),
        event_type: row.get("event_type"),
        actor_user_id: row.get("actor_user_id"),
        actor_seat: row.get("actor_seat"),
        request_id: row.get("request_id"),
        correlation_id: row.get("correlation_id"),
        causation_event_seq: row.get("causation_event_seq"),
        event_payload: row.get::<sqlx::types::Json<Value>, _>("event_payload").0,
        created_at_unix_ms: row.get::<i64, _>("created_at_unix_ms") as u64,
    })
}

fn room_config_to_record(config: &LobbyRoomConfig) -> RoomConfigRecord {
    RoomConfigRecord {
        ruleset_id: config.ruleset_id.clone(),
        seat_count: config.seat_count,
        enable_flower_tiles: config.enable_flower_tiles,
        enable_robbing_kong: config.enable_robbing_kong,
        enable_kong_draw: config.enable_kong_draw,
        reconnect_grace_seconds: config.reconnect_grace_seconds,
        action_timeout_ms: config.action_timeout_ms,
    }
}

fn json_room_config_to_domain(config: RoomConfigRecord) -> LobbyRoomConfig {
    LobbyRoomConfig {
        ruleset_id: config.ruleset_id,
        seat_count: config.seat_count,
        enable_flower_tiles: config.enable_flower_tiles,
        enable_robbing_kong: config.enable_robbing_kong,
        enable_kong_draw: config.enable_kong_draw,
        reconnect_grace_seconds: config.reconnect_grace_seconds,
        action_timeout_ms: config.action_timeout_ms,
    }
}

fn snapshot_to_domain(snapshot: RoomSnapshotRecord) -> Result<LobbyRoom, LobbyError> {
    Ok(LobbyRoom {
        room_id: snapshot.room.room_id,
        room_code: snapshot.room.room_code,
        owner_user_id: snapshot.room.owner_user_id,
        status: match snapshot.room.status {
            RoomStatusRecord::Waiting => LobbyRoomStatus::Waiting,
            RoomStatusRecord::Active => LobbyRoomStatus::Active,
            RoomStatusRecord::Closed => LobbyRoomStatus::Closed,
        },
        config: json_room_config_to_domain(snapshot.room.config_snapshot),
        current_match_id: snapshot.room.current_match_id,
        members: snapshot
            .members
            .into_iter()
            .map(|member| {
                Ok(LobbyMember {
                    user_id: member.user_id,
                    display_name: member.display_name,
                    seat: parse_seat(&member.seat)?,
                    joined_at_unix_ms: member.joined_at_unix_ms,
                    ready: member.ready,
                    kicked_at_unix_ms: member.kicked_at_unix_ms,
                    left_at_unix_ms: member.left_at_unix_ms,
                })
            })
            .collect::<Result<Vec<_>, LobbyError>>()?,
        created_at_unix_ms: snapshot.room.created_at_unix_ms,
    })
}

fn build_seating_snapshot(members: &[RoomMemberRecord]) -> Value {
    let mut seating = serde_json::Map::new();
    for member in members {
        seating.insert(
            member.seat.clone(),
            json!({
                "user_id": member.user_id,
                "display_name": member.display_name,
                "initial_score": 25000,
            }),
        );
    }
    Value::Object(seating)
}

fn parse_room_status(value: &str) -> Result<LobbyRoomStatus, LobbyError> {
    match value {
        "waiting" => Ok(LobbyRoomStatus::Waiting),
        "active" => Ok(LobbyRoomStatus::Active),
        "closed" => Ok(LobbyRoomStatus::Closed),
        _ => Err(LobbyError::Storage("invalid room status in database".to_owned())),
    }
}

fn parse_seat(value: &str) -> Result<Seat, LobbyError> {
    match value {
        "SEAT_EAST" => Ok(Seat::East),
        "SEAT_SOUTH" => Ok(Seat::South),
        "SEAT_WEST" => Ok(Seat::West),
        "SEAT_NORTH" => Ok(Seat::North),
        _ => Err(LobbyError::Storage(format!("invalid seat value {value}"))),
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires PostgreSQL and DATABASE_URL"]
    fn postgres_repository_tests_require_database_url() {
        let _ = DatabaseConfig::from_env().expect("DATABASE_URL should be set for postgres tests");
    }
}
