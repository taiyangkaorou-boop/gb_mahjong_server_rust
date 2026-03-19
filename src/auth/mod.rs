use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::http::{header, HeaderMap};
use hex::ToHex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::db::{UserRecord, UserSessionRecord};

// 第一版网关采用服务端会话模型。
// 这里先用内存存储跑通链路，后续可替换为数据库实现。
const DEFAULT_SESSION_TTL_MS: u64 = 1000 * 60 * 60 * 24 * 30;

#[derive(Clone, Debug, Serialize)]
pub struct AuthenticatedUser {
    // 这是已经通过服务端鉴权后的最小用户视图，
    // 供 HTTP handler、WebSocket 网关和大厅服务复用。
    pub user_id: String,
    pub display_name: String,
    pub auth_provider: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionView {
    // session_token 是客户端后续访问 HTTP 和 WS 的凭证，
    // 服务端内部只保存其哈希，不直接保存明文。
    pub user_id: String,
    pub display_name: String,
    pub session_token: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug)]
pub enum AuthError {
    MissingAuthorization,
    InvalidAuthorization,
    InvalidDisplayName,
    InvalidSession,
}

#[derive(Default)]
struct AuthStore {
    // user/session/token-hash 三张索引表拆开存，便于后续替换成真正持久层时保持接口稳定。
    users: HashMap<String, UserRecord>,
    sessions: HashMap<String, UserSessionRecord>,
    session_token_index: HashMap<String, String>,
}

#[derive(Clone)]
pub struct AuthService {
    // 所有认证读写都集中在这里，避免 HTTP 和 WS 各自维护一套会话逻辑。
    store: Arc<RwLock<AuthStore>>,
    session_ttl_ms: u64,
}

impl Default for AuthService {
    fn default() -> Self {
        Self::new(DEFAULT_SESSION_TTL_MS)
    }
}

impl AuthService {
    pub fn new(session_ttl_ms: u64) -> Self {
        Self {
            store: Arc::new(RwLock::new(AuthStore::default())),
            session_ttl_ms,
        }
    }

    // 游客注册的结果就是“创建用户 + 创建会话”一次完成，
    // 不单独区分注册和登录。
    pub async fn register_guest(
        &self,
        display_name: impl Into<String>,
    ) -> Result<SessionView, AuthError> {
        let display_name = normalize_display_name(display_name.into())?;
        let now = unix_time_ms();
        let user_id = format!("usr_{}", Uuid::new_v4().simple());
        let session_id = format!("sess_{}", Uuid::new_v4().simple());
        let session_token = build_session_token();
        let session_token_hash = hash_session_token(&session_token);
        let expires_at_unix_ms = now + self.session_ttl_ms;

        let user = UserRecord {
            user_id: user_id.clone(),
            display_name: display_name.clone(),
            auth_provider: "guest".to_owned(),
            auth_subject: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            last_seen_at_unix_ms: Some(now),
        };
        let session = UserSessionRecord {
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            session_token_hash: session_token_hash.clone(),
            issued_at_unix_ms: now,
            expires_at_unix_ms,
            revoked_at_unix_ms: None,
            last_seen_at_unix_ms: Some(now),
        };

        let mut store = self.store.write().await;
        store.users.insert(user_id.clone(), user);
        store.sessions.insert(session_id, session);
        store
            .session_token_index
            .insert(session_token_hash, user_id.clone());

        Ok(SessionView {
            user_id,
            display_name,
            session_token,
            expires_at_unix_ms,
        })
    }

    // 刷新会话时采用“旧 token 作废 + 新 token 生效”的轮转策略，
    // 避免一个长期 token 永远有效。
    pub async fn refresh_session(&self, session_token: &str) -> Result<SessionView, AuthError> {
        let user = self.authenticate_session(session_token).await?;
        let now = unix_time_ms();
        let new_session_id = format!("sess_{}", Uuid::new_v4().simple());
        let new_session_token = build_session_token();
        let new_session_token_hash = hash_session_token(&new_session_token);
        let expires_at_unix_ms = now + self.session_ttl_ms;
        let old_session_token_hash = hash_session_token(session_token);

        let mut store = self.store.write().await;
        let Some(old_session_id) = store.session_token_index.remove(&old_session_token_hash) else {
            return Err(AuthError::InvalidSession);
        };

        if let Some(old_session) = store.sessions.get_mut(&old_session_id) {
            old_session.revoked_at_unix_ms = Some(now);
            old_session.last_seen_at_unix_ms = Some(now);
        }

        store.sessions.insert(
            new_session_id.clone(),
            UserSessionRecord {
                session_id: new_session_id,
                user_id: user.user_id.clone(),
                session_token_hash: new_session_token_hash.clone(),
                issued_at_unix_ms: now,
                expires_at_unix_ms,
                revoked_at_unix_ms: None,
                last_seen_at_unix_ms: Some(now),
            },
        );
        store
            .session_token_index
            .insert(new_session_token_hash, user.user_id.clone());

        Ok(SessionView {
            user_id: user.user_id,
            display_name: user.display_name,
            session_token: new_session_token,
            expires_at_unix_ms,
        })
    }

    // WebSocket JoinRoom 和 HTTP Bearer 最终都会落到这个鉴权入口。
    pub async fn authenticate_session(
        &self,
        session_token: &str,
    ) -> Result<AuthenticatedUser, AuthError> {
        let now = unix_time_ms();
        let token_hash = hash_session_token(session_token);

        let mut store = self.store.write().await;
        let Some(user_id) = store.session_token_index.get(&token_hash).cloned() else {
            return Err(AuthError::InvalidSession);
        };
        let Some(session) = store
            .sessions
            .values_mut()
            .find(|session| session.user_id == user_id && session.session_token_hash == token_hash)
        else {
            return Err(AuthError::InvalidSession);
        };

        if session.revoked_at_unix_ms.is_some() || session.expires_at_unix_ms <= now {
            return Err(AuthError::InvalidSession);
        }
        session.last_seen_at_unix_ms = Some(now);

        let Some(user) = store.users.get_mut(&user_id) else {
            return Err(AuthError::InvalidSession);
        };
        user.last_seen_at_unix_ms = Some(now);
        user.updated_at_unix_ms = now;

        Ok(AuthenticatedUser {
            user_id: user.user_id.clone(),
            display_name: user.display_name.clone(),
            auth_provider: user.auth_provider.clone(),
        })
    }

    // HTTP 层统一使用 Authorization: Bearer <token> 头部。
    pub async fn authenticate_bearer(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedUser, AuthError> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Err(AuthError::MissingAuthorization);
        };
        let value = value
            .to_str()
            .map_err(|_| AuthError::InvalidAuthorization)?;
        let Some(token) = value.strip_prefix("Bearer ") else {
            return Err(AuthError::InvalidAuthorization);
        };

        self.authenticate_session(token.trim()).await
    }
}

fn normalize_display_name(display_name: String) -> Result<String, AuthError> {
    // 第一版只做最小校验：
    // 非空，且长度可被大厅和对局 UI 安全展示。
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name.len() > 32 {
        return Err(AuthError::InvalidDisplayName);
    }

    Ok(display_name.to_owned())
}

fn build_session_token() -> String {
    // 目前直接用随机串拼接，后续接数据库时可继续保留 opaque token 语义。
    format!("gst_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn hash_session_token(session_token: &str) -> String {
    // 服务端只保存 token 哈希，避免明文会话凭证落地。
    let mut hasher = Sha256::new();
    hasher.update(session_token.as_bytes());
    hasher.finalize().encode_hex::<String>()
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
