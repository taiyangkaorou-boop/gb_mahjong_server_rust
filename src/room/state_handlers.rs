// 这一段集中处理 RoomState 自身的命令分发、摸打流程、claim window 和结算推进。
// 拆分后优先把“真正改状态”的逻辑放在一起，便于后续继续细分 round_flow。

impl RoomState {
    // 这一段是房间状态机的主体流程：
    // 包括命令分发、加入/重连、准备、摸打、吃碰杠胡、抢操作窗口和结算推进。
    fn new(
        room_id: String,
        rule_engine: RuleEngineHandle,
        wall_factory: WallFactory,
        room_command_tx: mpsc::Sender<RoomCommand>,
        match_event_writer: MatchEventWriter,
    ) -> Self {
        Self {
            rule_engine,
            match_id: format!("match-{room_id}"),
            room_id,
            wall_factory,
            room_command_tx,
            match_event_writer,
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
            dead_wall_tiles_remaining: DEAD_WALL_TILE_COUNT as u32,
            next_event_seq: 1,
            next_action_window_id: 1,
            players_by_seat: HashMap::new(),
            player_round_state: HashMap::new(),
            round_runtime: None,
            user_to_seat: HashMap::new(),
            connection_to_user: HashMap::new(),
        }
    }

    async fn handle_command(&mut self, command: RoomCommand) {
        match command {
            RoomCommand::Join {
                request_id,
                connection,
                request,
                authorized_join,
            } => self.handle_join(request_id, connection, request, authorized_join),
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
            RoomCommand::ClaimWindowTimeout { action_window_id } => {
                self.handle_claim_window_timeout(action_window_id).await
            }
            RoomCommand::Disconnect { connection_id } => self.handle_disconnect(connection_id),
            RoomCommand::RemovePlayer { user_id } => self.handle_remove_player(user_id),
        }
    }

    fn handle_join(
        &mut self,
        request_id: String,
        connection: ConnectionHandle,
        _request: JoinRoomRequest,
        authorized_join: AuthorizedJoin,
    ) {
        // 到这里时，user_id/display_name/seat 都已经是网关确认过的值，
        // room task 不再重新信任客户端原始 JoinRoomRequest。
        if let Some(existing_seat) = self.user_to_seat.get(&authorized_join.user_id).copied() {
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

        if self.players_by_seat.contains_key(&authorized_join.seat) {
            connection.send_frame(build_join_room_response(
                self.allocate_event_seq(),
                request_id,
                false,
                RejectCode::SeatUnavailable,
                self.room_id.clone(),
                self.match_id.clone(),
                authorized_join.seat,
                connection.connection_id().to_owned(),
                String::new(),
                Some(self.room_config.clone()),
                "authorized seat is no longer available",
            ));
            return;
        }

        let user_id = authorized_join.user_id.clone();
        let resume_token = format!("resume-{}-{user_id}", self.room_id);

        self.connection_to_user
            .insert(connection.connection_id().to_owned(), user_id.clone());
        self.user_to_seat
            .insert(user_id.clone(), authorized_join.seat);
        self.players_by_seat.insert(
            authorized_join.seat,
            PlayerSlot {
                user_id,
                display_name: authorized_join.display_name,
                seat: authorized_join.seat,
                status: PlayerStatus::Joined,
                score: INITIAL_SCORE,
                connected: true,
                auto_play_enabled: false,
                connection_id: Some(connection.connection_id().to_owned()),
                outbound_tx: Some(connection.outbound_tx.clone()),
                resume_token: resume_token.clone(),
            },
        );
        self.player_round_state
            .insert(authorized_join.seat, PlayerRoundState::default());

        connection.send_frame(build_join_room_response(
            self.allocate_event_seq(),
            request_id,
            true,
            RejectCode::Unspecified,
            self.room_id.clone(),
            self.match_id.clone(),
            authorized_join.seat,
            connection.connection_id().to_owned(),
            resume_token,
            Some(self.room_config.clone()),
            "joined room task successfully",
        ));

        self.broadcast_sync_state(SyncReason::ServerResync);
        self.broadcast_connection_changed(authorized_join.seat, true);
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

        if all_ready {
            if let Err(error) = self.start_round() {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    error.reject_code,
                    error.message,
                    0,
                    0,
                );
                return;
            }

            self.broadcast_sync_state(SyncReason::RoundStart);
        } else {
            self.phase = GamePhase::Lobby;
            self.broadcast_sync_state(SyncReason::ServerResync);
        }
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

        // expected_event_seq 表示客户端确认自己所见的最新事件号。
        // 一旦和房间当前事件流不一致，就必须拒绝，避免在过期状态上继续操作。
        // 这是断线重连和多端乱序保护的关键闸门之一。
        let current_event_seq = self.current_event_seq();
        if request.expected_event_seq != current_event_seq {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::StaleEventSeq,
                format!(
                    "expected_event_seq {} is stale; current server event_seq is {}",
                    request.expected_event_seq, current_event_seq
                ),
                current_event_seq,
                request.action_window_id,
            );
            return;
        }

