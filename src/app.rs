use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use axum::{routing::get, Router};

use crate::{
    auth::AuthService, http::api_router, lobby::LobbyService, room::RoomManager, ws::ws_handler,
};

#[derive(Clone)]
pub struct AppState {
    // 连接号只用于网关层的短期标识，不参与持久化业务主键。
    next_connection_id: Arc<AtomicU64>,
    // event_seq 是服务端广播顺序号，客户端可用它做断线续传和乱序保护。
    next_event_seq: Arc<AtomicU64>,
    // 房间管理器本身只保存房间 sender，不直接暴露房间状态所有权。
    room_manager: RoomManager,
    // 认证服务只负责账户与会话，不直接感知房间状态机。
    auth_service: AuthService,
    // 大厅服务只负责房主权限、房间成员和 lobby 生命周期。
    lobby_service: LobbyService,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(
            RoomManager::default(),
            AuthService::default(),
            LobbyService::default(),
        )
    }
}

impl AppState {
    pub fn new(
        room_manager: RoomManager,
        auth_service: AuthService,
        lobby_service: LobbyService,
    ) -> Self {
        Self {
            next_connection_id: Arc::new(AtomicU64::new(1)),
            next_event_seq: Arc::new(AtomicU64::new(1)),
            room_manager,
            auth_service,
            lobby_service,
        }
    }

    #[allow(dead_code)]
    pub fn with_room_manager(room_manager: RoomManager) -> Self {
        Self::new(
            room_manager,
            AuthService::default(),
            LobbyService::default(),
        )
    }

    pub fn allocate_connection_id(&self) -> String {
        // 这里不要求严格连续，只要求在当前进程内唯一即可。
        let next_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        format!("conn-{next_id}")
    }

