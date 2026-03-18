use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

use crate::{
    engine::RuleEngineHandle,
    proto::{
        client::{
            player_action_request, server_frame, ActionRejected, GamePhase, GameSnapshot,
            JoinRoomRequest, JoinRoomResponse, PlayerActionRequest, PlayerConnectionChanged,
            PlayerState, PlayerStatus, ReadyRequest, RejectCode, ResumeSessionRequest,
            RoomConfig, Seat, SelfHandState, ServerFrame, SyncReason, SyncState, Tile,
        },
        engine::{self as engine_proto, candidate_action},
    },
};

const ROOM_COMMAND_BUFFER: usize = 256;
const RECONNECT_GRACE_MS: u64 = 30_000;
const INITIAL_SCORE: i64 = 25_000;
const SEAT_ORDER: [Seat; 4] = [Seat::East, Seat::South, Seat::West, Seat::North];

pub type ConnectionSender = mpsc::UnboundedSender<ServerFrame>;

#[derive(Clone)]
pub struct ConnectionHandle {
    connection_id: String,
    outbound_tx: ConnectionSender,
}

impl ConnectionHandle {
    pub fn new(connection_id: String, outbound_tx: ConnectionSender) -> Self {
        Self {
            connection_id,
            outbound_tx,
        }
    }

    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    pub fn send_frame(&self, frame: ServerFrame) {
        if let Err(error) = self.outbound_tx.send(frame) {
            warn!(
                connection_id = %self.connection_id,
                ?error,
                "failed to enqueue frame for websocket connection"
            );
        }
    }
}

#[derive(Clone, Default)]
pub struct RoomManager {
    rooms: Arc<RwLock<HashMap<String, RoomHandle>>>,
    rule_engine: RuleEngineHandle,
}

#[derive(Clone)]
struct RoomHandle {
    command_tx: mpsc::Sender<RoomCommand>,
}

enum RoomCommand {
    Join {
        request_id: String,
        connection: ConnectionHandle,
        request: JoinRoomRequest,
    },
    Ready {
        request_id: String,
        connection_id: String,
        request: ReadyRequest,
    },
    ResumeSession {
        request_id: String,
        connection: ConnectionHandle,
        request: ResumeSessionRequest,
    },
    PlayerAction {
        request_id: String,
        connection_id: String,
        request: PlayerActionRequest,
    },
    Disconnect {
        connection_id: String,
    },
}

impl RoomManager {
    pub fn with_rule_engine(rule_engine: RuleEngineHandle) -> Self {
        Self {
            rooms: Arc::new(RwLock::new(HashMap::new())),
            rule_engine,
        }
    }

