use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::StreamExt;
use prost::Message as ProstMessage;
use tracing::{debug, info, warn};
use tokio::sync::mpsc;

use crate::{
    app::AppState,
    proto::client::{
        client_frame, server_frame, ActionRejected, ClientFrame, Heartbeat, Pong, RejectCode,
        ServerFrame,
    },
    room::ConnectionHandle,
};

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let connection = ConnectionSession::new(state.allocate_connection_id());
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
    let mut connection = connection.with_outbound(outbound_tx);

    info!(connection_id = %connection.connection_id, "websocket connection established");

    loop {
        tokio::select! {
            outbound_frame = outbound_rx.recv() => {
                let Some(frame) = outbound_frame else {
                    debug!(connection_id = %connection.connection_id, "all websocket outbound senders dropped");
                    break;
                };

                if let Err(error) = send_server_frame(&mut socket, frame).await {
                    warn!(connection_id = %connection.connection_id, ?error, "failed to send protobuf response");
                    break;
                }
            }
            message_result = socket.next() => {
                let Some(message_result) = message_result else {
                    break;
                };

                match message_result {
                    Ok(Message::Binary(payload)) => {
                        match ClientFrame::decode(payload.as_ref()) {
                            Ok(frame) => handle_client_frame(&state, &mut connection, frame).await,
                            Err(error) => {
                                warn!(connection_id = %connection.connection_id, ?error, "failed to decode protobuf ClientFrame");
                                connection.send_frame(build_action_rejected(
                                    &state,
                                    String::new(),
                                    RejectCode::InvalidRequest,
                                    "invalid protobuf binary frame",
                                ));
                            }
                        }
                    }
                    Ok(Message::Text(_)) => {
                        connection.send_frame(build_action_rejected(
                            &state,
                            String::new(),
                            RejectCode::InvalidRequest,
                            "text frames are not supported; send protobuf binary frames",
                        ));
                    }
                    Ok(Message::Ping(payload)) => {
                        if let Err(error) = socket.send(Message::Pong(payload)).await {
                            warn!(connection_id = %connection.connection_id, ?error, "failed to answer websocket ping");
                            break;
                        }
                    }
                    Ok(Message::Pong(_)) => {
                        debug!(connection_id = %connection.connection_id, "received websocket pong");
                    }
                    Ok(Message::Close(frame)) => {
                        info!(connection_id = %connection.connection_id, ?frame, "client closed websocket");
                        break;
                    }
                    Err(error) => {
                        warn!(connection_id = %connection.connection_id, ?error, "websocket receive error");
                        break;
                    }
                }
            }
        }
    }

    if let Some(room_id) = connection.current_room_id.as_deref() {
        state
            .room_manager()
            .disconnect(room_id, &connection.connection_id)
            .await;
    }

    info!(connection_id = %connection.connection_id, "websocket connection closed");
}

async fn handle_client_frame(
    state: &AppState,
    connection: &mut ConnectionSession,
    frame: ClientFrame,
) {
    match frame.payload {
        Some(client_frame::Payload::Heartbeat(heartbeat)) => {
            info!(connection_id = %connection.connection_id, request_id = %frame.request_id, "decoded heartbeat frame");
            connection.send_frame(build_pong(state, frame.request_id, heartbeat));
        }
        Some(client_frame::Payload::JoinRoom(join_room)) => {
            info!(
                connection_id = %connection.connection_id,
                request_id = %frame.request_id,
                room_id = %join_room.room_id,
                user_id = %join_room.user_id,
                "decoded JoinRoomRequest",
            );

            connection.current_room_id = Some(join_room.room_id.clone());
            state
                .room_manager()
                .dispatch_join(frame.request_id, connection.handle(), join_room)
                .await;
        }
        Some(client_frame::Payload::Ready(ready)) => {
            info!(
                connection_id = %connection.connection_id,
                request_id = %frame.request_id,
                ready = ready.ready,
                "decoded ReadyRequest",
            );

            let Some(room_id) = connection.current_room_id.clone() else {
                connection.send_frame(build_action_rejected(
                    state,
                    frame.request_id,
                    RejectCode::NotInRoom,
                    "connection must join a room before ReadyRequest",
                ));
                return;
            };

            state
                .room_manager()
                .dispatch_ready(room_id, frame.request_id, connection.handle(), ready)
                .await;
        }
        Some(client_frame::Payload::ResumeSession(resume)) => {
            info!(
                connection_id = %connection.connection_id,
                request_id = %frame.request_id,
                room_id = %resume.room_id,
                user_id = %resume.user_id,
                last_received_event_seq = resume.last_received_event_seq,
                "decoded ResumeSessionRequest",
            );

            connection.current_room_id = Some(resume.room_id.clone());
            state
                .room_manager()
                .dispatch_resume(frame.request_id, connection.handle(), resume)
                .await;
        }
        Some(client_frame::Payload::PlayerAction(action)) => {
            info!(
                connection_id = %connection.connection_id,
                request_id = %frame.request_id,
                room_id = %action.room_id,
                match_id = %action.match_id,
                round_id = %action.round_id,
                expected_event_seq = action.expected_event_seq,
                action_window_id = action.action_window_id,
                "decoded PlayerActionRequest",
            );

            connection.current_room_id = Some(action.room_id.clone());
            state
                .room_manager()
                .dispatch_player_action(frame.request_id, connection.handle(), action)
                .await;
        }
        None => connection.send_frame(build_action_rejected(
            state,
            frame.request_id,
            RejectCode::InvalidRequest,
            "ClientFrame.payload must be set",
        )),
    }
}

