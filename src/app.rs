use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use axum::{routing::get, Router};

use crate::{
    auth::AuthService, db::MatchQueryService, http::api_router, lobby::LobbyService,
    room::RoomManager, ws::ws_handler,
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
    // match 查询服务只读数据库聚合和事件流，不参与实时状态推进。
    match_query_service: MatchQueryService,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(
            RoomManager::default(),
            AuthService::default(),
            LobbyService::default(),
            MatchQueryService::default(),
        )
    }
}

impl AppState {
    pub fn new(
        room_manager: RoomManager,
        auth_service: AuthService,
        lobby_service: LobbyService,
        match_query_service: MatchQueryService,
    ) -> Self {
        Self {
            next_connection_id: Arc::new(AtomicU64::new(1)),
            next_event_seq: Arc::new(AtomicU64::new(1)),
            room_manager,
            auth_service,
            lobby_service,
            match_query_service,
        }
    }

    #[allow(dead_code)]
    pub fn with_room_manager(room_manager: RoomManager) -> Self {
        Self::new(
            room_manager,
            AuthService::default(),
            LobbyService::default(),
            MatchQueryService::default(),
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

    pub fn match_query_service(&self) -> MatchQueryService {
        self.match_query_service.clone()
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
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
    };
    use serde_json::{json, Value};
    use tower::util::ServiceExt;

    use crate::db::{
        MatchEventRecord, MatchEventRepository, MatchQueryService, MatchRecord, MatchStatusRecord,
        NewMatchEventRecord,
    };

    #[derive(Default)]
    struct StaticMatchQueryRepository {
        // 回放查询测试只需要一个极小的静态仓储，
        // 用来验证 HTTP 鉴权和 MatchQueryService 的接线是否正确。
        record: Mutex<Option<MatchRecord>>,
        events: Mutex<Vec<MatchEventRecord>>,
    }

    impl StaticMatchQueryRepository {
        fn seed(&self, record: MatchRecord, events: Vec<MatchEventRecord>) {
            *self
                .record
                .lock()
                .expect("record lock should not be poisoned") = Some(record);
            *self
                .events
                .lock()
                .expect("events lock should not be poisoned") = events;
        }
    }

    #[async_trait]
    impl MatchEventRepository for StaticMatchQueryRepository {
        async fn append_event(&self, _event: NewMatchEventRecord) -> anyhow::Result<()> {
            Ok(())
        }

        async fn list_events(&self, _match_id: &str) -> anyhow::Result<Vec<MatchEventRecord>> {
            Ok(self
                .events
                .lock()
                .expect("events lock should not be poisoned")
                .clone())
        }

        async fn get_match(&self, _match_id: &str) -> anyhow::Result<Option<MatchRecord>> {
            Ok(self
                .record
                .lock()
                .expect("record lock should not be poisoned")
                .clone())
        }
    }

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

    #[tokio::test]
    async fn match_query_routes_return_authorized_match_and_events() {
        let repo = Arc::new(StaticMatchQueryRepository::default());
        let state = AppState::new(
            RoomManager::default(),
            AuthService::default(),
            LobbyService::default(),
            MatchQueryService::new(repo.clone()),
        );
        let session = state
            .auth_service()
            .register_guest("ReplayUser".to_owned())
            .await
            .expect("guest registration should succeed");
        repo.seed(
            MatchRecord {
                match_id: "match-replay-1".to_owned(),
                room_id: "room-replay-1".to_owned(),
                status: MatchStatusRecord::Finished,
                ruleset_id: "gb_mahjong_cn_v1".to_owned(),
                config_snapshot: json!({ "seat_count": 4 }),
                seating_snapshot: json!({
                    "EAST": {
                        "user_id": session.user_id,
                        "display_name": "ReplayUser"
                    }
                }),
                current_round_id: Some("hand-4".to_owned()),
                prevailing_wind: Some("EAST".to_owned()),
                dealer_seat: Some("EAST".to_owned()),
                started_at_unix_ms: Some(1_700_000_000_000u64),
                ended_at_unix_ms: Some(1_700_000_100_000u64),
                created_by_user_id: Some(session.user_id.clone()),
                winner_user_id: Some(session.user_id.clone()),
                last_event_seq: 42,
                final_result: Some(json!({
                    "standings": [{ "user_id": session.user_id, "rank": 1 }]
                })),
                created_at_unix_ms: 1_700_000_000_000u64,
                updated_at_unix_ms: 1_700_000_100_000u64,
            },
            vec![
                MatchEventRecord {
                    event_id: 1,
                    match_id: "match-replay-1".to_owned(),
                    round_id: Some("hand-4".to_owned()),
                    event_seq: 41,
                    event_type: "round_started".to_owned(),
                    actor_user_id: Some(session.user_id.clone()),
                    actor_seat: Some("EAST".to_owned()),
                    request_id: None,
                    correlation_id: None,
                    causation_event_seq: None,
                    event_payload: json!({
                        "phase": "GAME_PHASE_WAITING_DISCARD",
                        "prevailing_wind": "EAST",
                        "dealer_seat": "EAST",
                        "current_turn_seat": "EAST",
                        "hand_number": 4,
                        "dealer_streak": 0,
                        "wall_tiles_remaining": 70,
                        "dead_wall_tiles_remaining": 14,
                        "players": [],
                    }),
                    created_at_unix_ms: 1_700_000_050_000u64,
                },
                MatchEventRecord {
                    event_id: 2,
                    match_id: "match-replay-1".to_owned(),
                    round_id: Some("hand-4".to_owned()),
                    event_seq: 42,
                    event_type: "match_settlement".to_owned(),
                    actor_user_id: Some(session.user_id.clone()),
                    actor_seat: Some("EAST".to_owned()),
                    request_id: None,
                    correlation_id: None,
                    causation_event_seq: None,
                    event_payload: json!({
                        "final_result": {
                            "finished_at_unix_ms": 1_700_000_100_000u64,
                            "standings": [{ "user_id": session.user_id, "rank": 1 }]
                        }
                    }),
                    created_at_unix_ms: 1_700_000_100_000u64,
                },
            ],
        );
        let app = build_router(state);

        let match_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/matches/match-replay-1")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", session.session_token),
                    )
                    .body(Body::empty())
                    .expect("build match request"),
            )
            .await
            .expect("execute match request");
        assert_eq!(match_response.status(), StatusCode::OK);
        let match_body = to_bytes(match_response.into_body(), usize::MAX)
            .await
            .expect("read match body");
        let match_json: Value = serde_json::from_slice(&match_body).expect("parse match json");
        assert_eq!(match_json["match_id"], "match-replay-1");
        assert_eq!(match_json["status"], "Finished");

