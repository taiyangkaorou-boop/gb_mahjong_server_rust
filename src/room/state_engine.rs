impl RoomState {
    // 这一段专门负责把 Rust 房间状态投影成规则引擎可理解的 gRPC 请求。
    // 业务状态仍以 RoomState 为准，C++ 引擎只做纯计算与校验。
    fn build_validate_action_request(
        &self,
        actor_seat: Seat,
        request: &PlayerActionRequest,
    ) -> Result<engine_proto::ValidateActionRequest, ActionValidationError> {
        // 这是 Rust -> C++ 的关键组包入口：
        // 规则引擎拿到的是“当前房间真实状态快照”，而不是只拿玩家发来的动作字段。
        let Some(action) = request.action.as_ref() else {
            return Err(ActionValidationError::new(
                RejectCode::InvalidRequest,
                "player action payload must be set",
            ));
        };

        let candidate_action = self.build_candidate_action(actor_seat, action)?;
        let recent_action = self.build_recent_action_context();
        let claim_window = self.build_claim_window_context(request.action_window_id);

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
            actor_private_hand: Some(self.build_actor_private_hand(actor_seat)?),
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

    fn build_recent_action_context(&self) -> Option<engine_proto::RecentActionContext> {
        let action = self
            .round_runtime
            .as_ref()
            .and_then(|runtime| runtime.last_resolved_action.as_ref())?;

        Some(engine_proto::RecentActionContext {
            source_event_seq: action.event_seq,
            source_seat: map_seat_to_engine(action.actor_seat) as i32,
            source_action_kind: map_action_kind_to_engine(action.action_kind) as i32,
            source_tile: action
                .tile
                .map(|tile| map_tile_to_engine(tile) as i32)
                .unwrap_or(engine_proto::Tile::Unspecified as i32),
            replacement_draw: action.replacement_draw,
            rob_kong_candidate: action.rob_kong_candidate,
        })
    }

    fn build_claim_window_context(
        &self,
        action_window_id: u64,
    ) -> Option<engine_proto::ClaimWindowContext> {
        let runtime = self.round_runtime.as_ref()?;
        let window = runtime.active_claim_window.as_ref()?;
        if action_window_id != 0 && action_window_id != window.action_window_id {
            return None;
        }

        Some(engine_proto::ClaimWindowContext {
            action_window_id: window.action_window_id,
            source_event_seq: window.source_event_seq,
            source_seat: map_seat_to_engine(window.source_seat) as i32,
            target_tile: map_tile_to_engine(window.target_tile) as i32,
            trigger_action_kind: map_action_kind_to_engine(window.trigger_action_kind) as i32,
            eligible_seats: window
                .eligible_seats
                .iter()
                .map(|seat| map_seat_to_engine(*seat) as i32)
                .collect(),
        })
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
        let replacement_draw_pending = self
            .round_runtime
            .as_ref()
            .map(|runtime| runtime.replacement_draw_pending)
            .unwrap_or(false);
        engine_proto::RoundContext {
            prevailing_wind: map_seat_to_engine(self.prevailing_wind) as i32,
            dealer_seat: map_seat_to_engine(self.dealer_seat) as i32,
            current_turn_seat: map_seat_to_engine(self.current_turn_seat) as i32,
            hand_number: self.hand_number,
            dealer_streak: self.dealer_streak,
            wall_tiles_remaining: self.wall_tiles_remaining,
            dead_wall_tiles_remaining: self.dead_wall_tiles_remaining,
            replacement_draw_pending,
            is_last_tile: self.wall_tiles_remaining <= 1,
        }
    }

    fn build_engine_player_states(&self) -> Vec<engine_proto::EnginePlayerState> {
        SEAT_ORDER
            .into_iter()
            .filter_map(|seat| {
                let player = self.players_by_seat.get(&seat)?;
                let round_state = self.player_round_state.get(&seat)?;
                Some((player, round_state))
            })
            .map(|(player, round_state)| engine_proto::EnginePlayerState {
                seat: map_seat_to_engine(player.seat) as i32,
                current_score: player.score,
                concealed_tile_count: private_tile_count(round_state) as u32,
                melds: round_state
                    .melds
                    .iter()
                    .map(map_client_meld_to_engine)
                    .collect(),
                discards: round_state
                    .discards
                    .iter()
                    .map(map_client_discard_to_engine)
                    .collect(),
                flowers: round_state
                    .flowers
                    .iter()
                    .map(|tile| map_tile_to_engine(*tile) as i32)
                    .collect(),
            })
            .collect()
    }

    fn build_actor_private_hand(
        &self,
        actor_seat: Seat,
    ) -> Result<engine_proto::PrivateHandContext, ActionValidationError> {
        let round_state = self.player_round_state.get(&actor_seat).ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::NotInRoom,
                format!("seat {} does not exist in round state", actor_seat.as_str_name()),
            )
        })?;

        Ok(engine_proto::PrivateHandContext {
            concealed_tiles: round_state
                .concealed_tiles
                .iter()
                .map(|tile| map_tile_to_engine(*tile) as i32)
                .collect(),
            drawn_tile: round_state
                .drawn_tile
                .map(|tile| map_tile_to_engine(tile) as i32)
                .unwrap_or(engine_proto::Tile::Unspecified as i32),
        })
    }

    // DeclareWin 既可能来自当前摸牌者自摸，也可能来自抢操作窗口中的点和/抢杠胡。
    // 这里先在 Rust 侧做最基本的场景约束，再把最终合法性留给规则引擎确认。
    fn resolve_declare_win_context(
        &self,
        seat: Seat,
        request: &PlayerActionRequest,
        declare_win: &crate::proto::client::DeclareWinAction,
        win_type: crate::proto::client::WinType,
        winning_tile: Tile,
    ) -> Result<Option<Seat>, ActionValidationError> {
        match win_type {
            crate::proto::client::WinType::SelfDraw | crate::proto::client::WinType::KongDraw => {
                if self.phase != GamePhase::WaitingDiscard {
                    return Err(ActionValidationError::new(
                        RejectCode::PhaseMismatch,
                        "self draw win is only available during WaitingDiscard",
                    ));
                }
                if seat != self.current_turn_seat {
                    return Err(ActionValidationError::new(
                        RejectCode::NotYourTurn,
                        "only the current turn seat may declare self draw win",
                    ));
                }
                let drawn_tile = self
                    .player_round_state
                    .get(&seat)
                    .and_then(|round_state| round_state.drawn_tile)
                    .ok_or_else(|| {
                        ActionValidationError::new(
                            RejectCode::ActionNotAvailable,
                            "self draw win requires a pending drawn tile",
                        )
                    })?;
                if drawn_tile != winning_tile {
                    return Err(ActionValidationError::new(
                        RejectCode::InvalidTile,
                        "winning_tile does not match the current pending drawn tile",
                    ));
                }

                Ok(None)
            }
            crate::proto::client::WinType::Discard | crate::proto::client::WinType::RobKong => {
                let runtime = self.round_runtime.as_ref().ok_or_else(|| {
                    ActionValidationError::new(
                        RejectCode::PhaseMismatch,
                        "discard-based win requires an active round runtime",
                    )
                })?;
                let window = runtime.active_claim_window.as_ref().ok_or_else(|| {
                    ActionValidationError::new(
                        RejectCode::ActionWindowClosed,
                        "discard-based win requires an active claim window",
                    )
                })?;

                if request.action_window_id != window.action_window_id {
                    return Err(ActionValidationError::new(
                        RejectCode::ActionWindowClosed,
                        "request references a stale claim window",
                    ));
                }
                if !window.eligible_seats.contains(&seat) {
                    return Err(ActionValidationError::new(
                        RejectCode::ActionNotAvailable,
                        "seat is not eligible to declare win in the current claim window",
                    ));
                }
                if window.target_tile != winning_tile {
                    return Err(ActionValidationError::new(
                        RejectCode::InvalidTile,
                        "winning_tile does not match the active claim window target tile",
                    ));
                }

                let source_seat = parse_client_seat(declare_win.source_seat)?;
                if source_seat != window.source_seat {
                    return Err(ActionValidationError::new(
                        RejectCode::InvalidRequest,
                        "declare win source_seat does not match the active claim window",
                    ));
                }
                if declare_win.source_event_seq != window.source_event_seq {
                    return Err(ActionValidationError::new(
                        RejectCode::InvalidRequest,
                        "declare win source_event_seq does not match the active claim window",
                    ));
                }

                Ok(Some(source_seat))
            }
            crate::proto::client::WinType::Unspecified => Err(ActionValidationError::new(
                RejectCode::InvalidRequest,
                "declare win requires a concrete win_type",
            )),
        }
    }

    fn validate_claim_window_submission(
        &self,
        seat: Seat,
        action_window_id: u64,
        source_event_seq: u64,
    ) -> Result<(), ActionValidationError> {
        let runtime = self.round_runtime.as_ref().ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::PhaseMismatch,
                "no active round runtime exists",
            )
        })?;
        let window = runtime.active_claim_window.as_ref().ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::ActionWindowClosed,
                "there is no active claim window",
            )
        })?;

        if action_window_id != window.action_window_id {
            return Err(ActionValidationError::new(
                RejectCode::ActionWindowClosed,
                "request references a stale claim window",
            ));
        }
        if source_event_seq != window.source_event_seq {
            return Err(ActionValidationError::new(
                RejectCode::InvalidRequest,
                "claim source_event_seq does not match the active claim window",
            ));
        }
        if !window.eligible_seats.contains(&seat) {
            return Err(ActionValidationError::new(
                RejectCode::ActionNotAvailable,
                "seat is not eligible in the current claim window",
            ));
        }

        Ok(())
    }

    fn validate_claim_choice(
        &self,
        seat: Seat,
        claim: &crate::proto::client::ClaimAction,
    ) -> Result<(), ActionValidationError> {
        let runtime = self.round_runtime.as_ref().ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::PhaseMismatch,
                "no active round runtime exists",
            )
        })?;
        let window = runtime.active_claim_window.as_ref().ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::ActionWindowClosed,
                "there is no active claim window",
            )
        })?;

        let target_tile = parse_client_tile(claim.target_tile)?;
        if target_tile != window.target_tile {
            return Err(ActionValidationError::new(
                RejectCode::InvalidTile,
                "claim target_tile does not match the active claim window",
            ));
        }
        if parse_client_seat(claim.source_seat)? != window.source_seat {
            return Err(ActionValidationError::new(
                RejectCode::InvalidRequest,
                "claim source_seat does not match the active claim window",
            ));
        }

        let submitted_claim_kind = parse_client_claim_kind(claim.claim_kind)?;
        let mut submitted_consume_tiles = claim
            .consume_tiles
            .iter()
            .copied()
            .map(parse_client_tile)
            .collect::<Result<Vec<_>, _>>()?;
        sort_tiles(&mut submitted_consume_tiles);

        let Some(options) = window.options_by_seat.get(&seat) else {
            return Err(ActionValidationError::new(
                RejectCode::ActionNotAvailable,
                "seat does not have any claim options in the current window",
            ));
        };

        let matched = options.iter().any(|option| {
            if option.action_kind != crate::proto::client::ActionKind::Unspecified as i32
                && option.action_kind != map_claim_kind_to_client_action(submitted_claim_kind) as i32
            {
                return false;
            }
            if option.claim_kind != submitted_claim_kind as i32 {
                return false;
            }

            let mut option_consume_tiles = option
                .consume_tiles
                .iter()
                .copied()
                .map(parse_client_tile)
                .collect::<Result<Vec<_>, _>>()
                .unwrap_or_default();
            sort_tiles(&mut option_consume_tiles);

            option_consume_tiles == submitted_consume_tiles
        });

        if !matched {
            return Err(ActionValidationError::new(
                RejectCode::ActionNotAvailable,
                "submitted claim does not match any server-provided option",
            ));
        }

        Ok(())
    }

    fn select_highest_priority_claim_response(
        &self,
        window: &ActiveClaimWindowState,
        responded_seats: &HashMap<Seat, ClaimResponse>,
    ) -> Option<(Seat, ClaimResponse)> {
        responded_seats
            .iter()
            .filter_map(|(seat, response)| {
                let priority = claim_response_priority(response)?;
                Some((*seat, response.clone(), priority))
            })
            .max_by(|(left_seat, _, left_priority), (right_seat, _, right_priority)| {
                left_priority
                    .cmp(right_priority)
                    .then_with(|| {
                        claim_priority_distance(window.source_seat, *right_seat)
                            .cmp(&claim_priority_distance(window.source_seat, *left_seat))
                    })
            })
            .map(|(seat, response, _)| (seat, response))
    }

    fn apply_supplemental_kong(
        &mut self,
        seat: Seat,
        tile: Tile,
        source_event_seq: u64,
    ) -> Result<(), ActionValidationError> {
        // 这里是真正落地补杠的地方：
        // 先把已有 peng 升级成 supplemental kong，再从死墙补一张牌。
        let round_state = self.player_round_state.get_mut(&seat).ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::NotInRoom,
                format!("seat {} does not exist in round state", seat.as_str_name()),
            )
        })?;

        let Some(meld_index) = round_state.melds.iter().position(|meld| {
            meld.kind == crate::proto::client::MeldKind::Peng as i32
                && meld.claimed_tile == tile as i32
        }) else {
            return Err(ActionValidationError::new(
                RejectCode::ActionNotAvailable,
                "supplemental kong requires an existing peng meld of the target tile",
            ));
        };

        consume_tile_for_supplemental_kong(round_state, tile)?;

        let meld = round_state
            .melds
            .get_mut(meld_index)
            .expect("meld index should remain valid");
        meld.kind = crate::proto::client::MeldKind::SupplementalKong as i32;
        meld.tiles.push(tile as i32);
        meld.tiles.sort_unstable();
        meld.source_event_seq = source_event_seq;
        let resulting_meld = meld.clone();
        let _ = round_state;

        self.current_turn_seat = seat;
        self.phase = GamePhase::ResolvingKong;
        self.record_last_action(ResolvedActionRecord {
            event_seq: source_event_seq,
            actor_seat: seat,
            action_kind: crate::proto::client::ActionKind::SupplementalKong,
            tile: Some(tile),
            replacement_draw: false,
            rob_kong_candidate: false,
        });
        self.broadcast_supplemental_kong_action(seat, source_event_seq, tile, resulting_meld);

        let draw_outcome = self
            .draw_turn_tile(seat, DrawSource::DeadWall)?
            .ok_or_else(|| {
                ActionValidationError::new(
                    RejectCode::InternalError,
                    "dead wall was unexpectedly exhausted for supplemental kong replacement draw",
                )
            })?;
        self.phase = GamePhase::WaitingDiscard;
        let draw_event_seq = self.allocate_event_seq();
        self.record_last_action(ResolvedActionRecord {
            event_seq: draw_event_seq,
            actor_seat: seat,
            action_kind: crate::proto::client::ActionKind::Draw,
            tile: Some(draw_outcome.tile),
            replacement_draw: true,
            rob_kong_candidate: false,
        });
        self.broadcast_draw_action(seat, draw_event_seq, draw_outcome.tile, true);

        Ok(())
    }

    fn apply_resolved_claim_action(
        &mut self,
        seat: Seat,
        claim: &crate::proto::client::ClaimAction,
    ) -> Result<(), ActionValidationError> {
        // 统一处理 claim window 裁决胜出的吃/碰/明杠动作。
        let claim_kind = parse_client_claim_kind(claim.claim_kind)?;
        let target_tile = parse_client_tile(claim.target_tile)?;
        let source_seat = parse_client_seat(claim.source_seat)?;
        let mut consume_tiles = claim
            .consume_tiles
            .iter()
            .copied()
            .map(parse_client_tile)
            .collect::<Result<Vec<_>, _>>()?;
        sort_tiles(&mut consume_tiles);

        let player_round_state = self.player_round_state.get_mut(&seat).ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::NotInRoom,
                format!("seat {} does not exist in round state", seat.as_str_name()),
            )
        })?;
        if player_round_state.drawn_tile.is_some() {
            return Err(ActionValidationError::new(
                RejectCode::ActionNotAvailable,
                "claiming a discard is not allowed while a pending drawn tile exists",
            ));
        }

        remove_tiles_from_concealed(&mut player_round_state.concealed_tiles, &consume_tiles)?;

        let meld_kind = map_claim_kind_to_meld_kind(claim_kind);
        let action_kind = map_claim_kind_to_client_action(claim_kind);
        let mut meld_tiles = consume_tiles.clone();
        meld_tiles.push(target_tile);
        sort_tiles(&mut meld_tiles);
        player_round_state.melds.push(Meld {
            kind: meld_kind as i32,
            tiles: meld_tiles.iter().map(|tile| *tile as i32).collect(),
            from_seat: source_seat as i32,
            claimed_tile: target_tile as i32,
            concealed: false,
            source_event_seq: claim.source_event_seq,
        });
        let resulting_meld = player_round_state
            .melds
            .last()
            .cloned()
            .expect("just pushed meld should exist");
        let _ = player_round_state;

        self.mark_discard_claimed(source_seat, claim.source_event_seq);
        self.current_turn_seat = seat;
        self.phase = GamePhase::WaitingDiscard;
        let claim_event_seq = self.allocate_event_seq();
        self.record_last_action(ResolvedActionRecord {
            event_seq: claim_event_seq,
            actor_seat: seat,
            action_kind,
            tile: Some(target_tile),
            replacement_draw: false,
            rob_kong_candidate: false,
        });
        self.broadcast_claim_action(
            seat,
            claim_event_seq,
            claim_kind,
            target_tile,
            source_seat,
            consume_tiles.clone(),
            resulting_meld,
        );

        if claim_kind == crate::proto::client::ClaimKind::ExposedKong {
            let draw_outcome = self
                .draw_turn_tile(seat, DrawSource::DeadWall)?
                .ok_or_else(|| {
                    ActionValidationError::new(
                        RejectCode::InternalError,
                        "dead wall was unexpectedly exhausted for exposed kong replacement draw",
                    )
                })?;
            let draw_event_seq = self.allocate_event_seq();
            self.record_last_action(ResolvedActionRecord {
                event_seq: draw_event_seq,
                actor_seat: seat,
                action_kind: crate::proto::client::ActionKind::Draw,
                tile: Some(draw_outcome.tile),
                replacement_draw: true,
                rob_kong_candidate: false,
            });
            self.broadcast_draw_action(seat, draw_event_seq, draw_outcome.tile, true);
        }

        Ok(())
    }

    async fn finalize_winning_action(
        &mut self,
        winner_seat: Seat,
        discarder_seat: Option<Seat>,
        win_type: crate::proto::client::WinType,
        winning_tile: Tile,
        source_event_seq: u64,
    ) -> Result<(), ActionValidationError> {
        // 和牌收口路径：
        // 1. 构造终局动作快照
        // 2. 调规则引擎算番/算分
        // 3. 更新玩家分数
        // 4. 广播和牌动作与结算结果
        let terminal_action = ResolvedActionRecord {
            event_seq: self.allocate_event_seq(),
            actor_seat: winner_seat,
            action_kind: crate::proto::client::ActionKind::DeclareWin,
            tile: Some(winning_tile),
            replacement_draw: matches!(win_type, crate::proto::client::WinType::KongDraw),
            rob_kong_candidate: matches!(win_type, crate::proto::client::WinType::RobKong),
        };

        let calculate_request = self.build_calculate_score_request(
            winner_seat,
            discarder_seat,
            win_type,
            winning_tile,
            &terminal_action,
        )?;
        let calculate_response = self
            .rule_engine
            .calculate_score(calculate_request)
            .await
            .map_err(|error| {
                ActionValidationError::new(
                    RejectCode::InternalError,
                    format!("rule engine calculate_score RPC failed: {error}"),
                )
            })?;

        if let Some(discarder_seat) = discarder_seat {
            self.mark_discard_claimed(discarder_seat, source_event_seq);
        }
        if let Some(runtime) = self.round_runtime.as_mut() {
            runtime.active_claim_window = None;
        }
        self.phase = GamePhase::RoundSettlement;
        let terminal_event_seq = terminal_action.event_seq;
        self.record_last_action(terminal_action);
        self.apply_score_deltas(&calculate_response.score_delta_by_seat);
        self.broadcast_win_action(
            winner_seat,
            terminal_event_seq,
            win_type,
            winning_tile,
            discarder_seat,
            source_event_seq,
        );
        self.broadcast_round_settlement(
            winner_seat,
            discarder_seat,
            win_type,
            winning_tile,
            &calculate_response,
        );

        Ok(())
    }

    fn build_calculate_score_request(
        &self,
        winner_seat: Seat,
        discarder_seat: Option<Seat>,
        win_type: crate::proto::client::WinType,
        winning_tile: Tile,
        terminal_action: &ResolvedActionRecord,
    ) -> Result<engine_proto::CalculateScoreRequest, ActionValidationError> {
        Ok(engine_proto::CalculateScoreRequest {
            room_id: self.room_id.clone(),
            match_id: self.match_id.clone(),
            round_id: self.round_id(),
            rule_config: Some(self.build_engine_rule_config()),
            round_context: Some(self.build_engine_round_context()),
            win_type: map_win_type_to_engine(win_type) as i32,
            winner_seat: map_seat_to_engine(winner_seat) as i32,
            discarder_seat: discarder_seat
                .map(map_seat_to_engine)
                .unwrap_or(engine_proto::Seat::Unspecified) as i32,
            winning_tile: map_tile_to_engine(winning_tile) as i32,
            hands: self.build_engine_hand_snapshots(winner_seat, discarder_seat),
            settlement_flags: self
                .build_engine_settlement_flags(win_type)
                .into_iter()
                .map(|flag| flag as i32)
                .collect(),
            terminal_action: Some(Self::build_recent_action_context_from(terminal_action)),
        })
    }

    fn build_engine_hand_snapshots(
        &self,
        winner_seat: Seat,
        discarder_seat: Option<Seat>,
    ) -> Vec<engine_proto::HandSnapshot> {
        SEAT_ORDER
            .into_iter()
            .filter_map(|seat| {
                let player = self.players_by_seat.get(&seat)?;
                let round_state = self.player_round_state.get(&seat)?;
                Some((seat, player, round_state))
            })
            .map(|(seat, player, round_state)| engine_proto::HandSnapshot {
                seat: map_seat_to_engine(seat) as i32,
                concealed_tiles: merged_private_tiles(round_state)
                    .into_iter()
                    .map(|tile| map_tile_to_engine(tile) as i32)
                    .collect(),
                melds: round_state
                    .melds
                    .iter()
                    .map(map_client_meld_to_engine)
                    .collect(),
                flowers: round_state
                    .flowers
                    .iter()
                    .map(|tile| map_tile_to_engine(*tile) as i32)
                    .collect(),
                current_score: player.score,
                winner: seat == winner_seat,
                discarder: Some(seat) == discarder_seat,
            })
            .collect()
    }

    fn build_engine_settlement_flags(
        &self,
        win_type: crate::proto::client::WinType,
    ) -> Vec<engine_proto::SettlementFlag> {
        let mut flags = vec![match win_type {
            crate::proto::client::WinType::SelfDraw => engine_proto::SettlementFlag::SelfDraw,
            crate::proto::client::WinType::Discard => engine_proto::SettlementFlag::DiscardWin,
            crate::proto::client::WinType::RobKong => engine_proto::SettlementFlag::RobKongWin,
            crate::proto::client::WinType::KongDraw => engine_proto::SettlementFlag::KongDrawWin,
            crate::proto::client::WinType::Unspecified => {
                engine_proto::SettlementFlag::Unspecified
            }
        }];

        if self.build_engine_round_context().is_last_tile {
            let last_tile_flag = match win_type {
                crate::proto::client::WinType::SelfDraw
                | crate::proto::client::WinType::KongDraw => {
                    engine_proto::SettlementFlag::LastTileDraw
                }
                crate::proto::client::WinType::Discard
                | crate::proto::client::WinType::RobKong => {
                    engine_proto::SettlementFlag::LastTileClaim
                }
                crate::proto::client::WinType::Unspecified => {
                    engine_proto::SettlementFlag::Unspecified
                }
            };

            if last_tile_flag != engine_proto::SettlementFlag::Unspecified {
                flags.push(last_tile_flag);
            }
        }

        flags
    }

    fn build_recent_action_context_from(
        action: &ResolvedActionRecord,
    ) -> engine_proto::RecentActionContext {
        engine_proto::RecentActionContext {
            source_event_seq: action.event_seq,
            source_seat: map_seat_to_engine(action.actor_seat) as i32,
            source_action_kind: map_action_kind_to_engine(action.action_kind) as i32,
            source_tile: action
                .tile
                .map(|tile| map_tile_to_engine(tile) as i32)
                .unwrap_or(engine_proto::Tile::Unspecified as i32),
            replacement_draw: action.replacement_draw,
            rob_kong_candidate: action.rob_kong_candidate,
        }
    }
}