fn build_pong(state: &AppState, request_id: String, heartbeat: Heartbeat) -> ServerFrame {
    let event_seq = state.next_event_seq();

    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::Pong(Pong {
            request_id,
            server_time_ms: unix_time_ms(),
            echoed_client_time_ms: heartbeat.client_time_ms,
            latest_event_seq: event_seq,
        })),
    }
}

fn build_action_rejected(
    state: &AppState,
    request_id: String,
    reject_code: RejectCode,
    message: impl Into<String>,
) -> ServerFrame {
    let event_seq = state.next_event_seq();

    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::ActionRejected(ActionRejected {
            request_id,
            reject_code: reject_code as i32,
            message: message.into(),
            expected_event_seq: 0,
            actual_event_seq: 0,
            action_window_id: 0,
        })),
    }
}

async fn send_server_frame(socket: &mut WebSocket, frame: ServerFrame) -> Result<()> {
    let mut buffer = Vec::with_capacity(frame.encoded_len());
    frame.encode(&mut buffer).context("encode ServerFrame")?;

    socket
        .send(Message::Binary(buffer.into()))
        .await
        .context("send websocket binary frame")?;

    Ok(())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

struct ConnectionSession {
    connection_id: String,
    outbound_tx: Option<mpsc::UnboundedSender<ServerFrame>>,
    current_room_id: Option<String>,
}

impl ConnectionSession {
    fn new(connection_id: String) -> Self {
        Self {
            connection_id,
            outbound_tx: None,
            current_room_id: None,
        }
    }

    fn with_outbound(mut self, outbound_tx: mpsc::UnboundedSender<ServerFrame>) -> Self {
        self.outbound_tx = Some(outbound_tx);
        self
    }

    fn handle(&self) -> ConnectionHandle {
        ConnectionHandle::new(
            self.connection_id.clone(),
            self.outbound_tx
                .as_ref()
                .expect("outbound channel should be installed before use")
                .clone(),
        )
    }

    fn send_frame(&self, frame: ServerFrame) {
        self.handle().send_frame(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::client::{client_frame, server_frame};
    use tokio::sync::mpsc;

    fn test_connection() -> (ConnectionSession, mpsc::UnboundedReceiver<ServerFrame>) {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        (
            ConnectionSession::new("conn-test".to_owned()).with_outbound(outbound_tx),
            outbound_rx,
        )
    }

    #[tokio::test]
    async fn heartbeat_frame_returns_pong() {
        let state = AppState::default();
        let (mut connection, mut outbound_rx) = test_connection();
        let frame = ClientFrame {
            request_id: "req-heartbeat".to_owned(),
            payload: Some(client_frame::Payload::Heartbeat(Heartbeat {
                client_time_ms: 12345,
                last_received_event_seq: 9,
            })),
        };

        handle_client_frame(&state, &mut connection, frame).await;
        let response = outbound_rx.recv().await.expect("pong frame should be queued");

        match response.payload {
            Some(server_frame::Payload::Pong(pong)) => {
                assert_eq!(pong.request_id, "req-heartbeat");
                assert_eq!(pong.echoed_client_time_ms, 12345);
                assert_eq!(pong.latest_event_seq, 1);
                assert!(pong.server_time_ms > 0);
            }
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_payload_returns_action_rejected() {
        let state = AppState::default();
        let (mut connection, mut outbound_rx) = test_connection();
        let frame = ClientFrame {
            request_id: "req-empty".to_owned(),
            payload: None,
        };

        handle_client_frame(&state, &mut connection, frame).await;
        let response = outbound_rx
            .recv()
            .await
            .expect("action rejected frame should be queued");

        match response.payload {
            Some(server_frame::Payload::ActionRejected(rejected)) => {
                assert_eq!(rejected.request_id, "req-empty");
                assert_eq!(rejected.reject_code, RejectCode::InvalidRequest as i32);
                assert_eq!(rejected.message, "ClientFrame.payload must be set");
            }
            other => panic!("expected ActionRejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn join_room_frame_is_routed_into_room_manager() {
        let state = AppState::default();
        let (mut connection, mut outbound_rx) = test_connection();
        let frame = ClientFrame {
            request_id: "req-join".to_owned(),
            payload: Some(client_frame::Payload::JoinRoom(
                crate::proto::client::JoinRoomRequest {
                    room_id: "room-ws".to_owned(),
                    user_id: "user-ws".to_owned(),
                    session_token: String::new(),
                    display_name: "WebSocket Tester".to_owned(),
                    client_version: "test".to_owned(),
                },
            )),
        };

        handle_client_frame(&state, &mut connection, frame).await;

        assert_eq!(connection.current_room_id.as_deref(), Some("room-ws"));

        let join_response = outbound_rx
            .recv()
            .await
            .expect("join response should be queued");

        match join_response.payload {
            Some(server_frame::Payload::JoinRoom(response)) => {
                assert!(response.accepted);
                assert_eq!(response.room_id, "room-ws");
                assert_eq!(response.match_id, "match-room-ws");
            }
            other => panic!("expected JoinRoomResponse, got {other:?}"),
        }
    }
}