        match request.action.as_ref() {
            Some(player_action_request::Action::AutoPlayToggle(toggle)) => {
                self.handle_auto_play_toggle(&connection_id, seat, toggle.enabled);
                return;
            }
            Some(player_action_request::Action::Discard(discard)) => {
                self.handle_discard_action(
                    request_id,
                    connection_id,
                    seat,
                    &request,
                    discard,
                )
                .await;
                return;
            }
            Some(player_action_request::Action::Claim(claim)) => {
                self.handle_claim_action(
                    request_id,
                    connection_id,
                    seat,
                    &request,
                    claim,
                )
                .await;
                return;
            }
            Some(player_action_request::Action::DeclareWin(declare_win)) => {
                self.handle_declare_win_action(
                    request_id,
                    connection_id,
                    seat,
                    &request,
                    declare_win,
                )
                .await;
                return;
            }
            Some(player_action_request::Action::SupplementalKong(kong)) => {
                self.handle_supplemental_kong_action(
                    request_id,
                    connection_id,
                    seat,
                    &request,
                    kong,
                )
                .await;
                return;
            }
            Some(player_action_request::Action::Pass(pass)) => {
                self.handle_pass_action(
                    request_id,
                    connection_id,
                    seat,
                    &request,
                    pass,
                )
                .await;
                return;
            }
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
    }

