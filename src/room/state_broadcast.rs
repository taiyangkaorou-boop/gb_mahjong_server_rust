impl RoomState {
    // 这一段只负责对外广播和同步，不再混入规则裁决。
    // 这样可以把“状态推进”和“协议投影”分开看，减少 review 噪音。
    fn broadcast_discard_action(&self, seat: Seat, event_seq: u64, tile: Tile) {
        // 增量广播只发送本次动作及其直接效果，客户端可用 event_seq 做时序收敛。
        let discard = self
            .player_round_state
            .get(&seat)
            .and_then(|round_state| round_state.discards.last())
            .cloned();
        let resulting_effects = discard
            .clone()
            .map(|discard| {
                vec![ResultingEffect {
                    effect: Some(
                        crate::proto::client::resulting_effect::Effect::DiscardAdded(
                            DiscardAdded {
                                seat: seat as i32,
                                discard: Some(discard),
                            },
                        ),
                    ),
                }]
            })
            .unwrap_or_default();
        self.persist_match_event(
            "action_broadcast",
            event_seq,
            Some(seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "event_seq": event_seq,
                "actor_seat": seat.as_str_name(),
                "action_kind": "ACTION_KIND_DISCARD",
                "tile": tile.as_str_name(),
                "tsumogiri": discard
                    .as_ref()
                    .map(|discard| discard.drawn_and_discarded)
                    .unwrap_or(false),
                "wall_tiles_remaining": self.wall_tiles_remaining,
            }),
        );

        self.broadcast_action_frame(ServerFrame {
            event_seq,
            payload: Some(server_frame::Payload::ActionBroadcast(ActionBroadcast {
                room_id: self.room_id.clone(),
                match_id: self.match_id.clone(),
                round_id: self.round_id(),
                event_seq,
                actor_seat: seat as i32,
                action_kind: crate::proto::client::ActionKind::Discard as i32,
                action_detail: Some(ActionDetail {
                    detail: Some(crate::proto::client::action_detail::Detail::Discard(
                        crate::proto::client::DiscardAction {
                            tile: tile as i32,
                            tsumogiri: discard
                                .as_ref()
                                .map(|discard| discard.drawn_and_discarded)
                                .unwrap_or(false),
                        },
                    )),
                }),
                resulting_effects,
                resulting_phase: self.phase as i32,
                next_turn_seat: self.current_turn_seat as i32,
                wall_tiles_remaining: self.wall_tiles_remaining,
                action_deadline_unix_ms: 0,
            })),
        });
    }

    fn broadcast_draw_action(
        &self,
        seat: Seat,
        event_seq: u64,
        tile: Tile,
        replacement_draw: bool,
    ) {
        // 当前先发送可见版摸牌细节；后续如果需要更严格的隐藏信息，可在这里拆成定向广播。
        self.persist_match_event(
            "action_broadcast",
            event_seq,
            Some(seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "event_seq": event_seq,
                "actor_seat": seat.as_str_name(),
                "action_kind": "ACTION_KIND_DRAW",
                "draw_tile": tile.as_str_name(),
                "replacement_draw": replacement_draw,
                "wall_tiles_remaining": self.wall_tiles_remaining,
            }),
        );

        self.broadcast_action_frame(ServerFrame {
            event_seq,
            payload: Some(server_frame::Payload::ActionBroadcast(ActionBroadcast {
                room_id: self.room_id.clone(),
                match_id: self.match_id.clone(),
                round_id: self.round_id(),
                event_seq,
                actor_seat: seat as i32,
                action_kind: crate::proto::client::ActionKind::Draw as i32,
                action_detail: Some(ActionDetail {
                    detail: Some(crate::proto::client::action_detail::Detail::Draw(
                        DrawDetail {
                            tile: tile as i32,
                            visible_to_recipient: true,
                            replacement_draw,
                        },
                    )),
                }),
                resulting_effects: vec![ResultingEffect {
                    effect: Some(crate::proto::client::resulting_effect::Effect::TurnAdvanced(
                        TurnAdvanced {
                            next_turn_seat: seat as i32,
                            resulting_phase: self.phase as i32,
                        },
                    )),
                }],
                resulting_phase: self.phase as i32,
                next_turn_seat: seat as i32,
                wall_tiles_remaining: self.wall_tiles_remaining,
                action_deadline_unix_ms: self
                    .round_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.active_claim_window.as_ref())
                    .map(|window| window.deadline_unix_ms)
                    .unwrap_or(0),
            })),
        });
    }

    fn broadcast_claim_action(
        &self,
        seat: Seat,
        event_seq: u64,
        claim_kind: crate::proto::client::ClaimKind,
        target_tile: Tile,
        source_seat: Seat,
        consume_tiles: Vec<Tile>,
        resulting_meld: Meld,
    ) {
        let resulting_effects = vec![ResultingEffect {
            effect: Some(crate::proto::client::resulting_effect::Effect::MeldAdded(
                MeldAdded {
                    seat: seat as i32,
                    meld: Some(resulting_meld.clone()),
                },
            )),
        }];
        self.persist_match_event(
            "action_broadcast",
            event_seq,
            Some(seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "event_seq": event_seq,
                "actor_seat": seat.as_str_name(),
                "action_kind": claim_action_kind_label(claim_kind),
                "target_tile": target_tile.as_str_name(),
                "source_seat": source_seat.as_str_name(),
                "consume_tiles": consume_tiles.iter().map(|tile| tile.as_str_name()).collect::<Vec<_>>(),
                "resulting_meld": meld_json(&resulting_meld),
            }),
        );

        self.broadcast_action_frame(ServerFrame {
            event_seq,
            payload: Some(server_frame::Payload::ActionBroadcast(ActionBroadcast {
                room_id: self.room_id.clone(),
                match_id: self.match_id.clone(),
                round_id: self.round_id(),
                event_seq,
                actor_seat: seat as i32,
                action_kind: map_claim_kind_to_client_action(claim_kind) as i32,
                action_detail: Some(ActionDetail {
                    detail: Some(crate::proto::client::action_detail::Detail::Claim(
                        ResolvedClaim {
                            claim_kind: claim_kind as i32,
                            target_tile: target_tile as i32,
                            consume_tiles: consume_tiles
                                .into_iter()
                                .map(|tile| tile as i32)
                                .collect(),
                            source_seat: source_seat as i32,
                            resulting_meld: Some(resulting_meld),
                        },
                    )),
                }),
                resulting_effects,
                resulting_phase: self.phase as i32,
                next_turn_seat: self.current_turn_seat as i32,
                wall_tiles_remaining: self.wall_tiles_remaining,
                action_deadline_unix_ms: 0,
            })),
        });
    }

    fn broadcast_supplemental_kong_action(
        &self,
        seat: Seat,
        event_seq: u64,
        tile: Tile,
        resulting_meld: Meld,
    ) {
        self.persist_match_event(
            "action_broadcast",
            event_seq,
            Some(seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "event_seq": event_seq,
                "actor_seat": seat.as_str_name(),
                "action_kind": "ACTION_KIND_SUPPLEMENTAL_KONG",
                "tile": tile.as_str_name(),
                "source_event_seq": event_seq,
                "resulting_meld": meld_json(&resulting_meld),
            }),
        );

        self.broadcast_action_frame(ServerFrame {
            event_seq,
            payload: Some(server_frame::Payload::ActionBroadcast(ActionBroadcast {
                room_id: self.room_id.clone(),
                match_id: self.match_id.clone(),
                round_id: self.round_id(),
                event_seq,
                actor_seat: seat as i32,
                action_kind: crate::proto::client::ActionKind::SupplementalKong as i32,
                action_detail: Some(ActionDetail {
                    detail: Some(
                        crate::proto::client::action_detail::Detail::SupplementalKong(
                            crate::proto::client::SupplementalKongAction { tile: tile as i32 },
                        ),
                    ),
                }),
                resulting_effects: vec![ResultingEffect {
                    effect: Some(crate::proto::client::resulting_effect::Effect::MeldAdded(
                        MeldAdded {
                            seat: seat as i32,
                            meld: Some(resulting_meld),
                        },
                    )),
                }],
                resulting_phase: self.phase as i32,
                next_turn_seat: seat as i32,
                wall_tiles_remaining: self.wall_tiles_remaining,
                action_deadline_unix_ms: 0,
            })),
        });
    }

    fn broadcast_win_action(
        &self,
        seat: Seat,
        event_seq: u64,
        win_type: crate::proto::client::WinType,
        winning_tile: Tile,
        discarder_seat: Option<Seat>,
        source_event_seq: u64,
    ) {
        self.persist_match_event(
            "action_broadcast",
            event_seq,
            Some(seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "event_seq": event_seq,
                "actor_seat": seat.as_str_name(),
                "action_kind": "ACTION_KIND_DECLARE_WIN",
                "win_type": win_type.as_str_name(),
                "winning_tile": winning_tile.as_str_name(),
                "discarder_seat": discarder_seat.map(|seat| seat.as_str_name().to_owned()),
                "source_event_seq": source_event_seq,
            }),
        );

        self.broadcast_action_frame(ServerFrame {
            event_seq,
            payload: Some(server_frame::Payload::ActionBroadcast(ActionBroadcast {
                room_id: self.room_id.clone(),
                match_id: self.match_id.clone(),
                round_id: self.round_id(),
                event_seq,
                actor_seat: seat as i32,
                action_kind: crate::proto::client::ActionKind::DeclareWin as i32,
                action_detail: Some(ActionDetail {
                    detail: Some(crate::proto::client::action_detail::Detail::DeclareWin(
                        crate::proto::client::DeclareWinAction {
                            win_type: win_type as i32,
                            winning_tile: winning_tile as i32,
                            source_seat: discarder_seat.unwrap_or(seat) as i32,
                            source_event_seq,
                        },
                    )),
                }),
                resulting_effects: Vec::new(),
                resulting_phase: self.phase as i32,
                next_turn_seat: seat as i32,
                wall_tiles_remaining: self.wall_tiles_remaining,
                action_deadline_unix_ms: 0,
            })),
        });
    }

    fn broadcast_action_frame(&self, frame: ServerFrame) {
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
                    "failed to broadcast ActionBroadcast"
                );
            }
        }
    }

    fn apply_score_deltas(&mut self, score_deltas: &[engine_proto::SeatScoreDelta]) {
        for score_delta in score_deltas {
            let Some(seat) = map_engine_seat_to_client(score_delta.seat) else {
                continue;
            };
            if let Some(player) = self.players_by_seat.get_mut(&seat) {
                player.score = score_delta.final_total;
            }
        }
    }

    fn mark_discard_claimed(&mut self, source_seat: Seat, source_event_seq: u64) {
        let Some(round_state) = self.player_round_state.get_mut(&source_seat) else {
            return;
        };

        if let Some(discard) = round_state
            .discards
            .iter_mut()
            .find(|discard| discard.source_event_seq == source_event_seq)
        {
            discard.claimed = true;
        }
    }

    fn broadcast_round_settlement(
        &mut self,
        winner_seat: Seat,
        discarder_seat: Option<Seat>,
        win_type: crate::proto::client::WinType,
        winning_tile: Tile,
        response: &engine_proto::CalculateScoreResponse,
    ) {
        let event_seq = self.allocate_event_seq();
        let player_results = response
            .score_delta_by_seat
            .iter()
            .filter_map(|score_delta| {
                let seat = map_engine_seat_to_client(score_delta.seat)?;
                Some(RoundPlayerResult {
                    seat: seat as i32,
                    round_delta: score_delta.delta,
                    total_score_after: score_delta.final_total,
                })
            })
            .collect();
        let fan_details = response
            .fan_details
            .iter()
            .map(|fan_detail| FanDetail {
                fan_code: fan_detail.fan_code.clone(),
                fan_name: fan_detail.fan_name.clone(),
                fan_value: fan_detail.fan_value,
                count: fan_detail.count,
                description: fan_detail.description.clone(),
            })
            .collect();
        let revealed_hands = SEAT_ORDER
            .into_iter()
            .filter_map(|seat| {
                let round_state = self.player_round_state.get(&seat)?;
                Some(RevealedHand {
                    seat: seat as i32,
                    concealed_tiles: merged_private_tiles(round_state)
                        .into_iter()
                        .map(|tile| tile as i32)
                        .collect(),
                    melds: round_state.melds.clone(),
                    flowers: round_state.flowers.iter().map(|tile| *tile as i32).collect(),
                    winner: seat == winner_seat,
                })
            })
            .collect();
        let settlement_flags = response
            .settlement_flags
            .iter()
            .filter_map(|flag| map_engine_settlement_flag_to_client(*flag))
            .map(|flag| flag as i32)
            .collect();
        self.persist_match_event(
            "round_settlement",
            event_seq,
            Some(winner_seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "event_seq": event_seq,
                "winner_seat": winner_seat.as_str_name(),
                "discarder_seat": discarder_seat.map(|seat| seat.as_str_name().to_owned()),
                "win_type": win_type.as_str_name(),
                "winning_tile": winning_tile.as_str_name(),
                "player_results": response.score_delta_by_seat.iter().filter_map(|delta| map_engine_seat_to_client(delta.seat).map(|seat| serde_json::json!({
                    "seat": seat.as_str_name(),
                    "round_delta": delta.delta,
                    "total_score_after": delta.final_total,
                }))).collect::<Vec<_>>(),
                "fan_details": response.fan_details.iter().map(|fan| serde_json::json!({
                    "fan_code": fan.fan_code,
                    "fan_name": fan.fan_name,
                    "fan_value": fan.fan_value,
                    "count": fan.count,
                    "description": fan.description,
                })).collect::<Vec<_>>(),
                "settlement_flags": response.settlement_flags.iter().filter_map(|flag| map_engine_settlement_flag_to_client(*flag).map(|flag| flag.as_str_name().to_owned())).collect::<Vec<_>>(),
                "wall_tiles_remaining": self.wall_tiles_remaining,
            }),
        );

        let frame = ServerFrame {
            event_seq,
            payload: Some(server_frame::Payload::RoundSettlement(RoundSettlement {
                room_id: self.room_id.clone(),
                match_id: self.match_id.clone(),
                round_id: self.round_id(),
                prevailing_wind: self.prevailing_wind as i32,
                hand_number: self.hand_number,
                dealer_seat: self.dealer_seat as i32,
                win_type: win_type as i32,
                winner_seat: winner_seat as i32,
                discarder_seat: discarder_seat.unwrap_or(Seat::Unspecified) as i32,
                winning_tile: winning_tile as i32,
                player_results,
                fan_details,
                revealed_hands,
                settlement_flags,
                wall_tiles_remaining: self.wall_tiles_remaining,
            })),
        };

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
                    "failed to broadcast RoundSettlement"
                );
            }
        }
    }

    fn broadcast_sync_state(&mut self, reason: SyncReason) {
        // SyncState 是兜底快照：当状态切换复杂时，直接用一次全量同步把客户端拉回权威视图。
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
        // 自己能看到完整暗手，他人只看到张数和公开信息；
        // 所有断线重连与状态矫正都依赖这条边界。
        let players = SEAT_ORDER
            .into_iter()
            .filter_map(|seat| {
                let player = self.players_by_seat.get(&seat)?;
                let round_state = self.player_round_state.get(&seat);
                Some((player, round_state))
            })
            .map(|(player, round_state)| PlayerState {
                user_id: player.user_id.clone(),
                display_name: player.display_name.clone(),
                seat: player.seat as i32,
                status: player.status as i32,
                score: player.score,
                is_dealer: player.seat == self.dealer_seat,
                is_connected: player.connected,
                auto_play_enabled: player.auto_play_enabled,
                concealed_tile_count: round_state
                    .map(private_tile_count)
                    .unwrap_or_default() as u32,
                melds: round_state
                    .map(|round_state| round_state.melds.clone())
                    .unwrap_or_default(),
                discards: round_state
                    .map(|round_state| round_state.discards.clone())
                    .unwrap_or_default(),
                flowers: round_state
                    .map(|round_state| {
                        round_state
                            .flowers
                            .iter()
                            .map(|tile| *tile as i32)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                replacement_draw_count: round_state
                    .map(|round_state| round_state.replacement_draw_count)
                    .unwrap_or_default(),
            })
            .collect();

        let self_hand = self
            .player_round_state
            .get(&recipient_seat)
            .map(|round_state| SelfHandState {
                concealed_tiles: round_state
                    .concealed_tiles
                    .iter()
                    .map(|tile| *tile as i32)
                    .collect(),
                drawn_tile: round_state
                    .drawn_tile
                    .map(|tile| tile as i32)
                    .unwrap_or(Tile::Unspecified as i32),
            })
            .unwrap_or(SelfHandState {
                concealed_tiles: Vec::new(),
                drawn_tile: Tile::Unspecified as i32,
            });

        let active_claim_window = self
            .round_runtime
            .as_ref()
            .and_then(|runtime| runtime.active_claim_window.as_ref())
            .map(|window| ClaimWindow {
                action_window_id: window.action_window_id,
                source_event_seq: window.source_event_seq,
                source_seat: window.source_seat as i32,
                target_tile: window.target_tile as i32,
                trigger_action_kind: window.trigger_action_kind as i32,
                eligible_seats: window
                    .eligible_seats
                    .iter()
                    .map(|seat| *seat as i32)
                    .collect(),
                options: if window.responded_seats.contains_key(&recipient_seat) {
                    Vec::new()
                } else {
                    window
                        .options_by_seat
                        .get(&recipient_seat)
                        .cloned()
                        .unwrap_or_default()
                },
                deadline_unix_ms: window.deadline_unix_ms,
            });

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
                    self_hand: Some(self_hand),
                    active_claim_window,
                    action_deadline_unix_ms: self
                        .round_runtime
                        .as_ref()
                        .and_then(|runtime| runtime.active_claim_window.as_ref())
                        .map(|window| window.deadline_unix_ms)
                        .unwrap_or(0),
                }),
            })),
        }
    }

    fn persist_match_event(
        &self,
        event_type: &str,
        event_seq: u64,
        actor_seat: Option<Seat>,
        event_payload: serde_json::Value,
    ) {
        let actor_user_id = actor_seat.and_then(|seat| {
            self.players_by_seat
                .get(&seat)
                .map(|player| player.user_id.clone())
        });

        self.match_event_writer.append(NewMatchEventRecord {
            match_id: self.match_id.clone(),
            round_id: Some(self.round_id()),
            event_seq: event_seq as i64,
            event_type: event_type.to_owned(),
            actor_user_id,
            actor_seat: actor_seat.map(|seat| seat.as_str_name().to_owned()),
            request_id: None,
            correlation_id: None,
            causation_event_seq: None,
            event_payload,
        });
    }

    fn persist_round_started_event(&self, event_seq: u64) {
        let players = SEAT_ORDER
            .into_iter()
            .filter_map(|seat| {
                let player = self.players_by_seat.get(&seat)?;
                let round_state = self.player_round_state.get(&seat)?;
                Some(serde_json::json!({
                    "seat": seat.as_str_name(),
                    "user_id": player.user_id,
                    "display_name": player.display_name,
                    "concealed_tiles": round_state.concealed_tiles.iter().map(|tile| tile.as_str_name()).collect::<Vec<_>>(),
                    "drawn_tile": round_state.drawn_tile.map(|tile| tile.as_str_name().to_owned()),
                    "melds": round_state.melds.iter().map(meld_json).collect::<Vec<_>>(),
                    "flowers": round_state.flowers.iter().map(|tile| tile.as_str_name()).collect::<Vec<_>>(),
                    "score": player.score,
                }))
            })
            .collect::<Vec<_>>();

        self.persist_match_event(
            "round_started",
            event_seq,
            Some(self.dealer_seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "event_seq": event_seq,
                "phase": self.phase.as_str_name(),
                "prevailing_wind": self.prevailing_wind.as_str_name(),
                "dealer_seat": self.dealer_seat.as_str_name(),
                "current_turn_seat": self.current_turn_seat.as_str_name(),
                "hand_number": self.hand_number,
                "dealer_streak": self.dealer_streak,
                "wall_tiles_remaining": self.wall_tiles_remaining,
                "dead_wall_tiles_remaining": self.dead_wall_tiles_remaining,
                "players": players,
            }),
        );
    }

    fn persist_claim_window_opened_event(&self, window: &ActiveClaimWindowState) {
        self.persist_match_event(
            "claim_window_opened",
            window.source_event_seq,
            Some(window.source_seat),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "action_window_id": window.action_window_id,
                "source_event_seq": window.source_event_seq,
                "source_seat": window.source_seat.as_str_name(),
                "target_tile": window.target_tile.as_str_name(),
                "trigger_action_kind": window.trigger_action_kind.as_str_name(),
                "eligible_seats": window.eligible_seats.iter().map(|seat| seat.as_str_name()).collect::<Vec<_>>(),
                "options_by_seat": window.options_by_seat.iter().map(|(seat, options)| serde_json::json!({
                    "seat": seat.as_str_name(),
                    "options": options.iter().map(prompt_option_json).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "deadline_unix_ms": window.deadline_unix_ms,
            }),
        );
    }

    fn persist_claim_window_resolved_event(
        &self,
        window: &ActiveClaimWindowState,
        responded_seats: &HashMap<Seat, ClaimResponse>,
        winner_seat: Option<Seat>,
        resolution_kind: &str,
    ) {
        self.persist_match_event(
            "claim_window_resolved",
            self.current_event_seq(),
            winner_seat.or(Some(window.source_seat)),
            serde_json::json!({
                "room_id": self.room_id,
                "match_id": self.match_id,
                "round_id": self.round_id(),
                "action_window_id": window.action_window_id,
                "source_event_seq": window.source_event_seq,
                "source_seat": window.source_seat.as_str_name(),
                "target_tile": window.target_tile.as_str_name(),
                "resolution_kind": resolution_kind,
                "winner_seat": winner_seat.map(|seat| seat.as_str_name().to_owned()),
                "responses": responded_seats.iter().map(|(seat, response)| serde_json::json!({
                    "seat": seat.as_str_name(),
                    "response": claim_response_json(response),
                })).collect::<Vec<_>>(),
            }),
        );
    }
}
