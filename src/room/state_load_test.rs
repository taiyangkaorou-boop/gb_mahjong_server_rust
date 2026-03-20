impl RoomState {
    // LoadTest 命令会在房间任务内部自举 4 个虚拟玩家，
    // 然后直接运行一个最小随机东风场，不经过 WebSocket、规则引擎和数据库。
    async fn handle_load_test_room(
        &mut self,
        spec: LoadTestRoomSpec,
        result_tx: oneshot::Sender<LoadTestRoomResult>,
    ) {
        let result = if self.runtime_mode != RoomRuntimeMode::LoadTest {
            LoadTestRoomResult {
                room_id: spec.room_id,
                room_index: spec.room_index,
                success: false,
                actions_executed: 0,
                hands_completed: 0,
                winner_seat: None,
                failure_reason: Some("room is not running in load-test mode".to_owned()),
            }
        } else {
            match self.run_load_test_room(&spec).await {
                Ok(result) => result,
                Err(error) => LoadTestRoomResult {
                    room_id: spec.room_id,
                    room_index: spec.room_index,
                    success: false,
                    actions_executed: 0,
                    hands_completed: self.match_runtime.completed_hands,
                    winner_seat: None,
                    failure_reason: Some(error.message),
                },
            }
        };

        let _ = result_tx.send(result);
        self.should_shutdown = true;
    }

    async fn run_load_test_room(
        &mut self,
        spec: &LoadTestRoomSpec,
    ) -> Result<LoadTestRoomResult, ActionValidationError> {
        self.initialize_load_test_players(spec.player_base_index);
        self.match_runtime.total_hands = spec.hands_per_match;
        self.start_round()?;

        let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::seed_from_u64(spec.seed);
        let mut actions_executed = 0_u32;

        while self.phase != GamePhase::MatchSettlement {
            let settled_hand_number = self.hand_number;
            let mut hand_actions = 0_u32;

            while hand_actions < spec.max_actions && self.phase == GamePhase::WaitingDiscard {
                let seat = self.current_turn_seat;
                let (tile, tsumogiri) = self.pick_random_load_test_discard(seat, &mut rng)?;
                self.apply_load_test_discard(seat, tile, tsumogiri)?;
                actions_executed += 1;
                hand_actions += 1;
            }

            if self.phase == GamePhase::WaitingDiscard {
                let winner_seat = match spec.winner_policy {
                    LoadTestWinnerPolicy::Random => {
                        let winner_index = rand::Rng::gen_range(&mut rng, 0..SEAT_ORDER.len());
                        SEAT_ORDER[winner_index]
                    }
                };
                self.settle_load_test_random_winner(winner_seat);
            }

            match self.phase {
                GamePhase::RoundSettlement => {
                    // LoadTest 模式不会走异步延迟推进，因此每手结束后要显式进入下一手。
                    self.advance_after_round_settlement(settled_hand_number).await;
                }
                GamePhase::MatchSettlement => break,
                GamePhase::WaitingDiscard => {
                    return Err(ActionValidationError::new(
                        RejectCode::InternalError,
                        "load-test room unexpectedly remained in WaitingDiscard after hand loop",
                    ));
                }
                other => {
                    return Err(ActionValidationError::new(
                        RejectCode::InternalError,
                        format!(
                            "load-test room entered unexpected phase {} during match simulation",
                            other.as_str_name()
                        ),
                    ));
                }
            }
        }

        let winner_seat = self
            .match_runtime
            .last_match_settlement
            .as_ref()
            .and_then(|summary| summary.standings.first())
            .and_then(|standing| Seat::try_from(standing.seat).ok())
            .map(|seat| seat.as_str_name().to_owned());

        Ok(LoadTestRoomResult {
            room_id: spec.room_id.clone(),
            room_index: spec.room_index,
            success: true,
            actions_executed,
            hands_completed: self.match_runtime.completed_hands,
            winner_seat,
            failure_reason: None,
        })
    }

    fn initialize_load_test_players(&mut self, player_base_index: u64) {
        self.players_by_seat.clear();
        self.player_round_state.clear();
        self.user_to_seat.clear();
        self.connection_to_user.clear();
        self.next_event_seq = 1;
        self.next_action_window_id = 1;
        self.phase = GamePhase::Lobby;
        self.prevailing_wind = Seat::East;
        self.dealer_seat = Seat::East;
        self.current_turn_seat = Seat::East;
        self.hand_number = 1;
        self.dealer_streak = 0;
        self.round_runtime = None;
        self.match_runtime = MatchRuntimeState::new();

        for (offset, seat) in SEAT_ORDER.into_iter().enumerate() {
            let user_id = format!("load-user-{}", player_base_index + offset as u64);
            self.user_to_seat.insert(user_id.clone(), seat);
            self.players_by_seat.insert(
                seat,
                PlayerSlot {
                    user_id,
                    display_name: format!("Load Player {}", player_base_index + offset as u64),
                    seat,
                    status: PlayerStatus::Ready,
                    score: INITIAL_SCORE,
                    connected: false,
                    auto_play_enabled: true,
                    connection_id: None,
                    outbound_tx: None,
                    resume_token: String::new(),
                },
            );
            self.player_round_state.insert(seat, PlayerRoundState::default());
        }
    }

    fn pick_random_load_test_discard(
        &self,
        seat: Seat,
        rng: &mut rand::rngs::StdRng,
    ) -> Result<(Tile, bool), ActionValidationError> {
        let round_state = self.player_round_state.get(&seat).ok_or_else(|| {
            ActionValidationError::new(
                RejectCode::NotInRoom,
                format!("seat {} does not exist in round state", seat.as_str_name()),
            )
        })?;

        let total_private_tiles =
            round_state.concealed_tiles.len() + usize::from(round_state.drawn_tile.is_some());
        if total_private_tiles == 0 {
            return Err(ActionValidationError::new(
                RejectCode::InvalidTile,
                "load-test player has no tile to discard",
            ));
        }

        let index = rand::Rng::gen_range(rng, 0..total_private_tiles);
        if index < round_state.concealed_tiles.len() {
            Ok((round_state.concealed_tiles[index], false))
        } else {
            Ok((
                round_state.drawn_tile.ok_or_else(|| {
                    ActionValidationError::new(
                        RejectCode::InvalidTile,
                        "load-test discard selected a missing drawn tile",
                    )
                })?,
                true,
            ))
        }
    }

    fn apply_load_test_discard(
        &mut self,
        seat: Seat,
        tile: Tile,
        tsumogiri: bool,
    ) -> Result<(), ActionValidationError> {
        if self.phase != GamePhase::WaitingDiscard {
            return Err(ActionValidationError::new(
                RejectCode::PhaseMismatch,
                "load-test discard is only available during WaitingDiscard",
            ));
        }
        if seat != self.current_turn_seat {
            return Err(ActionValidationError::new(
                RejectCode::NotYourTurn,
                "load-test discard selected a non-current seat",
            ));
        }

        self.discard_tile_from_hand(seat, tile, tsumogiri)?;
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
        self.advance_turn_after_passes(seat);

        Ok(())
    }

    fn settle_load_test_random_winner(&mut self, winner_seat: Seat) {
        // 压测模式只需要一个稳定的收口路径，不做真实麻将结算。
        // 这里用固定零和分数变化标记一手牌结束，并把结果记入整场统计。
        for seat in SEAT_ORDER {
            if let Some(player) = self.players_by_seat.get_mut(&seat) {
                if seat == winner_seat {
                    player.score += 3000;
                } else {
                    player.score -= 1000;
                }
            }
        }

        let winning_tile = self.player_round_state.get(&winner_seat).and_then(|round_state| {
            round_state
                .drawn_tile
                .or_else(|| round_state.concealed_tiles.last().copied())
        });

        if let Some(runtime) = self.round_runtime.as_mut() {
            runtime.active_claim_window = None;
        }
        self.phase = GamePhase::RoundSettlement;
        let event_seq = self.allocate_event_seq();
        self.record_last_action(ResolvedActionRecord {
            event_seq,
            actor_seat: winner_seat,
            action_kind: crate::proto::client::ActionKind::DeclareWin,
            tile: winning_tile,
            replacement_draw: false,
            rob_kong_candidate: false,
        });
        self.record_round_outcome(Some(winner_seat), winner_seat == self.current_turn_seat);
    }
}