    pub async fn dispatch_join(
        &self,
        request_id: String,
        connection: ConnectionHandle,
        request: JoinRoomRequest,
    ) {
        let room_id = request.room_id.clone();
        let sender = self.get_or_create_room_sender(room_id.clone()).await;

        self.send_command(
            sender,
            RoomCommand::Join {
                request_id,
                connection: connection.clone(),
                request,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn dispatch_ready(
        &self,
        room_id: String,
        request_id: String,
        connection: ConnectionHandle,
        request: ReadyRequest,
    ) {
        let Some(sender) = self.get_room_sender(&room_id).await else {
            connection.send_frame(build_action_rejected(
                0,
                request_id,
                RejectCode::RoomNotFound,
                "room task not found for ReadyRequest",
                0,
                0,
            ));
            return;
        };

        self.send_command(
            sender,
            RoomCommand::Ready {
                request_id,
                connection_id: connection.connection_id().to_owned(),
                request,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn dispatch_resume(
        &self,
        request_id: String,
        connection: ConnectionHandle,
        request: ResumeSessionRequest,
    ) {
        let room_id = request.room_id.clone();
        let Some(sender) = self.get_room_sender(&room_id).await else {
            connection.send_frame(build_action_rejected(
                0,
                request_id,
                RejectCode::RoomNotFound,
                "room task not found for ResumeSessionRequest",
                0,
                0,
            ));
            return;
        };

        self.send_command(
            sender,
            RoomCommand::ResumeSession {
                request_id,
                connection: connection.clone(),
                request,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn dispatch_player_action(
        &self,
        request_id: String,
        connection: ConnectionHandle,
        request: PlayerActionRequest,
    ) {
        let room_id = request.room_id.clone();
        let Some(sender) = self.get_room_sender(&room_id).await else {
            connection.send_frame(build_action_rejected(
                0,
                request_id,
                RejectCode::RoomNotFound,
                "room task not found for PlayerActionRequest",
                request.expected_event_seq,
                request.action_window_id,
            ));
            return;
        };

        self.send_command(
            sender,
            RoomCommand::PlayerAction {
                request_id,
                connection_id: connection.connection_id().to_owned(),
                request,
            },
            Some((connection, room_id)),
        )
        .await;
    }

    pub async fn disconnect(&self, room_id: &str, connection_id: &str) {
        let Some(sender) = self.get_room_sender(room_id).await else {
            return;
        };

        if let Err(error) = sender
            .send(RoomCommand::Disconnect {
                connection_id: connection_id.to_owned(),
            })
            .await
        {
            debug!(
                %room_id,
                %connection_id,
                ?error,
                "room task already stopped before disconnect command"
            );
        }
    }

    async fn send_command(
        &self,
        sender: mpsc::Sender<RoomCommand>,
        command: RoomCommand,
        fallback: Option<(ConnectionHandle, String)>,
    ) {
        if let Err(error) = sender.send(command).await {
            if let Some((connection, room_id)) = fallback {
                connection.send_frame(build_action_rejected(
                    0,
                    String::new(),
                    RejectCode::InternalError,
                    format!("failed to enqueue command for room {room_id}: {error}"),
                    0,
                    0,
                ));
            }
        }
    }

    async fn get_room_sender(&self, room_id: &str) -> Option<mpsc::Sender<RoomCommand>> {
        let rooms = self.rooms.read().await;
        rooms.get(room_id).map(|handle| handle.command_tx.clone())
    }

    async fn get_or_create_room_sender(&self, room_id: String) -> mpsc::Sender<RoomCommand> {
        if let Some(sender) = self.get_room_sender(&room_id).await {
            return sender;
        }

        let mut rooms = self.rooms.write().await;

        if let Some(handle) = rooms.get(&room_id) {
            return handle.command_tx.clone();
        }

        let (command_tx, command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let room_state = RoomState::new(room_id.clone(), self.rule_engine.clone());

        info!(%room_id, "spawning room task");
        tokio::spawn(run_room_task(room_state, command_rx));

        rooms.insert(
            room_id,
            RoomHandle {
                command_tx: command_tx.clone(),
            },
        );

        command_tx
    }
}

async fn run_room_task(mut room_state: RoomState, mut command_rx: mpsc::Receiver<RoomCommand>) {
    info!(room_id = %room_state.room_id, "room task started");

    while let Some(command) = command_rx.recv().await {
        room_state.handle_command(command).await;
    }

    info!(room_id = %room_state.room_id, "room task stopped");
}

struct RoomState {
    rule_engine: RuleEngineHandle,
    room_id: String,
    match_id: String,
    room_config: RoomConfig,
    phase: GamePhase,
    prevailing_wind: Seat,
    dealer_seat: Seat,
    current_turn_seat: Seat,
    hand_number: u32,
    dealer_streak: u32,
    wall_tiles_remaining: u32,
    dead_wall_tiles_remaining: u32,
    next_event_seq: u64,
    players_by_seat: HashMap<Seat, PlayerSlot>,
    user_to_seat: HashMap<String, Seat>,
    connection_to_user: HashMap<String, String>,
}

struct ActionValidationError {
    reject_code: RejectCode,
    message: String,
}

impl ActionValidationError {
    fn new(reject_code: RejectCode, message: impl Into<String>) -> Self {
        Self {
            reject_code,
            message: message.into(),
        }
    }
}

struct PlayerSlot {
    user_id: String,
    display_name: String,
    seat: Seat,
    status: PlayerStatus,
    score: i64,
    connected: bool,
    auto_play_enabled: bool,
    connection_id: Option<String>,
    outbound_tx: Option<ConnectionSender>,
    resume_token: String,
}

impl RoomState {
    fn new(room_id: String, rule_engine: RuleEngineHandle) -> Self {
        Self {
            rule_engine,
            match_id: format!("match-{room_id}"),
            room_id,
            room_config: RoomConfig {
                ruleset_id: "gb_mahjong_cn_v1".to_owned(),
                seat_count: 4,
                enable_flower_tiles: true,
                enable_robbing_kong: true,
                enable_kong_draw: true,
                reconnect_grace_seconds: (RECONNECT_GRACE_MS / 1000) as u32,
                action_timeout_ms: 15_000,
            },
            phase: GamePhase::Lobby,
            prevailing_wind: Seat::East,
            dealer_seat: Seat::East,
            current_turn_seat: Seat::East,
            hand_number: 1,
            dealer_streak: 0,
            wall_tiles_remaining: 144,
            dead_wall_tiles_remaining: 14,
            next_event_seq: 1,
            players_by_seat: HashMap::new(),
            user_to_seat: HashMap::new(),
            connection_to_user: HashMap::new(),
        }
    }

    async fn handle_command(&mut self, command: RoomCommand) {
        match command {
            RoomCommand::Join {
                request_id,
                connection,
                request
            } => self.handle_join(request_id, connection, request),
            RoomCommand::Ready {
                request_id,
                connection_id,
                request
            } => self.handle_ready(request_id, connection_id, request),
            RoomCommand::ResumeSession {
                request_id,
                connection,
                request
            } => self.handle_resume(request_id, connection, request),
            RoomCommand::PlayerAction {
                request_id,
                connection_id,
                request,
            } => self.handle_player_action(request_id, connection_id, request).await,
            RoomCommand::Disconnect { connection_id } => self.handle_disconnect(connection_id),
        }
    }

    fn handle_join(
        &mut self,
        request_id: String,
        connection: ConnectionHandle,
        request: JoinRoomRequest,
    ) {
        if let Some(existing_seat) = self.user_to_seat.get(&request.user_id).copied() {
            connection.send_frame(build_join_room_response(
                self.allocate_event_seq(),
                request_id,
                false,
                RejectCode::AlreadyJoined,
                self.room_id.clone(),
                self.match_id.clone(),
                existing_seat,
                connection.connection_id().to_owned(),
                String::new(),
                Some(self.room_config.clone()),
                "user already joined this room",
            ));
            return;
        }

        let Some(seat) = self.next_open_seat() else {
            connection.send_frame(build_join_room_response(
                self.allocate_event_seq(),
                request_id,
                false,
                RejectCode::SeatUnavailable,
                self.room_id.clone(),
                self.match_id.clone(),
                Seat::Unspecified,
                connection.connection_id().to_owned(),
                String::new(),
                Some(self.room_config.clone()),
                "room is full",
            ));
            return;
        };

        let user_id = request.user_id.clone();
        let resume_token = format!("resume-{}-{user_id}", self.room_id);

        self.connection_to_user
            .insert(connection.connection_id().to_owned(), user_id.clone());
        self.user_to_seat.insert(user_id.clone(), seat);
        self.players_by_seat.insert(
            seat,
            PlayerSlot {
                user_id,
                display_name: request.display_name,
                seat,
                status: PlayerStatus::Joined,
                score: INITIAL_SCORE,
                connected: true,
                auto_play_enabled: false,
                connection_id: Some(connection.connection_id().to_owned()),
                outbound_tx: Some(connection.outbound_tx.clone()),
                resume_token: resume_token.clone(),
            },
        );

        connection.send_frame(build_join_room_response(
            self.allocate_event_seq(),
            request_id,
            true,
            RejectCode::Unspecified,
            self.room_id.clone(),
            self.match_id.clone(),
            seat,
            connection.connection_id().to_owned(),
            resume_token,
            Some(self.room_config.clone()),
            "joined room task successfully",
        ));

        self.broadcast_sync_state(SyncReason::ServerResync);
        self.broadcast_connection_changed(seat, true);
    }

    fn handle_ready(&mut self, request_id: String, connection_id: String, request: ReadyRequest) {
        let Some(seat) = self.seat_for_connection(&connection_id) else {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::NotInRoom,
                "connection is not registered in this room",
                0,
                0,
            );
            return;
        };

        if let Some(player) = self.players_by_seat.get_mut(&seat) {
            player.status = if request.ready {
                PlayerStatus::Ready
            } else {
                PlayerStatus::Joined
            };
        }

        let all_ready = self.players_by_seat.len() == self.room_config.seat_count as usize
            && self
                .players_by_seat
                .values()
                .all(|player| player.status == PlayerStatus::Ready);

        self.phase = if all_ready {
            GamePhase::Dealing
        } else {
            GamePhase::Lobby
        };

        self.broadcast_sync_state(if all_ready {
            SyncReason::RoundStart
        } else {
            SyncReason::ServerResync
        });
    }

    fn handle_resume(
        &mut self,
        request_id: String,
        connection: ConnectionHandle,
        request: ResumeSessionRequest,
    ) {
        let Some(seat) = self.user_to_seat.get(&request.user_id).copied() else {
            connection.send_frame(build_action_rejected(
                self.allocate_event_seq(),
                request_id,
                RejectCode::NotInRoom,
                "user is not known in this room",
                request.last_received_event_seq,
                0,
            ));
            return;
        };

        let Some(existing_resume_token) = self
            .players_by_seat
            .get(&seat)
            .map(|player| player.resume_token.clone())
        else {
            connection.send_frame(build_action_rejected(
                self.allocate_event_seq(),
                request_id,
                RejectCode::InternalError,
                "room state lost player seat mapping",
                request.last_received_event_seq,
                0,
            ));
            return;
        };

        if existing_resume_token != request.resume_token {
            connection.send_frame(build_action_rejected(
                self.allocate_event_seq(),
                request_id,
                RejectCode::AuthFailed,
                "resume token does not match room state",
                request.last_received_event_seq,
                0,
            ));
            return;
        }

        let Some(player) = self.players_by_seat.get_mut(&seat) else {
            connection.send_frame(build_action_rejected(
                self.allocate_event_seq(),
                request_id,
                RejectCode::InternalError,
                "room state lost player seat mapping during resume update",
                request.last_received_event_seq,
                0,
            ));
            return;
        };

        let user_id = player.user_id.clone();
        if let Some(old_connection_id) = player
            .connection_id
            .replace(connection.connection_id().to_owned())
        {
            self.connection_to_user.remove(&old_connection_id);
        }

        self.connection_to_user
            .insert(connection.connection_id().to_owned(), user_id);
        player.connected = true;
        player.outbound_tx = Some(connection.outbound_tx.clone());

        let event_seq = self.allocate_event_seq();
        let sync_frame = self.build_sync_state_for(seat, event_seq, SyncReason::Reconnect);
        connection.send_frame(sync_frame);
        self.broadcast_connection_changed(seat, true);
    }

    async fn handle_player_action(
        &mut self,
        request_id: String,
        connection_id: String,
        request: PlayerActionRequest,
    ) {
        let Some(seat) = self.seat_for_connection(&connection_id) else {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::NotInRoom,
                "connection is not registered in this room",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        };

        if request.match_id != self.match_id {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::InvalidRequest,
                format!(
                    "match_id {} does not match active room match {}",
                    request.match_id, self.match_id
                ),
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        let current_round_id = self.round_id();
        if request.round_id != current_round_id {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::InvalidRequest,
                format!(
                    "round_id {} does not match active round {}",
                    request.round_id, current_round_id
                ),
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        match request.action.as_ref() {
            Some(player_action_request::Action::AutoPlayToggle(toggle)) => {
                self.handle_auto_play_toggle(&connection_id, seat, toggle.enabled);
                return;
            }
            Some(_) => {}
            None => {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    RejectCode::InvalidRequest,
                    "player action payload must be set",
                    request.expected_event_seq,
                    request.action_window_id,
                );
                return;
            }
        }

        let validate_request = match self.build_validate_action_request(seat, &request) {
            Ok(validate_request) => validate_request,
            Err(error) => {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    error.reject_code,
                    error.message,
                    request.expected_event_seq,
                    request.action_window_id,
                );
                return;
            }
        };

        match self.rule_engine.validate_action(validate_request).await {
            Ok(response) if response.is_legal => {
                let derived_action = engine_proto::ActionKind::try_from(response.derived_action_type)
                    .map(|action_kind| action_kind.as_str_name().to_owned())
                    .unwrap_or_else(|_| format!("ACTION_KIND_UNKNOWN_{}", response.derived_action_type));

                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    RejectCode::ActionNotAvailable,
                    format!(
                        "action validated via rule engine as {derived_action}; applying validated actions lands in a later task"
                    ),
                    request.expected_event_seq,
                    request.action_window_id,
                );
            }
            Ok(response) => {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    map_validation_reject_code(response.reject_code),
                    response.explanation,
                    request.expected_event_seq,
                    request.action_window_id,
                );
            }
            Err(error) => {
                warn!(
                    room_id = %self.room_id,
                    %connection_id,
                    ?error,
                    "rule engine validation RPC failed",
                );
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    RejectCode::InternalError,
                    format!("rule engine validation RPC failed: {error}"),
                    request.expected_event_seq,
                    request.action_window_id,
                );
            }
        }
    }

    fn handle_disconnect(&mut self, connection_id: String) {
        let Some(user_id) = self.connection_to_user.remove(&connection_id) else {
            return;
        };

        let Some(seat) = self.user_to_seat.get(&user_id).copied() else {
            return;
        };

        let Some(player) = self.players_by_seat.get_mut(&seat) else {
            return;
        };

        player.connected = false;
        player.connection_id = None;
        player.outbound_tx = None;

        self.broadcast_sync_state(SyncReason::ServerResync);
        self.broadcast_connection_changed(seat, false);
    }

    fn next_open_seat(&self) -> Option<Seat> {
        SEAT_ORDER
            .into_iter()
            .find(|seat| !self.players_by_seat.contains_key(seat))
    }

    fn handle_auto_play_toggle(&mut self, connection_id: &str, seat: Seat, enabled: bool) {
        if let Some(player) = self.players_by_seat.get_mut(&seat) {
            player.auto_play_enabled = enabled;
        } else {
            warn!(
                room_id = %self.room_id,
                %connection_id,
                seat = %seat.as_str_name(),
                "auto play toggle targeted an unknown seat"
            );
            return;
        }

        self.broadcast_sync_state(SyncReason::ServerResync);
    }

    fn seat_for_connection(&self, connection_id: &str) -> Option<Seat> {
        let user_id = self.connection_to_user.get(connection_id)?;
        self.user_to_seat.get(user_id).copied()
    }

    fn allocate_event_seq(&mut self) -> u64 {
        let event_seq = self.next_event_seq;
        self.next_event_seq += 1;
        event_seq
    }

    fn current_event_seq(&self) -> u64 {
        self.next_event_seq.saturating_sub(1)
    }

    fn round_id(&self) -> String {
        format!("hand-{}", self.hand_number)
    }

    fn build_validate_action_request(
        &self,
        actor_seat: Seat,
        request: &PlayerActionRequest,
    ) -> Result<engine_proto::ValidateActionRequest, ActionValidationError> {
        let Some(action) = request.action.as_ref() else {
            return Err(ActionValidationError::new(
                RejectCode::InvalidRequest,
                "player action payload must be set",
            ));
        };

        let candidate_action = self.build_candidate_action(actor_seat, action)?;
        let recent_action = self.build_recent_action_context(actor_seat, action)?;
        let claim_window = self.build_claim_window_context(actor_seat, request, action)?;

        Ok(engine_proto::ValidateActionRequest {
            room_id: self.room_id.clone(),
            match_id: self.match_id.clone(),
            round_id: self.round_id(),
            server_event_seq: self.current_event_seq(),
            rule_config: Some(self.build_engine_rule_config()),
            round_context: Some(self.build_engine_round_context()),
            actor_seat: map_seat_to_engine(actor_seat) as i32,
            candidate_action: Some(engine_proto::CandidateAction {
                action: Some(candidate_action),
            }),
            recent_action,
            players: self.build_engine_player_states(),
            actor_private_hand: Some(engine_proto::PrivateHandContext {
                concealed_tiles: Vec::new(),
                drawn_tile: engine_proto::Tile::Unspecified as i32,
            }),
            claim_window,
        })
    }

    fn build_candidate_action(
        &self,
        actor_seat: Seat,
        action: &player_action_request::Action,
    ) -> Result<candidate_action::Action, ActionValidationError> {
        match action {
            player_action_request::Action::Discard(discard) => {
                Ok(candidate_action::Action::Discard(engine_proto::DiscardAction {
                    tile: parse_client_tile_value(discard.tile)?,
                    tsumogiri: discard.tsumogiri,
                }))
            }
            player_action_request::Action::Claim(claim) => {
                Ok(candidate_action::Action::Claim(engine_proto::ClaimAction {
                    claim_kind: parse_client_claim_kind_value(claim.claim_kind)?,
                    target_tile: parse_client_tile_value(claim.target_tile)?,
                    consume_tiles: claim
                        .consume_tiles
                        .iter()
                        .copied()
                        .map(parse_client_tile_value)
                        .collect::<Result<Vec<_>, _>>()?,
                    source_seat: parse_client_seat_value(claim.source_seat)?,
                    source_event_seq: claim.source_event_seq,
                }))
            }
            player_action_request::Action::DeclareWin(declare_win) => {
                Ok(candidate_action::Action::DeclareWin(engine_proto::DeclareWinAction {
                    win_type: parse_client_win_type_value(declare_win.win_type)?,
                    winning_tile: parse_client_tile_value(declare_win.winning_tile)?,
                    source_seat: parse_client_seat_value(declare_win.source_seat)?,
                    source_event_seq: declare_win.source_event_seq,
                }))
            }
            player_action_request::Action::Pass(pass) => {
                Ok(candidate_action::Action::Pass(engine_proto::PassAction {
                    source_event_seq: pass.source_event_seq,
                }))
            }
            player_action_request::Action::SupplementalKong(kong) => {
                Ok(candidate_action::Action::Kong(engine_proto::KongAction {
                    kong_kind: engine_proto::ActionKind::SupplementalKong as i32,
                    tile: parse_client_tile_value(kong.tile)?,
                    consume_tiles: Vec::new(),
                    source_seat: map_seat_to_engine(actor_seat) as i32,
                    source_event_seq: self.current_event_seq(),
                }))
            }
            player_action_request::Action::AutoPlayToggle(_) => Err(ActionValidationError::new(
                RejectCode::ActionNotAvailable,
                "auto play toggles are handled locally and do not go through the rule engine",
            )),
        }
    }

    fn build_recent_action_context(
        &self,
        actor_seat: Seat,
        action: &player_action_request::Action,
    ) -> Result<Option<engine_proto::RecentActionContext>, ActionValidationError> {
        let context = match action {
            player_action_request::Action::Discard(discard) => Some(engine_proto::RecentActionContext {
                source_event_seq: self.current_event_seq(),
                source_seat: map_seat_to_engine(actor_seat) as i32,
                source_action_kind: engine_proto::ActionKind::Draw as i32,
                source_tile: parse_client_tile_value(discard.tile)?,
                replacement_draw: false,
                rob_kong_candidate: false,
            }),
            player_action_request::Action::Claim(claim) => Some(engine_proto::RecentActionContext {
                source_event_seq: claim.source_event_seq,
                source_seat: parse_client_seat_value(claim.source_seat)?,
                source_action_kind: derive_claim_trigger_action(claim.claim_kind) as i32,
                source_tile: parse_client_tile_value(claim.target_tile)?,
                replacement_draw: false,
                rob_kong_candidate: parse_client_claim_kind(claim.claim_kind)?
                    == crate::proto::client::ClaimKind::RobKongWin,
            }),
            player_action_request::Action::DeclareWin(declare_win) => Some(
                engine_proto::RecentActionContext {
                    source_event_seq: declare_win.source_event_seq,
                    source_seat: parse_client_seat_value(declare_win.source_seat)?,
                    source_action_kind: derive_win_trigger_action(declare_win.win_type) as i32,
                    source_tile: parse_client_tile_value(declare_win.winning_tile)?,
                    replacement_draw: parse_client_win_type(declare_win.win_type)?
                        == crate::proto::client::WinType::KongDraw,
                    rob_kong_candidate: parse_client_win_type(declare_win.win_type)?
                        == crate::proto::client::WinType::RobKong,
                },
            ),
            player_action_request::Action::Pass(pass) => Some(engine_proto::RecentActionContext {
                source_event_seq: pass.source_event_seq,
                source_seat: engine_proto::Seat::Unspecified as i32,
                source_action_kind: engine_proto::ActionKind::Unspecified as i32,
                source_tile: engine_proto::Tile::Unspecified as i32,
                replacement_draw: false,
                rob_kong_candidate: false,
            }),
            player_action_request::Action::SupplementalKong(kong) => Some(
                engine_proto::RecentActionContext {
                    source_event_seq: self.current_event_seq(),
                    source_seat: map_seat_to_engine(actor_seat) as i32,
                    source_action_kind: engine_proto::ActionKind::SupplementalKong as i32,
                    source_tile: parse_client_tile_value(kong.tile)?,
                    replacement_draw: false,
                    rob_kong_candidate: true,
                },
            ),
            player_action_request::Action::AutoPlayToggle(_) => None,
        };

        Ok(context)
    }

    fn build_claim_window_context(
        &self,
        actor_seat: Seat,
        request: &PlayerActionRequest,
        action: &player_action_request::Action,
    ) -> Result<Option<engine_proto::ClaimWindowContext>, ActionValidationError> {
        if request.action_window_id == 0 {
            return Ok(None);
        }

        let (source_event_seq, source_seat, target_tile, trigger_action_kind) = match action {
            player_action_request::Action::Claim(claim) => (
                claim.source_event_seq,
                parse_client_seat_value(claim.source_seat)?,
                parse_client_tile_value(claim.target_tile)?,
                derive_claim_trigger_action(claim.claim_kind) as i32,
            ),
            player_action_request::Action::DeclareWin(declare_win) => (
                declare_win.source_event_seq,
                parse_client_seat_value(declare_win.source_seat)?,
                parse_client_tile_value(declare_win.winning_tile)?,
                derive_win_trigger_action(declare_win.win_type) as i32,
            ),
            player_action_request::Action::Pass(pass) => (
                pass.source_event_seq,
                engine_proto::Seat::Unspecified as i32,
                engine_proto::Tile::Unspecified as i32,
                engine_proto::ActionKind::Unspecified as i32,
            ),
            player_action_request::Action::Discard(_) => (
                self.current_event_seq(),
                map_seat_to_engine(actor_seat) as i32,
                engine_proto::Tile::Unspecified as i32,
                engine_proto::ActionKind::Draw as i32,
            ),
            player_action_request::Action::SupplementalKong(kong) => (
                self.current_event_seq(),
                map_seat_to_engine(actor_seat) as i32,
                parse_client_tile_value(kong.tile)?,
                engine_proto::ActionKind::SupplementalKong as i32,
            ),
            player_action_request::Action::AutoPlayToggle(_) => {
                return Ok(None);
            }
        };

        Ok(Some(engine_proto::ClaimWindowContext {
            action_window_id: request.action_window_id,
            source_event_seq,
            source_seat,
            target_tile,
            trigger_action_kind,
            eligible_seats: vec![map_seat_to_engine(actor_seat) as i32],
        }))
    }

    fn build_engine_rule_config(&self) -> engine_proto::RuleConfig {
        engine_proto::RuleConfig {
            ruleset_id: self.room_config.ruleset_id.clone(),
            seat_count: self.room_config.seat_count,
            enable_flower_tiles: self.room_config.enable_flower_tiles,
            enable_robbing_kong: self.room_config.enable_robbing_kong,
            enable_kong_draw: self.room_config.enable_kong_draw,
            enable_last_tile_bonus: true,
            base_points: 0,
        }
    }

    fn build_engine_round_context(&self) -> engine_proto::RoundContext {
        engine_proto::RoundContext {
            prevailing_wind: map_seat_to_engine(self.prevailing_wind) as i32,
            dealer_seat: map_seat_to_engine(self.dealer_seat) as i32,
            current_turn_seat: map_seat_to_engine(self.current_turn_seat) as i32,
            hand_number: self.hand_number,
            dealer_streak: self.dealer_streak,
            wall_tiles_remaining: self.wall_tiles_remaining,
            dead_wall_tiles_remaining: self.dead_wall_tiles_remaining,
            replacement_draw_pending: false,
            is_last_tile: self.wall_tiles_remaining <= 1,
        }
    }

    fn build_engine_player_states(&self) -> Vec<engine_proto::EnginePlayerState> {
        SEAT_ORDER
            .into_iter()
            .filter_map(|seat| self.players_by_seat.get(&seat))
            .map(|player| engine_proto::EnginePlayerState {
                seat: map_seat_to_engine(player.seat) as i32,
                current_score: player.score,
                concealed_tile_count: 0,
                melds: Vec::new(),
                discards: Vec::new(),
                flowers: Vec::new(),
            })
            .collect()
    }

    fn broadcast_sync_state(&mut self, reason: SyncReason) {
        let event_seq = self.allocate_event_seq();

        for seat in SEAT_ORDER {
            let Some(player) = self.players_by_seat.get(&seat) else {
                continue;
            };

            if !player.connected {
                continue;
            }

            let Some(outbound_tx) = player.outbound_tx.as_ref() else {
                continue;
            };

            if let Err(error) = outbound_tx.send(self.build_sync_state_for(seat, event_seq, reason)) {
                warn!(
                    room_id = %self.room_id,
                    seat = %seat.as_str_name(),
                    ?error,
                    "failed to broadcast SyncState"
                );
            }
        }
    }

    fn broadcast_connection_changed(&mut self, seat: Seat, connected: bool) {
        let Some((user_id, status)) = self
            .players_by_seat
            .get(&seat)
            .map(|player| (player.user_id.clone(), player.status))
        else {
            return;
        };

        let frame = build_player_connection_changed(
            self.allocate_event_seq(),
            self.room_id.clone(),
            self.match_id.clone(),
            user_id,
            seat,
            status,
            connected,
        );

        for player in self.players_by_seat.values() {
            if !player.connected {
                continue;
            }

            let Some(outbound_tx) = player.outbound_tx.as_ref() else {
                continue;
            };

            if let Err(error) = outbound_tx.send(frame.clone()) {
                warn!(
                    room_id = %self.room_id,
                    seat = %player.seat.as_str_name(),
                    ?error,
                    "failed to broadcast PlayerConnectionChanged"
                );
            }
        }
    }

    fn send_reject_to_connection(
        &mut self,
        connection_id: &str,
        request_id: String,
        reject_code: RejectCode,
        message: impl Into<String>,
        expected_event_seq: u64,
        action_window_id: u64,
    ) {
        let Some(user_id) = self.connection_to_user.get(connection_id) else {
            return;
        };
        let Some(seat) = self.user_to_seat.get(user_id).copied() else {
            return;
        };
        let Some(outbound_tx) = self
            .players_by_seat
            .get(&seat)
            .and_then(|player| player.outbound_tx.clone())
        else {
            return;
        };

        let frame = build_action_rejected(
            self.allocate_event_seq(),
            request_id,
            reject_code,
            message,
            expected_event_seq,
            action_window_id,
        );

        if let Err(error) = outbound_tx.send(frame) {
            warn!(
                room_id = %self.room_id,
                %connection_id,
                ?error,
                "failed to send ActionRejected"
            );
        }
    }

    fn build_sync_state_for(
        &self,
        recipient_seat: Seat,
        event_seq: u64,
        reason: SyncReason,
    ) -> ServerFrame {
        let players = SEAT_ORDER
            .into_iter()
            .filter_map(|seat| self.players_by_seat.get(&seat))
            .map(|player| PlayerState {
                user_id: player.user_id.clone(),
                display_name: player.display_name.clone(),
                seat: player.seat as i32,
                status: player.status as i32,
                score: player.score,
                is_dealer: player.seat == self.dealer_seat,
                is_connected: player.connected,
                auto_play_enabled: player.auto_play_enabled,
                concealed_tile_count: 0,
                melds: Vec::new(),
                discards: Vec::new(),
                flowers: Vec::new(),
                replacement_draw_count: 0,
            })
            .collect();

        ServerFrame {
            event_seq,
            payload: Some(server_frame::Payload::SyncState(SyncState {
                room_id: self.room_id.clone(),
                match_id: self.match_id.clone(),
                round_id: self.round_id(),
                reason: reason as i32,
                server_time_ms: unix_time_ms(),
                snapshot: Some(GameSnapshot {
                    room_config: Some(self.room_config.clone()),
                    phase: self.phase as i32,
                    self_seat: recipient_seat as i32,
                    prevailing_wind: self.prevailing_wind as i32,
                    dealer_seat: self.dealer_seat as i32,
                    current_turn_seat: self.current_turn_seat as i32,
                    hand_number: self.hand_number,
                    dealer_streak: self.dealer_streak,
                    wall_tiles_remaining: self.wall_tiles_remaining,
                    dead_wall_tiles_remaining: self.dead_wall_tiles_remaining,
                    latest_event_seq: event_seq,
                    players,
                    self_hand: Some(SelfHandState {
                        concealed_tiles: Vec::new(),
                        drawn_tile: Tile::Unspecified as i32,
                    }),
                    active_claim_window: None,
                    action_deadline_unix_ms: 0,
                }),
            })),
        }
    }
}

fn build_join_room_response(
    event_seq: u64,
    request_id: String,
    accepted: bool,
    reject_code: RejectCode,
    room_id: String,
    match_id: String,
    seat: Seat,
    connection_id: String,
    resume_token: String,
    room_config: Option<RoomConfig>,
    message: impl Into<String>,
) -> ServerFrame {
    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::JoinRoom(JoinRoomResponse {
            request_id,
            accepted,
            reject_code: reject_code as i32,
            room_id,
            match_id,
            seat: seat as i32,
            connection_id,
            resume_token,
            room_config,
            message: message.into(),
            latest_event_seq: event_seq,
        })),
    }
}

fn build_player_connection_changed(
    event_seq: u64,
    room_id: String,
    match_id: String,
    user_id: String,
    seat: Seat,
    status: PlayerStatus,
    connected: bool,
) -> ServerFrame {
    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::PlayerConnectionChanged(
            PlayerConnectionChanged {
                room_id,
                match_id,
                user_id,
                seat: seat as i32,
                status: status as i32,
                connected,
                reconnect_deadline_unix_ms: if connected {
                    0
                } else {
                    unix_time_ms() + RECONNECT_GRACE_MS
                },
            },
        )),
    }
}

fn build_action_rejected(
    event_seq: u64,
    request_id: String,
    reject_code: RejectCode,
    message: impl Into<String>,
    expected_event_seq: u64,
    action_window_id: u64,
) -> ServerFrame {
    ServerFrame {
        event_seq,
        payload: Some(server_frame::Payload::ActionRejected(ActionRejected {
            request_id,
            reject_code: reject_code as i32,
            message: message.into(),
            expected_event_seq,
            actual_event_seq: event_seq,
            action_window_id,
        })),
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn map_seat_to_engine(seat: Seat) -> engine_proto::Seat {
    engine_proto::Seat::from_str_name(seat.as_str_name()).unwrap_or(engine_proto::Seat::Unspecified)
}

fn map_validation_reject_code(reject_code: i32) -> RejectCode {
    match engine_proto::ValidationRejectCode::try_from(reject_code)
        .unwrap_or(engine_proto::ValidationRejectCode::Unspecified)
    {
        engine_proto::ValidationRejectCode::Unspecified => RejectCode::RuleValidationFailed,
        engine_proto::ValidationRejectCode::InvalidContext => RejectCode::InvalidRequest,
        engine_proto::ValidationRejectCode::TileNotOwned
        | engine_proto::ValidationRejectCode::TileNotAvailable => RejectCode::InvalidTile,
        engine_proto::ValidationRejectCode::MeldNotFormable
        | engine_proto::ValidationRejectCode::HandNotComplete
        | engine_proto::ValidationRejectCode::RulesetViolation => RejectCode::RuleValidationFailed,
        engine_proto::ValidationRejectCode::ActionWindowMismatch => RejectCode::ActionWindowClosed,
        engine_proto::ValidationRejectCode::InternalError => RejectCode::InternalError,
    }
}

fn parse_client_seat_value(seat: i32) -> Result<i32, ActionValidationError> {
    Ok(map_seat_to_engine(parse_client_seat(seat)?) as i32)
}

fn parse_client_tile_value(tile: i32) -> Result<i32, ActionValidationError> {
    Ok(map_tile_to_engine(parse_client_tile(tile)?) as i32)
}

fn parse_client_claim_kind_value(claim_kind: i32) -> Result<i32, ActionValidationError> {
    Ok(map_claim_kind_to_engine(parse_client_claim_kind(claim_kind)?) as i32)
}

fn parse_client_win_type_value(win_type: i32) -> Result<i32, ActionValidationError> {
    Ok(map_win_type_to_engine(parse_client_win_type(win_type)?) as i32)
}

fn parse_client_seat(seat: i32) -> Result<Seat, ActionValidationError> {
    Seat::try_from(seat).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidRequest,
            format!("unknown seat enum value {seat}"),
        )
    })
}

fn parse_client_tile(tile: i32) -> Result<Tile, ActionValidationError> {
    Tile::try_from(tile).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidTile,
            format!("unknown tile enum value {tile}"),
        )
    })
}