        let events_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/matches/match-replay-1/events")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", session.session_token),
                    )
                    .body(Body::empty())
                    .expect("build events request"),
            )
            .await
            .expect("execute events request");
        assert_eq!(events_response.status(), StatusCode::OK);
        let events_body = to_bytes(events_response.into_body(), usize::MAX)
            .await
            .expect("read events body");
        let events_json: Value = serde_json::from_slice(&events_body).expect("parse events json");
        assert_eq!(events_json.as_array().unwrap().len(), 2);
        assert_eq!(events_json[0]["event_type"], "round_started");
        assert_eq!(events_json[1]["event_type"], "match_settlement");

        let replay_response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/matches/match-replay-1/replay")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", session.session_token),
                    )
                    .body(Body::empty())
                    .expect("build replay request"),
            )
            .await
            .expect("execute replay request");
        assert_eq!(replay_response.status(), StatusCode::OK);
        let replay_body = to_bytes(replay_response.into_body(), usize::MAX)
            .await
            .expect("read replay body");
        let replay_json: Value = serde_json::from_slice(&replay_body).expect("parse replay json");
        assert_eq!(replay_json["match_record"]["match_id"], "match-replay-1");
        assert_eq!(replay_json["rounds"].as_array().unwrap().len(), 1);
        assert_eq!(
            replay_json["terminal_entry"]["event_type"],
            "match_settlement"
        );
        assert_eq!(replay_json["warnings"].as_array().unwrap().len(), 0);
    }
}