    async fn handle_discard_action(
        &mut self,
        request_id: String,
        connection_id: String,
        seat: Seat,
        request: &PlayerActionRequest,
        discard: &crate::proto::client::DiscardAction,
    ) {
        // 出牌是最常见的公开动作起点：
        // 先落本地状态，再广播，再看是否需要为其他玩家打开抢操作窗口。
        if self.phase != GamePhase::WaitingDiscard {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::PhaseMismatch,
                "discard is only available during WaitingDiscard",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        if seat != self.current_turn_seat {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::NotYourTurn,
                "only the current turn seat may discard",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        let tile = match parse_client_tile(discard.tile) {
            Ok(tile) => tile,
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

        if let Err(error) = self.discard_tile_from_hand(seat, tile, discard.tsumogiri) {
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

        let discard_event_seq = self.allocate_event_seq();
        self.set_last_discard_source_event_seq(seat, discard_event_seq);
        self.record_last_action(ResolvedActionRecord {
            event_seq: discard_event_seq,
            actor_seat: seat,
            action_kind: crate::proto::client::ActionKind::Discard,
            tile: Some(tile),
            replacement_draw: false,
            rob_kong_candidate: false,
        });
        self.broadcast_discard_action(seat, discard_event_seq, tile);

        match self.open_discard_claim_window(seat, tile, discard_event_seq).await {
            Ok(true) => {
                self.broadcast_sync_state(SyncReason::ServerResync);
            }
            Ok(false) => {
                self.advance_turn_after_passes(seat);
                self.broadcast_sync_state(SyncReason::ServerResync);
            }
            Err(error) => {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    error.reject_code,
                    error.message,
                    request.expected_event_seq,
                    request.action_window_id,
                );
            }
        }
    }

    async fn handle_pass_action(
        &mut self,
        request_id: String,
        connection_id: String,
        seat: Seat,
        request: &PlayerActionRequest,
        pass: &crate::proto::client::PassAction,
    ) {
        let Some(runtime) = self.round_runtime.as_mut() else {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::PhaseMismatch,
                "no active round runtime exists",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        };

        let Some(window) = runtime.active_claim_window.as_mut() else {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::ActionWindowClosed,
                "there is no active claim window",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        };

        if request.action_window_id != window.action_window_id {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::ActionWindowClosed,
                "request references a stale claim window",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        if pass.source_event_seq != window.source_event_seq {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::InvalidRequest,
                "pass source_event_seq does not match the active claim window",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        if !window.eligible_seats.contains(&seat) {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::ActionNotAvailable,
                "seat is not eligible in the current claim window",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        window.responded_seats.insert(seat, ClaimResponse::Pass);
        let should_resolve = window.responded_seats.len() == window.eligible_seats.len();
        let _ = runtime;

        self.resolve_claim_window_if_ready(should_resolve).await;
    }

    async fn handle_claim_action(
        &mut self,
        request_id: String,
        connection_id: String,
        seat: Seat,
        request: &PlayerActionRequest,
        claim: &crate::proto::client::ClaimAction,
    ) {
        // claim 先过窗口有效性和规则引擎合法性检查；
        // 通过后只记入响应池，等窗口收齐后统一裁决，不做先到先得。
        if let Err(error) = self.validate_claim_window_submission(
            seat,
            request.action_window_id,
            claim.source_event_seq,
        ) {
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

        let validate_request = match self.build_validate_action_request(seat, request) {
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

        let validation_response = match self.rule_engine.validate_action(validate_request).await {
            Ok(response) => response,
            Err(error) => {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    RejectCode::InternalError,
                    format!("rule engine validation RPC failed: {error}"),
                    request.expected_event_seq,
                    request.action_window_id,
                );
                return;
            }
        };

        if !validation_response.is_legal {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                map_validation_reject_code(validation_response.reject_code),
                validation_response.explanation,
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        if let Err(error) = self.validate_claim_choice(seat, claim) {
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

        if let Some(window) = self
            .round_runtime
            .as_mut()
            .and_then(|runtime| runtime.active_claim_window.as_mut())
        {
            window
                .responded_seats
                .insert(seat, ClaimResponse::Claim(claim.clone()));
            let should_resolve = window.responded_seats.len() == window.eligible_seats.len();
            let _ = window;
            self.resolve_claim_window_if_ready(should_resolve).await;
        }
    }

    async fn handle_supplemental_kong_action(
        &mut self,
        request_id: String,
        connection_id: String,
        seat: Seat,
        request: &PlayerActionRequest,
        kong: &crate::proto::client::SupplementalKongAction,
    ) {
        // 补杠是一个两阶段动作：
        // 先确认玩家真的能补杠，再决定是否要为抢杠胡打开 claim window。
        if self.phase != GamePhase::WaitingDiscard {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::PhaseMismatch,
                "supplemental kong is only available during WaitingDiscard",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }
        if seat != self.current_turn_seat {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                RejectCode::NotYourTurn,
                "only the current turn seat may declare a supplemental kong",
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        let tile = match parse_client_tile(kong.tile) {
            Ok(tile) => tile,
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

        if let Err(error) = self.validate_supplemental_kong_choice(seat, tile) {
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

        let validate_request = match self.build_validate_action_request(seat, request) {
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

        let validation_response = match self.rule_engine.validate_action(validate_request).await {
            Ok(response) => response,
            Err(error) => {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    RejectCode::InternalError,
                    format!("rule engine validation RPC failed: {error}"),
                    request.expected_event_seq,
                    request.action_window_id,
                );
                return;
            }
        };

        if !validation_response.is_legal {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                map_validation_reject_code(validation_response.reject_code),
                validation_response.explanation,
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        let kong_event_seq = self.allocate_event_seq();
        if self.room_config.enable_robbing_kong {
            match self
                .open_rob_kong_claim_window(seat, tile, kong_event_seq)
                .await
            {
                Ok(true) => {
                    self.phase = GamePhase::ResolvingKong;
                    self.broadcast_sync_state(SyncReason::ServerResync);
                    return;
                }
                Ok(false) => {}
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
            }
        }

        if let Err(error) = self.apply_supplemental_kong(seat, tile, kong_event_seq) {
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

        self.broadcast_sync_state(SyncReason::ServerResync);
    }

    async fn handle_declare_win_action(
        &mut self,
        request_id: String,
        connection_id: String,
        seat: Seat,
        request: &PlayerActionRequest,
        declare_win: &crate::proto::client::DeclareWinAction,
    ) {
        let validate_request = match self.build_validate_action_request(seat, request) {
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

        let validation_response = match self.rule_engine.validate_action(validate_request).await {
            Ok(response) => response,
            Err(error) => {
                self.send_reject_to_connection(
                    &connection_id,
                    request_id,
                    RejectCode::InternalError,
                    format!("rule engine validation RPC failed: {error}"),
                    request.expected_event_seq,
                    request.action_window_id,
                );
                return;
            }
        };

        if !validation_response.is_legal {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                map_validation_reject_code(validation_response.reject_code),
                validation_response.explanation,
                request.expected_event_seq,
                request.action_window_id,
            );
            return;
        }

        let win_type = match parse_client_win_type(declare_win.win_type) {
            Ok(win_type) => win_type,
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
        let winning_tile = match parse_client_tile(declare_win.winning_tile) {
            Ok(tile) => tile,
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

        let discarder_seat = match self.resolve_declare_win_context(
            seat,
            request,
            declare_win,
            win_type,
            winning_tile,
        ) {
            Ok(discarder_seat) => discarder_seat,
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

        if discarder_seat.is_some() {
            if let Some(window) = self
                .round_runtime
                .as_mut()
                .and_then(|runtime| runtime.active_claim_window.as_mut())
            {
                window
                    .responded_seats
                    .insert(seat, ClaimResponse::DeclareWin(declare_win.clone()));
                let should_resolve = window.responded_seats.len() == window.eligible_seats.len();
                let _ = window;
                self.resolve_claim_window_if_ready(should_resolve).await;
                return;
            }
        }

        if let Err(error) = self
            .finalize_winning_action(
                seat,
                discarder_seat,
                win_type,
                winning_tile,
                declare_win.source_event_seq,
            )
            .await
        {
            self.send_reject_to_connection(
                &connection_id,
                request_id,
                error.reject_code,
                error.message,
                request.expected_event_seq,
                request.action_window_id,
            );
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

    fn handle_remove_player(&mut self, user_id: String) {
        // 只在 Lobby 阶段响应大厅层的 remove 命令，
        // 防止正式对局中被 HTTP 踢人破坏状态一致性。
        if self.phase != GamePhase::Lobby {
            return;
        }

        let Some(seat) = self.user_to_seat.remove(&user_id) else {
            return;
        };

        let removed_player = self.players_by_seat.remove(&seat);
        self.player_round_state.remove(&seat);

        if let Some(player) = removed_player {
            if let Some(connection_id) = player.connection_id {
                self.connection_to_user.remove(&connection_id);
            }
            if let Some(outbound_tx) = player.outbound_tx {
                let _ = outbound_tx.send(build_action_rejected(
                    self.allocate_event_seq(),
                    String::new(),
                    RejectCode::NotInRoom,
                    "player was removed from the lobby room",
                    0,
                    0,
                ));
            }
        }

        self.broadcast_sync_state(SyncReason::ServerResync);
    }

    // claim window 的所有响应最终都走这里统一裁决，避免 pass/claim/win/timeout 分别维护三套推进逻辑。
    async fn resolve_claim_window_if_ready(&mut self, force_resolve: bool) {
        let should_resolve = self
            .round_runtime
            .as_ref()
            .and_then(|runtime| runtime.active_claim_window.as_ref())
            .map(|window| force_resolve || window.responded_seats.len() == window.eligible_seats.len())
            .unwrap_or(false);

        if !should_resolve {
            self.broadcast_sync_state(SyncReason::ServerResync);
            return;
        }

        let Some(window) = self
            .round_runtime
            .as_mut()
            .and_then(|runtime| runtime.active_claim_window.take())
        else {
            return;
        };

        let mut responded_seats = window.responded_seats.clone();
        for seat in &window.eligible_seats {
            responded_seats
                .entry(*seat)
                .or_insert(ClaimResponse::Pass);
        }

        let Some((winner_seat, winning_response)) =
            self.select_highest_priority_claim_response(&window, &responded_seats)
        else {
            self.persist_claim_window_resolved_event(&window, &responded_seats, None, "PASS");
            match window.trigger_action_kind {
                crate::proto::client::ActionKind::SupplementalKong => {
                    if let Err(error) = self.apply_supplemental_kong(
                        window.source_seat,
                        window.target_tile,
                        window.source_event_seq,
                    ) {
                        warn!(
                            room_id = %self.room_id,
                            seat = %window.source_seat.as_str_name(),
                            ?error,
                            "failed to finalize supplemental kong after rob-kong window"
                        );
                    }
                    self.broadcast_sync_state(SyncReason::ServerResync);
                }
                _ => {
                    self.advance_turn_after_passes(window.source_seat);
                    self.broadcast_sync_state(SyncReason::ServerResync);
                }
            }
            return;
        };

        self.persist_claim_window_resolved_event(
            &window,
            &responded_seats,
            Some(winner_seat),
            match &winning_response {
                ClaimResponse::Pass => "PASS",
                ClaimResponse::DeclareWin(_) => "DECLARE_WIN",
                ClaimResponse::Claim(_) => "CLAIM",
            },
        );

        match winning_response {
            ClaimResponse::Pass => {
                self.advance_turn_after_passes(window.source_seat);
                self.broadcast_sync_state(SyncReason::ServerResync);
            }
            ClaimResponse::DeclareWin(declare_win) => {
                let win_type =
                    parse_client_win_type(declare_win.win_type).unwrap_or(crate::proto::client::WinType::Discard);
                let winning_tile = parse_client_tile(declare_win.winning_tile).unwrap_or(window.target_tile);
                if let Err(error) = self
                    .finalize_winning_action(
                        winner_seat,
                        Some(window.source_seat),
                        win_type,
                        winning_tile,
                        declare_win.source_event_seq,
                    )
                    .await
                {
                    warn!(
                        room_id = %self.room_id,
                        seat = %winner_seat.as_str_name(),
                        ?error,
                        "failed to finalize winning action from claim window"
                    );
                    self.broadcast_sync_state(SyncReason::ServerResync);
                }
            }
            ClaimResponse::Claim(claim) => match self.apply_resolved_claim_action(winner_seat, &claim)
            {
                Ok(()) => self.broadcast_sync_state(SyncReason::ServerResync),
                Err(error) => {
                    warn!(
                        room_id = %self.room_id,
                        seat = %winner_seat.as_str_name(),
                        ?error,
                        "failed to apply resolved claim action"
                    );
                    self.broadcast_sync_state(SyncReason::ServerResync);
                }
            },
        }
    }

    async fn handle_claim_window_timeout(&mut self, action_window_id: u64) {
        let Some(runtime) = self.round_runtime.as_mut() else {
            return;
        };

        let Some(window) = runtime.active_claim_window.as_ref() else {
            return;
        };

        if window.action_window_id != action_window_id {
            return;
        }

        // 超时等价于未响应玩家全部视为 pass，房间必须继续推进。
        let _ = runtime;
        self.resolve_claim_window_if_ready(true).await;
    }

    #[allow(dead_code)]
    fn next_open_seat(&self) -> Option<Seat> {
        SEAT_ORDER
            .into_iter()
            .find(|seat| !self.players_by_seat.contains_key(seat))
    }

    fn discard_tile_from_hand(
        &mut self,
        seat: Seat,
        tile: Tile,
        tsumogiri: bool,
    ) -> Result<(), ActionValidationError> {
        // drawn_tile 与 concealed_tiles 分开存放后，
        // 这里可以清晰地区分摸切和手切两条路径。
        let player = self
            .player_round_state
            .get_mut(&seat)
            .expect("player round state should exist");

        let drawn_tile = player.drawn_tile;
        let drawn_and_discarded;

        if tsumogiri {
            if drawn_tile != Some(tile) {
                return Err(ActionValidationError::new(
                    RejectCode::InvalidTile,
                    "tsumogiri requires the pending drawn tile to match the discarded tile",
                ));
            }

            player.drawn_tile = None;
            drawn_and_discarded = true;
        } else if let Some(position) = player
            .concealed_tiles
            .iter()
            .position(|hand_tile| *hand_tile == tile)
        {
            player.concealed_tiles.remove(position);

            if let Some(drawn_tile) = player.drawn_tile.take() {
                player.concealed_tiles.push(drawn_tile);
                sort_tiles(&mut player.concealed_tiles);
            }

            drawn_and_discarded = false;
        } else if player.drawn_tile == Some(tile) {
            player.drawn_tile = None;
            drawn_and_discarded = true;
        } else {
            return Err(ActionValidationError::new(
                RejectCode::InvalidTile,
                "discard tile is not present in the player's private hand",
            ));
        }

        let turn_index = self
            .round_runtime
            .as_mut()
            .map(|runtime| {
                runtime.turn_index += 1;
                runtime.turn_index
            })
            .unwrap_or_default();

        player.discards.push(crate::proto::client::DiscardTile {
            tile: tile as i32,
            turn_index,
            claimed: false,
            drawn_and_discarded,
            source_event_seq: 0,
        });

        Ok(())
    }

    fn set_last_discard_source_event_seq(&mut self, seat: Seat, source_event_seq: u64) {
        if let Some(player) = self.player_round_state.get_mut(&seat) {
            if let Some(discard) = player.discards.last_mut() {
                discard.source_event_seq = source_event_seq;
            }
        }
    }

    async fn open_discard_claim_window(
        &mut self,
        source_seat: Seat,
        tile: Tile,
        source_event_seq: u64,
    ) -> Result<bool, ActionValidationError> {
        // 吃/碰/杠先靠本地牌理推导，点和再询问规则引擎，
        // 这样能减少一轮窗口构建时的 RPC 次数。
        let mut eligible_seats = Vec::new();
        let mut options_by_seat = HashMap::new();

        for seat in SEAT_ORDER {
            if seat == source_seat {
                continue;
            }

            let mut options =
                self.build_local_discard_claim_options(seat, source_seat, tile, source_event_seq)?;
            if self
                .can_win_on_discard(seat, source_seat, tile, source_event_seq)
                .await?
            {
                options.push(PromptOption {
                    option_id: format!("discard-win-{source_event_seq}-{}", seat.as_str_name()),
                    action_kind: crate::proto::client::ActionKind::DeclareWin as i32,
                    claim_kind: crate::proto::client::ClaimKind::DiscardWin as i32,
                    win_type: crate::proto::client::WinType::Discard as i32,
                    candidate_tiles: vec![tile as i32],
                    consume_tiles: Vec::new(),
                });
            }

            if !options.is_empty() {
                eligible_seats.push(seat);
                options_by_seat.insert(seat, options);
            }
        }

        if eligible_seats.is_empty() {
            return Ok(false);
        }

        let action_window_id = self.next_action_window_id;
        self.next_action_window_id += 1;
        let deadline_unix_ms = unix_time_ms() + self.room_config.action_timeout_ms as u64;
        let window = ActiveClaimWindowState {
            action_window_id,
            source_event_seq,
            source_seat,
            target_tile: tile,
            trigger_action_kind: crate::proto::client::ActionKind::Discard,
            eligible_seats: eligible_seats.clone(),
            options_by_seat,
            responded_seats: HashMap::new(),
            deadline_unix_ms,
        };

        if let Some(runtime) = self.round_runtime.as_mut() {
            runtime.active_claim_window = Some(window);
        }
        if let Some(window) = self
            .round_runtime
            .as_ref()
            .and_then(|runtime| runtime.active_claim_window.as_ref())
        {
            self.persist_claim_window_opened_event(window);
        }
        self.phase = GamePhase::WaitingClaim;
        self.send_claim_prompts(action_window_id, deadline_unix_ms, &eligible_seats);
        self.schedule_claim_window_timeout(action_window_id);

        Ok(true)
    }

    fn build_local_discard_claim_options(
        &self,
        seat: Seat,
        source_seat: Seat,
        target_tile: Tile,
        source_event_seq: u64,
    ) -> Result<Vec<PromptOption>, ActionValidationError> {
        // 这里做的是“本地可推导动作”过滤：
        // 吃只给下家，碰/明杠只看张数，和牌不在这里判断。
        let mut options = Vec::new();
        let Some(round_state) = self.player_round_state.get(&seat) else {
            return Ok(options);
        };
        let concealed_tiles = &round_state.concealed_tiles;

        if has_n_tiles(concealed_tiles, target_tile, 2) {
            options.push(PromptOption {
                option_id: format!("peng-{source_event_seq}-{}", seat.as_str_name()),
                action_kind: crate::proto::client::ActionKind::Peng as i32,
                claim_kind: crate::proto::client::ClaimKind::Peng as i32,
                win_type: crate::proto::client::WinType::Unspecified as i32,
                candidate_tiles: vec![target_tile as i32],
                consume_tiles: vec![target_tile as i32, target_tile as i32],
            });
        }

        if has_n_tiles(concealed_tiles, target_tile, 3) {
            options.push(PromptOption {
                option_id: format!("gang-{source_event_seq}-{}", seat.as_str_name()),
                action_kind: crate::proto::client::ActionKind::ExposedKong as i32,
                claim_kind: crate::proto::client::ClaimKind::ExposedKong as i32,
                win_type: crate::proto::client::WinType::Unspecified as i32,
                candidate_tiles: vec![target_tile as i32],
                consume_tiles: vec![target_tile as i32, target_tile as i32, target_tile as i32],
            });
        }

        if next_seat(source_seat) == seat {
            for consume_tiles in build_chi_consume_patterns(concealed_tiles, target_tile) {
                let mut candidate_tiles = consume_tiles.clone();
                candidate_tiles.push(target_tile);
                sort_tiles(&mut candidate_tiles);
                options.push(PromptOption {
                    option_id: format!(
                        "chi-{source_event_seq}-{}-{}",
                        seat.as_str_name(),
                        candidate_tiles
                            .iter()
                            .map(|tile| (*tile as i32).to_string())
                            .collect::<Vec<_>>()
                            .join("-")
                    ),
                    action_kind: crate::proto::client::ActionKind::Chi as i32,
                    claim_kind: crate::proto::client::ClaimKind::Chi as i32,
                    win_type: crate::proto::client::WinType::Unspecified as i32,
                    candidate_tiles: candidate_tiles.iter().map(|tile| *tile as i32).collect(),
                    consume_tiles: consume_tiles.iter().map(|tile| *tile as i32).collect(),
                });
            }
        }

        Ok(options)
    }

    fn validate_supplemental_kong_choice(
        &self,
        seat: Seat,
        tile: Tile,
    ) -> Result<(), ActionValidationError> {
        let round_state = self.player_round_state.get(&seat).ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::NotInRoom,
                format!("seat {} does not exist in round state", seat.as_str_name()),
            )
        })?;

        let has_matching_peng = round_state.melds.iter().any(|meld| {
            meld.kind == crate::proto::client::MeldKind::Peng as i32
                && meld.claimed_tile == tile as i32
        });
        if !has_matching_peng {
            return Err(ActionValidationError::new(
                RejectCode::ActionNotAvailable,
                "supplemental kong requires an existing peng meld of the target tile",
            ));
        }

        let tile_available = round_state.drawn_tile == Some(tile)
            || round_state.concealed_tiles.iter().any(|hand_tile| *hand_tile == tile);
        if !tile_available {
            return Err(ActionValidationError::new(
                RejectCode::InvalidTile,
                "supplemental kong tile is not present in the player's private hand",
            ));
        }

        Ok(())
    }

    async fn open_rob_kong_claim_window(
        &mut self,
        source_seat: Seat,
        tile: Tile,
        source_event_seq: u64,
    ) -> Result<bool, ActionValidationError> {
        // 抢杠胡窗口只允许 DeclareWin，不复用普通弃牌窗口里的吃碰杠逻辑。
        let mut eligible_seats = Vec::new();
        let mut options_by_seat = HashMap::new();

        for seat in SEAT_ORDER {
            if seat == source_seat {
                continue;
            }

            if self
                .can_win_on_rob_kong(seat, source_seat, tile, source_event_seq)
                .await?
            {
                eligible_seats.push(seat);
                options_by_seat.insert(
                    seat,
                    vec![PromptOption {
                        option_id: format!("rob-kong-win-{source_event_seq}-{}", seat.as_str_name()),
                        action_kind: crate::proto::client::ActionKind::DeclareWin as i32,
                        claim_kind: crate::proto::client::ClaimKind::RobKongWin as i32,
                        win_type: crate::proto::client::WinType::RobKong as i32,
                        candidate_tiles: vec![tile as i32],
                        consume_tiles: Vec::new(),
                    }],
                );
            }
        }

        if eligible_seats.is_empty() {
            return Ok(false);
        }

        let action_window_id = self.next_action_window_id;
        self.next_action_window_id += 1;
        let deadline_unix_ms = unix_time_ms() + self.room_config.action_timeout_ms as u64;
        let window = ActiveClaimWindowState {
            action_window_id,
            source_event_seq,
            source_seat,
            target_tile: tile,
            trigger_action_kind: crate::proto::client::ActionKind::SupplementalKong,
            eligible_seats: eligible_seats.clone(),
            options_by_seat,
            responded_seats: HashMap::new(),
            deadline_unix_ms,
        };

        if let Some(runtime) = self.round_runtime.as_mut() {
            runtime.active_claim_window = Some(window);
        }
        if let Some(window) = self
            .round_runtime
            .as_ref()
            .and_then(|runtime| runtime.active_claim_window.as_ref())
        {
            self.persist_claim_window_opened_event(window);
        }
        self.send_claim_prompts(action_window_id, deadline_unix_ms, &eligible_seats);
        self.schedule_claim_window_timeout(action_window_id);

        Ok(true)
    }

    async fn can_win_on_discard(
        &self,
        actor_seat: Seat,
        source_seat: Seat,
        tile: Tile,
        source_event_seq: u64,
    ) -> Result<bool, ActionValidationError> {
        let action = player_action_request::Action::DeclareWin(
            crate::proto::client::DeclareWinAction {
                win_type: crate::proto::client::WinType::Discard as i32,
                winning_tile: tile as i32,
                source_seat: source_seat as i32,
                source_event_seq,
            },
        );

        let request = self.build_validate_action_request_for_action(actor_seat, &action)?;
        match self.rule_engine.validate_action(request).await {
            Ok(response) => Ok(response.is_legal),
            Err(error) => Err(ActionValidationError::new(
                RejectCode::InternalError,
                format!("rule engine validation RPC failed while building claim window: {error}"),
            )),
        }
    }

    async fn can_win_on_rob_kong(
        &self,
        actor_seat: Seat,
        source_seat: Seat,
        tile: Tile,
        source_event_seq: u64,
    ) -> Result<bool, ActionValidationError> {
        let action = player_action_request::Action::DeclareWin(
            crate::proto::client::DeclareWinAction {
                win_type: crate::proto::client::WinType::RobKong as i32,
                winning_tile: tile as i32,
                source_seat: source_seat as i32,
                source_event_seq,
            },
        );

        let mut request = self.build_validate_action_request_for_action(actor_seat, &action)?;
        request.recent_action = Some(engine_proto::RecentActionContext {
            source_event_seq,
            source_seat: map_seat_to_engine(source_seat) as i32,
            source_action_kind: engine_proto::ActionKind::SupplementalKong as i32,
            source_tile: map_tile_to_engine(tile) as i32,
            replacement_draw: false,
            rob_kong_candidate: true,
        });

        match self.rule_engine.validate_action(request).await {
            Ok(response) => Ok(response.is_legal),
            Err(error) => Err(ActionValidationError::new(
                RejectCode::InternalError,
                format!("rule engine validation RPC failed while building rob-kong window: {error}"),
            )),
        }
    }

    fn build_validate_action_request_for_action(
        &self,
        actor_seat: Seat,
        action: &player_action_request::Action,
    ) -> Result<engine_proto::ValidateActionRequest, ActionValidationError> {
        let candidate_action = self.build_candidate_action(actor_seat, action)?;
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
            recent_action: self.build_recent_action_context(),
            players: self.build_engine_player_states(),
            actor_private_hand: Some(self.build_actor_private_hand(actor_seat)?),
            claim_window: None,
        })
    }

    fn send_claim_prompts(
        &mut self,
        action_window_id: u64,
        deadline_unix_ms: u64,
        eligible_seats: &[Seat],
    ) {
        // ActionPrompt 本身也是服务端生成的事件，需要独立事件号。
        let prompt_event_seq = self.allocate_event_seq();

        for seat in eligible_seats {
            let Some(player) = self.players_by_seat.get(seat) else {
                continue;
            };
            let Some(outbound_tx) = player.outbound_tx.as_ref() else {
                continue;
            };
            let Some(runtime) = self.round_runtime.as_ref() else {
                continue;
            };
            let Some(window) = runtime.active_claim_window.as_ref() else {
                continue;
            };
            let options = window.options_by_seat.get(seat).cloned().unwrap_or_default();

            let frame = ServerFrame {
                event_seq: prompt_event_seq,
                payload: Some(server_frame::Payload::ActionPrompt(ActionPrompt {
                    room_id: self.room_id.clone(),
                    match_id: self.match_id.clone(),
                    round_id: self.round_id(),
                    event_seq: prompt_event_seq,
                    action_window_id,
                    actor_seat: *seat as i32,
                    options,
                    deadline_unix_ms,
                })),
            };

            if let Err(error) = outbound_tx.send(frame) {
                warn!(
                    room_id = %self.room_id,
                    seat = %seat.as_str_name(),
                    ?error,
                    "failed to send ActionPrompt"
                );
            }
        }
    }

    fn advance_turn_after_passes(&mut self, source_seat: Seat) {
        // 当前窗口无人胜出时，轮到放弃牌来源座位的下家摸牌。
        let next_turn_seat = next_seat(source_seat);
        self.current_turn_seat = next_turn_seat;
        if let Some(runtime) = self.round_runtime.as_mut() {
            runtime.active_claim_window = None;
        }

        match self.draw_turn_tile(next_turn_seat, DrawSource::LiveWall) {
            Ok(Some(draw_outcome)) => {
                self.phase = GamePhase::WaitingDiscard;
                let draw_event_seq = self.allocate_event_seq();
                self.record_last_action(ResolvedActionRecord {
                    event_seq: draw_event_seq,
                    actor_seat: next_turn_seat,
                    action_kind: crate::proto::client::ActionKind::Draw,
                    tile: Some(draw_outcome.tile),
                    replacement_draw: draw_outcome.replacement_draw,
                    rob_kong_candidate: false,
                });
                self.persist_match_event(
                    "action_broadcast",
                    draw_event_seq,
                    Some(next_turn_seat),
                    serde_json::json!({
                        "room_id": self.room_id,
                        "match_id": self.match_id,
                        "round_id": self.round_id(),
                        "event_seq": draw_event_seq,
                        "actor_seat": next_turn_seat.as_str_name(),
                        "action_kind": "ACTION_KIND_DRAW",
                        "draw_tile": draw_outcome.tile.as_str_name(),
                        "replacement_draw": draw_outcome.replacement_draw,
                        "wall_tiles_remaining": self.wall_tiles_remaining,
                    }),
                );
            }
            Ok(None) => {
                self.phase = GamePhase::RoundSettlement;
            }
            Err(error) => {
                warn!(
                    room_id = %self.room_id,
                    ?error.message,
                    "failed to advance turn after passes"
                );
            }
        }
    }

    // RoundStart 后真正创建牌墙并发牌，而不是只切一个 Dealing phase。
    fn start_round(&mut self) -> Result<(), ActionValidationError> {
        // 每手牌开局都重建 RoundRuntimeState，避免上一手的窗口/牌墙状态残留。
        let wall = (self.wall_factory)(&self.room_config, &self.room_id, self.hand_number);
        if wall.len() <= DEAD_WALL_TILE_COUNT {
            return Err(ActionValidationError::new(
                RejectCode::InternalError,
                "wall factory returned too few tiles",
            ));
        }

        let dead_wall_start = wall.len() - DEAD_WALL_TILE_COUNT;
        self.round_runtime = Some(RoundRuntimeState {
            live_wall: VecDeque::from(wall[..dead_wall_start].to_vec()),
            dead_wall: VecDeque::from(wall[dead_wall_start..].to_vec()),
            active_claim_window: None,
            last_resolved_action: None,
            turn_index: 0,
            replacement_draw_pending: false,
        });

        self.phase = GamePhase::Dealing;
        self.current_turn_seat = self.dealer_seat;

        for seat in SEAT_ORDER {
            self.player_round_state
                .insert(seat, PlayerRoundState::default());
        }

        for player in self.players_by_seat.values_mut() {
            player.status = PlayerStatus::Playing;
        }

        for seat in SEAT_ORDER {
            for _ in 0..13 {
                self.deal_concealed_tile(seat)?;
            }
        }

        let Some(draw_outcome) = self.draw_turn_tile(self.dealer_seat, DrawSource::LiveWall)? else {
            return Err(ActionValidationError::new(
                RejectCode::InternalError,
                "live wall was unexpectedly exhausted during initial dealer draw",
            ));
        };

        self.phase = GamePhase::WaitingDiscard;
        let draw_event_seq = self.allocate_event_seq();
        self.record_last_action(ResolvedActionRecord {
            event_seq: draw_event_seq,
            actor_seat: self.dealer_seat,
            action_kind: crate::proto::client::ActionKind::Draw,
            tile: Some(draw_outcome.tile),
            replacement_draw: draw_outcome.replacement_draw,
            rob_kong_candidate: false,
        });
        self.sync_wall_counters();
        self.persist_round_started_event(draw_event_seq);

        Ok(())
    }

    fn pop_tile(&mut self, source: DrawSource) -> Option<Tile> {
        let tile = {
            let runtime = self.round_runtime.as_mut()?;
            match source {
                DrawSource::LiveWall => runtime.live_wall.pop_front(),
                DrawSource::DeadWall => runtime.dead_wall.pop_front(),
            }
        };

        self.sync_wall_counters();
        tile
    }

    // 初始牌与补花都落在 concealed_tiles，保持每家起手张数稳定。
    fn deal_concealed_tile(&mut self, seat: Seat) -> Result<(), ActionValidationError> {
        loop {
            let Some(tile) = self.pop_tile(DrawSource::LiveWall) else {
                return Err(ActionValidationError::new(
                    RejectCode::InternalError,
                    "live wall exhausted during initial dealing",
                ));
            };

            if self.room_config.enable_flower_tiles && is_flower_tile(tile) {
                let player = self
                    .player_round_state
                    .get_mut(&seat)
                    .expect("player round state should exist");
                player.flowers.push(tile);
                player.replacement_draw_count += 1;
                sort_tiles(&mut player.flowers);

                loop {
                    let Some(replacement_tile) = self.pop_tile(DrawSource::DeadWall) else {
                        return Err(ActionValidationError::new(
                            RejectCode::InternalError,
                            "dead wall exhausted while replacing flowers during dealing",
                        ));
                    };

                    if self.room_config.enable_flower_tiles && is_flower_tile(replacement_tile) {
                        let player = self
                            .player_round_state
                            .get_mut(&seat)
                            .expect("player round state should exist");
                        player.flowers.push(replacement_tile);
                        player.replacement_draw_count += 1;
                        sort_tiles(&mut player.flowers);
                        continue;
                    }

                    let player = self
                        .player_round_state
                        .get_mut(&seat)
                        .expect("player round state should exist");
                    player.concealed_tiles.push(replacement_tile);
                    sort_tiles(&mut player.concealed_tiles);
                    return Ok(());
                }
            }

            let player = self
                .player_round_state
                .get_mut(&seat)
                .expect("player round state should exist");
            player.concealed_tiles.push(tile);
            sort_tiles(&mut player.concealed_tiles);
            return Ok(());
        }
    }

    // 轮到玩家行动时，摸牌会单独挂在 drawn_tile 上，便于区分摸切与手切。
    fn draw_turn_tile(
        &mut self,
        seat: Seat,
        initial_source: DrawSource,
    ) -> Result<Option<DrawOutcome>, ActionValidationError> {
        let player = self
            .player_round_state
            .get_mut(&seat)
            .expect("player round state should exist");
        if player.drawn_tile.is_some() {
            return Err(ActionValidationError::new(
                RejectCode::InternalError,
                "player already holds a pending drawn tile",
            ));
        }

        let mut source = initial_source;
        let mut replacement_draw = matches!(initial_source, DrawSource::DeadWall);

        loop {
            let tile = match self.pop_tile(source) {
                Some(tile) => tile,
                None if matches!(source, DrawSource::LiveWall) => return Ok(None),
                None => {
                    return Err(ActionValidationError::new(
                        RejectCode::InternalError,
                        "dead wall exhausted while replacing flowers",
                    ))
                }
            };

            if self.room_config.enable_flower_tiles && is_flower_tile(tile) {
                let player = self
                    .player_round_state
                    .get_mut(&seat)
                    .expect("player round state should exist");
                player.flowers.push(tile);
                player.replacement_draw_count += 1;
                sort_tiles(&mut player.flowers);

                if let Some(runtime) = self.round_runtime.as_mut() {
                    runtime.replacement_draw_pending = true;
                }
                source = DrawSource::DeadWall;
                replacement_draw = true;
                continue;
            }

            let player = self
                .player_round_state
                .get_mut(&seat)
                .expect("player round state should exist");
            player.drawn_tile = Some(tile);
            if let Some(runtime) = self.round_runtime.as_mut() {
                runtime.replacement_draw_pending = false;
            }

            return Ok(Some(DrawOutcome {
                tile,
                replacement_draw,
            }));
        }
    }

    fn sync_wall_counters(&mut self) {
        if let Some(runtime) = self.round_runtime.as_ref() {
            self.wall_tiles_remaining = runtime.live_wall.len() as u32;
            self.dead_wall_tiles_remaining = runtime.dead_wall.len() as u32;
        }
    }

    fn record_last_action(&mut self, action: ResolvedActionRecord) {
        if let Some(runtime) = self.round_runtime.as_mut() {
            runtime.last_resolved_action = Some(action);
        }
    }

    fn schedule_claim_window_timeout(&self, action_window_id: u64) {
        let command_tx = self.room_command_tx.clone();
        let timeout = Duration::from_millis(self.room_config.action_timeout_ms as u64);

        tokio::spawn(async move {
            sleep(timeout).await;
            let _ = command_tx
                .send(RoomCommand::ClaimWindowTimeout { action_window_id })
                .await;
        });
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
}
