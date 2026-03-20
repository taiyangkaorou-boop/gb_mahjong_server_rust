use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::AppState,
    auth::{AuthError, AuthenticatedUser, SessionView},
    db::{MatchEventRecord, MatchRecord, MatchReplayRecord},
    lobby::{LobbyError, LobbyRoomView},
};

// HTTP 层只承载登录注册、建房/加房/踢人等大厅能力。
// 正式对局仍然由 WebSocket + Protobuf 驱动。
#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum ApiError {
    // 这里统一把领域错误映射成 HTTP 语义，避免 handler 自己拼状态码。
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    BadRequest(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ApiError::Unauthorized(message) => (StatusCode::UNAUTHORIZED, "unauthorized", message),
            ApiError::Forbidden(message) => (StatusCode::FORBIDDEN, "forbidden", message),
            ApiError::NotFound(message) => (StatusCode::NOT_FOUND, "not_found", message),
            ApiError::Conflict(message) => (StatusCode::CONFLICT, "conflict", message),
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", message),
            ApiError::Internal(message) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
            }
        };

        (status, Json(ErrorBody { code, message })).into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct GuestRegisterRequest {
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct JoinRoomHttpRequest {
    pub room_code: String,
}

#[derive(Debug, Deserialize)]
pub struct KickRoomMemberRequest {
    pub target_user_id: String,
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub user_id: String,
    pub display_name: String,
    pub auth_provider: String,
}

pub fn api_router() -> Router<AppState> {
    // 第一版网关全部走 JSON，方便浏览器和调试工具直接调用。
    Router::new()
        .route("/api/v1/auth/guest/register", post(register_guest))
        .route("/api/v1/auth/session/refresh", post(refresh_session))
        .route("/api/v1/me", get(me))
        .route("/api/v1/rooms", post(create_room))
        .route("/api/v1/rooms/join", post(join_room))
        .route("/api/v1/rooms/:room_id", get(get_room))
        .route("/api/v1/rooms/:room_id/leave", post(leave_room))
        .route("/api/v1/rooms/:room_id/kick", post(kick_member))
        .route("/api/v1/rooms/:room_id/disband", post(disband_room))
        .route("/api/v1/matches/:match_id", get(get_match))
        .route("/api/v1/matches/:match_id/events", get(list_match_events))
        .route("/api/v1/matches/:match_id/replay", get(get_match_replay))
}

async fn register_guest(
    State(state): State<AppState>,
    Json(request): Json<GuestRegisterRequest>,
) -> Result<Json<SessionView>, ApiError> {
    // 注册游客的同时直接签发会话，减少前端额外的一跳登录。
    let session = state
        .auth_service()
        .register_guest(request.display_name)
        .await
        .map_err(map_auth_error)?;

    Ok(Json(session))
}

async fn refresh_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, ApiError> {
    // 刷新接口走 Bearer 头，不单独再发旧 token 字段。
    let token = bearer_token(&headers).map_err(map_auth_error)?;
    let session = state
        .auth_service()
        .refresh_session(&token)
        .await
        .map_err(map_auth_error)?;

    Ok(Json(session))
}

async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MeResponse>, ApiError> {
    let user = authenticate_request(&state, &headers).await?;
    Ok(Json(MeResponse {
        user_id: user.user_id,
        display_name: user.display_name,
        auth_provider: user.auth_provider,
    }))
}

async fn create_room(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LobbyRoomView>, ApiError> {
    // 建房成功后，客户端再用 room_id + session_token 走 WebSocket JoinRoom。
    let user = authenticate_request(&state, &headers).await?;
    let room = state
        .lobby_service()
        .create_room(&user.user_id, &user.display_name)
        .await
        .map_err(map_lobby_error)?;

    Ok(Json(room))
}

async fn join_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JoinRoomHttpRequest>,
) -> Result<Json<LobbyRoomView>, ApiError> {
    let user = authenticate_request(&state, &headers).await?;
    let room = state
        .lobby_service()
        .join_room(&user.user_id, &user.display_name, &request.room_code)
        .await
        .map_err(map_lobby_error)?;

    Ok(Json(room))
}

async fn get_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Result<Json<LobbyRoomView>, ApiError> {
    let user = authenticate_request(&state, &headers).await?;
    let room = state
        .lobby_service()
        .get_room_for_user(&user.user_id, &room_id)
        .await
        .map_err(map_lobby_error)?;

    Ok(Json(room))
}

async fn leave_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Result<Json<Option<LobbyRoomView>>, ApiError> {
    // HTTP 大厅变更后，需要顺手通知 RoomManager 清掉 waiting 阶段里可能残留的玩家连接。
    let user = authenticate_request(&state, &headers).await?;
    let room = state
        .lobby_service()
        .leave_room(&user.user_id, &room_id)
        .await
        .map_err(map_lobby_error)?;
    state
        .room_manager()
        .dispatch_remove_player(room_id, user.user_id)
        .await;

    Ok(Json(room))
}

async fn kick_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Json(request): Json<KickRoomMemberRequest>,
) -> Result<Json<LobbyRoomView>, ApiError> {
    let user = authenticate_request(&state, &headers).await?;
    let room = state
        .lobby_service()
        .kick_member(&user.user_id, &room_id, &request.target_user_id)
        .await
        .map_err(map_lobby_error)?;
    state
        .room_manager()
        .dispatch_remove_player(room_id, request.target_user_id)
        .await;

    Ok(Json(room))
}

