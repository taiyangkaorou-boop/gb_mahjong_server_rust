use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use axum::{routing::get, Router};

use crate::{room::RoomManager, ws::ws_handler};

#[derive(Clone)]
pub struct AppState {
    next_connection_id: Arc<AtomicU64>,
    next_event_seq: Arc<AtomicU64>,
    room_manager: RoomManager,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            next_connection_id: Arc::new(AtomicU64::new(1)),
            next_event_seq: Arc::new(AtomicU64::new(1)),
            room_manager: RoomManager::default(),
        }
    }
}

impl AppState {
    pub fn with_room_manager(room_manager: RoomManager) -> Self {
        Self {
            next_connection_id: Arc::new(AtomicU64::new(1)),
            next_event_seq: Arc::new(AtomicU64::new(1)),
            room_manager,
        }
    }

    pub fn allocate_connection_id(&self) -> String {
        let next_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        format!("conn-{next_id}")
    }

    pub fn next_event_seq(&self) -> u64 {
        self.next_event_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn room_manager(&self) -> RoomManager {
        self.room_manager.clone()
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
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
        http::{Request, StatusCode},
    };
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
}