fn parse_client_claim_kind(
    claim_kind: i32,
) -> Result<crate::proto::client::ClaimKind, ActionValidationError> {
    crate::proto::client::ClaimKind::try_from(claim_kind).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidRequest,
            format!("unknown claim kind enum value {claim_kind}"),
        )
    })
}

fn parse_client_win_type(win_type: i32) -> Result<crate::proto::client::WinType, ActionValidationError> {
    crate::proto::client::WinType::try_from(win_type).map_err(|_| {
        ActionValidationError::new(
            RejectCode::InvalidRequest,
            format!("unknown win type enum value {win_type}"),
        )
    })
}

fn map_tile_to_engine(tile: Tile) -> engine_proto::Tile {
    engine_proto::Tile::from_str_name(tile.as_str_name()).unwrap_or(engine_proto::Tile::Unspecified)
}

fn map_claim_kind_to_engine(
    claim_kind: crate::proto::client::ClaimKind,
) -> engine_proto::ClaimKind {
    engine_proto::ClaimKind::from_str_name(claim_kind.as_str_name())
        .unwrap_or(engine_proto::ClaimKind::Unspecified)
}

fn map_win_type_to_engine(win_type: crate::proto::client::WinType) -> engine_proto::WinType {
    engine_proto::WinType::from_str_name(win_type.as_str_name())
        .unwrap_or(engine_proto::WinType::Unspecified)
}