async fn disband_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // 解散房间前先拿成员快照，便于把仍连着的 waiting 房间连接一起清掉。
    let user = authenticate_request(&state, &headers).await?;
    let members = state
        .lobby_service()
        .snapshot_room_member_records(&room_id)
        .await;
    state
        .lobby_service()
        .disband_room(&user.user_id, &room_id)
        .await
        .map_err(map_lobby_error)?;
    for member in members {
        state
            .room_manager()
            .dispatch_remove_player(room_id.clone(), member.user_id)
            .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn get_match(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(match_id): Path<String>,
) -> Result<Json<MatchRecord>, ApiError> {
    // 对局摘要查询走只读数据库路径，不经过 room task。
    let user = authenticate_request(&state, &headers).await?;
    let Some(record) = state
        .match_query_service()
        .get_match_for_user(&user.user_id, &match_id)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
    else {
        return Err(ApiError::NotFound("match was not found".to_owned()));
    };

    Ok(Json(record))
}

async fn list_match_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(match_id): Path<String>,
) -> Result<Json<Vec<MatchEventRecord>>, ApiError> {
    // replay 读取统一按 event_seq 升序返回，客户端无需自己重新排序。
    let user = authenticate_request(&state, &headers).await?;
    let Some(events) = state
        .match_query_service()
        .list_events_for_user(&user.user_id, &match_id)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
    else {
        return Err(ApiError::NotFound("match was not found".to_owned()));
    };

    Ok(Json(events))
}

async fn get_match_replay(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(match_id): Path<String>,
) -> Result<Json<MatchReplayRecord>, ApiError> {
    // replay 是对 raw match_events 的容错型读模型，
    // 第一版只开放给已结束对局，避免把进行中的私有牌过程暴露出去。
    let user = authenticate_request(&state, &headers).await?;
    let Some(replay) = state
        .match_query_service()
        .build_replay_for_user(&user.user_id, &match_id)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
    else {
        return Err(ApiError::NotFound("match replay was not found".to_owned()));
    };

    Ok(Json(replay))
}

async fn authenticate_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, ApiError> {
    // HTTP handler 统一走这一层，避免每个接口自己手写 Bearer 校验。
    state
        .auth_service()
        .authenticate_bearer(headers)
        .await
        .map_err(map_auth_error)
}

fn bearer_token(headers: &HeaderMap) -> Result<String, AuthError> {
    // 先把 Authorization 头解析干净，再把 token 交给 AuthService 做真正鉴权。
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(AuthError::MissingAuthorization);
    };
    let value = value
        .to_str()
        .map_err(|_| AuthError::InvalidAuthorization)?;
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(AuthError::InvalidAuthorization);
    };
    Ok(token.trim().to_owned())
}

fn map_auth_error(error: AuthError) -> ApiError {
    // 认证错误尽量压成少量稳定的 HTTP 语义，便于前端统一处理。
    match error {
        AuthError::MissingAuthorization | AuthError::InvalidAuthorization => {
            ApiError::Unauthorized("missing or invalid authorization header".to_owned())
        }
        AuthError::InvalidDisplayName => {
            ApiError::BadRequest("display_name must be between 1 and 32 characters".to_owned())
        }
        AuthError::InvalidSession => {
            ApiError::Unauthorized("session is invalid or expired".to_owned())
        }
        AuthError::Storage(message) => ApiError::Internal(message),
    }
}

fn map_lobby_error(error: LobbyError) -> ApiError {
    // 大厅错误与 HTTP 状态的映射是产品语义的一部分：
    // 例如房满/已开局属于冲突，不是权限错误。
    match error {
        LobbyError::AlreadyInRoom => {
            ApiError::Conflict("user is already in another room".to_owned())
        }
        LobbyError::RoomNotFound | LobbyError::InvalidRoomCode => {
            ApiError::NotFound("room was not found".to_owned())
        }
        LobbyError::RoomFull => ApiError::Conflict("room is already full".to_owned()),
        LobbyError::RoomAlreadyActive => ApiError::Conflict(
            "room has already started and no longer accepts lobby mutations".to_owned(),
        ),
        LobbyError::NotRoomOwner => {
            ApiError::Forbidden("only the room owner may perform this action".to_owned())
        }
        LobbyError::CannotKickSelf => {
            ApiError::Conflict("room owner cannot kick themselves".to_owned())
        }
        LobbyError::NotRoomMember => {
            ApiError::Forbidden("user is not a member of this room".to_owned())
        }
        LobbyError::Storage(message) => ApiError::Internal(message),
    }
}