    pub fn next_event_seq(&self) -> u64 {
        // event_seq 仅用于协议层时序，Relaxed 足以满足这里的递增需求。
        self.next_event_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn room_manager(&self) -> RoomManager {
        self.room_manager.clone()
    }

    pub fn auth_service(&self) -> AuthService {
        self.auth_service.clone()
    }

    pub fn lobby_service(&self) -> LobbyService {
        self.lobby_service.clone()
    }
}

pub fn build_router(state: AppState) -> Router {
    // Router 层不感知“牌局怎么跑”，它只负责把连接引导到 ws_handler。
    // 当前骨架只暴露健康检查和 WebSocket 入口，后续 HTTP API 可继续挂在这里。
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .merge(api_router())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
    };
    use serde_json::{json, Value};
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn healthz_route_returns_ok() {
        let app = build_router(AppState::default());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("execute request");

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");

        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn guest_register_and_me_routes_work() {
        let app = build_router(AppState::default());

        let register_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/guest/register")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "display_name": "Alice" }))
                            .expect("serialize register body"),
                    ))
                    .expect("build register request"),
            )
            .await
            .expect("execute register request");

        assert_eq!(register_response.status(), StatusCode::OK);
        let register_body = to_bytes(register_response.into_body(), usize::MAX)
            .await
            .expect("read register response");
        let register_json: Value =
            serde_json::from_slice(&register_body).expect("parse register json");
        let session_token = register_json["session_token"]
            .as_str()
            .expect("session_token should be present");

        let me_response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header(header::AUTHORIZATION, format!("Bearer {session_token}"))
                    .body(Body::empty())
                    .expect("build me request"),
            )
            .await
            .expect("execute me request");

        assert_eq!(me_response.status(), StatusCode::OK);
        let me_body = to_bytes(me_response.into_body(), usize::MAX)
            .await
            .expect("read me response");
        let me_json: Value = serde_json::from_slice(&me_body).expect("parse me json");
        assert_eq!(me_json["display_name"], "Alice");
        assert_eq!(me_json["auth_provider"], "guest");
    }

    #[tokio::test]
    async fn create_room_and_join_room_routes_work() {
        let app = build_router(AppState::default());

        async fn register_guest(app: &Router, display_name: &str) -> Value {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/auth/guest/register")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&json!({ "display_name": display_name }))
                                .expect("serialize register body"),
                        ))
                        .expect("build register request"),
                )
                .await
                .expect("execute register request");
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read register response");
            serde_json::from_slice(&body).expect("parse register json")
        }

        let owner = register_guest(&app, "Owner").await;
        let owner_token = owner["session_token"].as_str().unwrap();
        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms")
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .body(Body::empty())
                    .expect("build create room request"),
            )
            .await
            .expect("execute create room request");
        assert_eq!(create_response.status(), StatusCode::OK);
        let create_body = to_bytes(create_response.into_body(), usize::MAX)
            .await
            .expect("read create room response");
        let create_json: Value = serde_json::from_slice(&create_body).expect("parse create room");
        let room_id = create_json["room_id"].as_str().unwrap().to_owned();
        let room_code = create_json["room_code"].as_str().unwrap().to_owned();

        let guest = register_guest(&app, "Guest").await;
        let guest_token = guest["session_token"].as_str().unwrap();
        let join_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/join")
                    .header(header::AUTHORIZATION, format!("Bearer {guest_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "room_code": room_code }))
                            .expect("serialize join body"),
                    ))
                    .expect("build join request"),
            )
            .await
            .expect("execute join request");
        assert_eq!(join_response.status(), StatusCode::OK);

        let room_response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/rooms/{room_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {guest_token}"))
                    .body(Body::empty())
                    .expect("build get room request"),
            )
            .await
            .expect("execute get room request");
        assert_eq!(room_response.status(), StatusCode::OK);
        let room_body = to_bytes(room_response.into_body(), usize::MAX)
            .await
            .expect("read room response");
        let room_json: Value = serde_json::from_slice(&room_body).expect("parse room json");
        assert_eq!(room_json["room_id"], room_id);
        assert_eq!(room_json["members"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn kick_member_route_removes_guest_from_room() {
        let app = build_router(AppState::default());

        async fn register_guest(app: &Router, display_name: &str) -> Value {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/auth/guest/register")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&json!({ "display_name": display_name }))
                                .expect("serialize register body"),
                        ))
                        .expect("build register request"),
                )
                .await
                .expect("execute register request");
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read register response");
            serde_json::from_slice(&body).expect("parse register json")
        }

        let owner = register_guest(&app, "Owner").await;
        let owner_token = owner["session_token"].as_str().unwrap();
        let owner_id = owner["user_id"].as_str().unwrap().to_owned();
        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms")
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .body(Body::empty())
                    .expect("build create room request"),
            )
            .await
            .expect("execute create room request");
        let create_body = to_bytes(create_response.into_body(), usize::MAX)
            .await
            .expect("read create room response");
        let create_json: Value = serde_json::from_slice(&create_body).expect("parse create room");
        let room_id = create_json["room_id"].as_str().unwrap().to_owned();
        let room_code = create_json["room_code"].as_str().unwrap().to_owned();

        let guest = register_guest(&app, "Guest").await;
        let guest_token = guest["session_token"].as_str().unwrap();
        let guest_id = guest["user_id"].as_str().unwrap().to_owned();
        let join_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/join")
                    .header(header::AUTHORIZATION, format!("Bearer {guest_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "room_code": room_code }))
                            .expect("serialize join body"),
                    ))
                    .expect("build join request"),
            )
            .await
            .expect("execute join request");
        assert_eq!(join_response.status(), StatusCode::OK);

        let kick_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/rooms/{room_id}/kick"))
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "target_user_id": guest_id }))
                            .expect("serialize kick body"),
                    ))
                    .expect("build kick request"),
            )
            .await
            .expect("execute kick request");
        assert_eq!(kick_response.status(), StatusCode::OK);

        let room_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/rooms/{room_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .body(Body::empty())
                    .expect("build get room request"),
            )
            .await
            .expect("execute get room request");
        let room_body = to_bytes(room_response.into_body(), usize::MAX)
            .await
            .expect("read room response");
        let room_json: Value = serde_json::from_slice(&room_body).expect("parse room json");
        assert_eq!(room_json["owner_user_id"], owner_id);
        assert_eq!(room_json["members"].as_array().unwrap().len(), 1);

        let guest_room_response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/rooms/{room_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {guest_token}"))
                    .body(Body::empty())
                    .expect("build guest get room request"),
            )
            .await
            .expect("execute guest get room request");
        assert_eq!(guest_room_response.status(), StatusCode::FORBIDDEN);
    }
}