fn derive_claim_trigger_action(claim_kind: i32) -> engine_proto::ActionKind {
    match parse_client_claim_kind(claim_kind).unwrap_or(crate::proto::client::ClaimKind::Unspecified) {
        crate::proto::client::ClaimKind::RobKongWin => engine_proto::ActionKind::SupplementalKong,
        crate::proto::client::ClaimKind::Unspecified
        | crate::proto::client::ClaimKind::Chi
        | crate::proto::client::ClaimKind::Peng
        | crate::proto::client::ClaimKind::ExposedKong
        | crate::proto::client::ClaimKind::DiscardWin => engine_proto::ActionKind::Discard,
    }
}

fn derive_win_trigger_action(win_type: i32) -> engine_proto::ActionKind {
    match parse_client_win_type(win_type).unwrap_or(crate::proto::client::WinType::Unspecified) {
        crate::proto::client::WinType::SelfDraw => engine_proto::ActionKind::Draw,
        crate::proto::client::WinType::Discard => engine_proto::ActionKind::Discard,
        crate::proto::client::WinType::RobKong => engine_proto::ActionKind::SupplementalKong,
        crate::proto::client::WinType::KongDraw => engine_proto::ActionKind::Draw,
        crate::proto::client::WinType::Unspecified => engine_proto::ActionKind::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::{
        engine::{MockRuleEngine, RuleEngineHandle},
        proto::{
            client::{player_action_request, server_frame, PassAction},
            engine as engine_proto,
        },
    };

    fn test_connection(
        connection_id: &str,
    ) -> (ConnectionHandle, mpsc::UnboundedReceiver<ServerFrame>) {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        (
            ConnectionHandle::new(connection_id.to_owned(), outbound_tx),
            outbound_rx,
        )
    }

    async fn recv_frame(rx: &mut mpsc::UnboundedReceiver<ServerFrame>) -> ServerFrame {
        timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("frame should arrive in time")
            .expect("connection should remain open")
    }

    #[tokio::test]
    async fn join_room_creates_room_task_and_assigns_east_seat() {
        let manager = RoomManager::default();
        let (connection, mut outbound_rx) = test_connection("conn-1");

        manager
            .dispatch_join(
                "req-join".to_owned(),
                connection,
                JoinRoomRequest {
                    room_id: "room-alpha".to_owned(),
                    user_id: "user-alpha".to_owned(),
                    session_token: String::new(),
                    display_name: "Alice".to_owned(),
                    client_version: "test".to_owned(),
                },
            )
            .await;

        let join_response = recv_frame(&mut outbound_rx).await;
        let sync_state = recv_frame(&mut outbound_rx).await;
        let connection_changed = recv_frame(&mut outbound_rx).await;

        match join_response.payload {
            Some(server_frame::Payload::JoinRoom(response)) => {
                assert!(response.accepted);
                assert_eq!(response.room_id, "room-alpha");
                assert_eq!(response.match_id, "match-room-alpha");
                assert_eq!(response.seat, Seat::East as i32);
            }
            other => panic!("expected JoinRoomResponse, got {other:?}"),
        }

        match sync_state.payload {
            Some(server_frame::Payload::SyncState(sync)) => {
                let snapshot = sync.snapshot.expect("snapshot should be present");
                assert_eq!(snapshot.self_seat, Seat::East as i32);
                assert_eq!(snapshot.players.len(), 1);
                assert_eq!(snapshot.players[0].status, PlayerStatus::Joined as i32);
            }
            other => panic!("expected SyncState, got {other:?}"),
        }

        match connection_changed.payload {
            Some(server_frame::Payload::PlayerConnectionChanged(changed)) => {
                assert_eq!(changed.user_id, "user-alpha");
                assert_eq!(changed.seat, Seat::East as i32);
                assert!(changed.connected);
            }
            other => panic!("expected PlayerConnectionChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ready_request_is_serialized_through_room_task() {
        let manager = RoomManager::default();
        let (connection, mut outbound_rx) = test_connection("conn-2");

        manager
            .dispatch_join(
                "req-join".to_owned(),
                connection.clone(),
                JoinRoomRequest {
                    room_id: "room-beta".to_owned(),
                    user_id: "user-beta".to_owned(),
                    session_token: String::new(),
                    display_name: "Bob".to_owned(),
                    client_version: "test".to_owned(),
                },
            )
            .await;

        for _ in 0..3 {
            let _ = recv_frame(&mut outbound_rx).await;
        }

        manager
            .dispatch_ready(
                "room-beta".to_owned(),
                "req-ready".to_owned(),
                connection,
                ReadyRequest { ready: true },
            )
            .await;

        let sync_state = recv_frame(&mut outbound_rx).await;

        match sync_state.payload {
            Some(server_frame::Payload::SyncState(sync)) => {
                let snapshot = sync.snapshot.expect("snapshot should be present");
                assert_eq!(snapshot.players.len(), 1);
                assert_eq!(snapshot.players[0].status, PlayerStatus::Ready as i32);
                assert_eq!(snapshot.phase, GamePhase::Lobby as i32);
            }
            other => panic!("expected SyncState, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn player_action_is_queued_into_room_task_and_rejected_there() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: true,
            reject_code: engine_proto::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine_proto::ActionKind::Pass as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "mock validation accepted action".to_owned(),
        });
        let manager = RoomManager::with_rule_engine(RuleEngineHandle::new(mock_rule_engine.clone()));
        let (connection, mut outbound_rx) = test_connection("conn-3");

        manager
            .dispatch_join(
                "req-join".to_owned(),
                connection.clone(),
                JoinRoomRequest {
                    room_id: "room-gamma".to_owned(),
                    user_id: "user-gamma".to_owned(),
                    session_token: String::new(),
                    display_name: "Carol".to_owned(),
                    client_version: "test".to_owned(),
                },
            )
            .await;

        for _ in 0..3 {
            let _ = recv_frame(&mut outbound_rx).await;
        }

        manager
            .dispatch_player_action(
                "req-action".to_owned(),
                connection,
                PlayerActionRequest {
                    room_id: "room-gamma".to_owned(),
                    match_id: "match-room-gamma".to_owned(),
                    round_id: "hand-1".to_owned(),
                    expected_event_seq: 3,
                    action_window_id: 0,
                    action: Some(player_action_request::Action::Pass(PassAction {
                        source_event_seq: 0,
                    })),
                },
            )
            .await;

        let rejected = recv_frame(&mut outbound_rx).await;

        match rejected.payload {
            Some(server_frame::Payload::ActionRejected(rejected)) => {
                assert_eq!(rejected.request_id, "req-action");
                assert_eq!(rejected.reject_code, RejectCode::ActionNotAvailable as i32);
                assert_eq!(rejected.expected_event_seq, 3);
                assert!(rejected.message.contains("validated via rule engine"));
            }
            other => panic!("expected ActionRejected, got {other:?}"),
        }

        let validate_requests = mock_rule_engine.validate_requests();
        assert_eq!(validate_requests.len(), 1);

        let validate_request = &validate_requests[0];
        assert_eq!(validate_request.room_id, "room-gamma");
        assert_eq!(validate_request.match_id, "match-room-gamma");
        assert_eq!(validate_request.round_id, "hand-1");
        assert_eq!(validate_request.actor_seat, engine_proto::Seat::East as i32);

        match validate_request
            .candidate_action
            .as_ref()
            .and_then(|candidate| candidate.action.as_ref())
        {
            Some(engine_proto::candidate_action::Action::Pass(pass)) => {
                assert_eq!(pass.source_event_seq, 0);
            }
            other => panic!("expected Pass candidate action, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rule_engine_validation_failure_is_sent_back_to_client() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: false,
            reject_code: engine_proto::ValidationRejectCode::ActionWindowMismatch as i32,
            derived_action_type: engine_proto::ActionKind::Unspecified as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "claim window already closed".to_owned(),
        });
        let manager = RoomManager::with_rule_engine(RuleEngineHandle::new(mock_rule_engine));
        let (connection, mut outbound_rx) = test_connection("conn-4");

        manager
            .dispatch_join(
                "req-join".to_owned(),
                connection.clone(),
                JoinRoomRequest {
                    room_id: "room-delta".to_owned(),
                    user_id: "user-delta".to_owned(),
                    session_token: String::new(),
                    display_name: "Delta".to_owned(),
                    client_version: "test".to_owned(),
                },
            )
            .await;

        for _ in 0..3 {
            let _ = recv_frame(&mut outbound_rx).await;
        }

        manager
            .dispatch_player_action(
                "req-action".to_owned(),
                connection,
                PlayerActionRequest {
                    room_id: "room-delta".to_owned(),
                    match_id: "match-room-delta".to_owned(),
                    round_id: "hand-1".to_owned(),
                    expected_event_seq: 3,
                    action_window_id: 42,
                    action: Some(player_action_request::Action::Pass(PassAction {
                        source_event_seq: 41,
                    })),
                },
            )
            .await;

        let rejected = recv_frame(&mut outbound_rx).await;

        match rejected.payload {
            Some(server_frame::Payload::ActionRejected(rejected)) => {
                assert_eq!(rejected.request_id, "req-action");
                assert_eq!(rejected.reject_code, RejectCode::ActionWindowClosed as i32);
                assert_eq!(rejected.expected_event_seq, 3);
                assert_eq!(rejected.action_window_id, 42);
                assert_eq!(rejected.message, "claim window already closed");
            }
            other => panic!("expected ActionRejected, got {other:?}"),
        }
    }
}
