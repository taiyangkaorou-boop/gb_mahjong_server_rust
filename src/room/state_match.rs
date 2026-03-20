impl MatchRuntimeState {
    fn new() -> Self {
        // 整场统计默认给四个座位都预留位置，
        // 这样后面更新胜局数和最终排行时不需要再反复补 key。
        let player_stats = SEAT_ORDER
            .into_iter()
            .map(|seat| (seat, MatchPlayerStats::default()))
            .collect();

        Self {
            total_hands: MATCH_TOTAL_HANDS,
            completed_hands: 0,
            player_stats,
            last_round_winner: None,
            last_round_self_draw: false,
            last_match_settlement: None,
        }
    }
}

impl RoomState {
    fn schedule_post_round_transition(&self, settled_hand_number: u32) {
        // 结算后不立即推进下一手，而是走一条内部命令，
        // 这样客户端能先稳定消费 RoundSettlement，再进入下一手同步。
        if self.runtime_mode != RoomRuntimeMode::Production {
            return;
        }

        let command_tx = self.room_command_tx.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            let _ = command_tx
                .send(RoomCommand::AdvanceAfterRoundSettlement { settled_hand_number })
                .await;
        });
    }

    fn record_round_outcome(&mut self, winner_seat: Option<Seat>, self_draw: bool) {
        // 整场统计只记录每手的最终赢家和自摸标记；
        // 更细的过程仍以 match_events 事件流为权威来源。
        self.match_runtime.last_round_winner = winner_seat;
        self.match_runtime.last_round_self_draw = self_draw;
        self.match_runtime.last_match_settlement = None;

        if let Some(winner_seat) = winner_seat {
            let stats = self
                .match_runtime
                .player_stats
                .entry(winner_seat)
                .or_default();
            stats.rounds_won += 1;
            if self_draw {
                stats.self_draw_wins += 1;
            }
        }
    }

    fn settle_draw_game(&mut self) {
        // 流局不走算番引擎，直接生成 0 分变化的正式结算。
        if let Some(runtime) = self.round_runtime.as_mut() {
            runtime.active_claim_window = None;
        }
        self.phase = GamePhase::RoundSettlement;
        self.record_round_outcome(None, false);
        self.broadcast_draw_round_settlement();
        self.schedule_post_round_transition(self.hand_number);
    }

    async fn advance_after_round_settlement(&mut self, settled_hand_number: u32) {
        // 结算后推进命令必须校验“当前还是同一手的结算阶段”，
        // 避免旧命令越过时序边界污染新一手状态。
        if self.phase != GamePhase::RoundSettlement || self.hand_number != settled_hand_number {
            return;
        }

        let transition = self.build_round_transition(settled_hand_number);
        self.match_runtime.completed_hands = settled_hand_number;

        if transition.match_finished {
            self.phase = GamePhase::MatchSettlement;
            let summary = self.build_match_settlement_summary();
            self.broadcast_match_settlement(&summary);
            self.match_runtime.last_match_settlement = Some(summary);
            return;
        }

        self.prevailing_wind = transition.next_prevailing_wind;
        self.dealer_seat = transition.next_dealer_seat;
        self.current_turn_seat = transition.next_dealer_seat;
        self.hand_number = transition.next_hand_number;
        self.dealer_streak = transition.next_dealer_streak;
        self.match_runtime.last_round_winner = None;
        self.match_runtime.last_round_self_draw = false;

        if let Err(error) = self.start_round() {
            warn!(
                room_id = %self.room_id,
                hand_number = self.hand_number,
                ?error,
                "failed to start next round after settlement"
            );
            self.phase = GamePhase::RoundSettlement;
            return;
        }

        self.broadcast_sync_state(SyncReason::RoundStart);
    }

    fn build_round_transition(&self, settled_hand_number: u32) -> RoundTransition {
        // 当前版本先实现一个简化东风场：固定 4 手。
        // 这能先把“整场能跑完”的闭环做出来，再继续细化更完整的国标局数推进规则。
        let match_finished = settled_hand_number >= self.match_runtime.total_hands;
        let (next_dealer_seat, next_dealer_streak) = match self.match_runtime.last_round_winner {
            Some(winner) if winner == self.dealer_seat => (self.dealer_seat, self.dealer_streak + 1),
            Some(_) | None => (next_seat(self.dealer_seat), 0),
        };

        RoundTransition {
            next_prevailing_wind: Seat::East,
            next_dealer_seat,
            next_hand_number: settled_hand_number.saturating_add(1),
            next_dealer_streak,
            match_finished,
        }
    }

    fn build_match_settlement_summary(&mut self) -> MatchSettlementSummary {
        // 最终排名以当前玩家总分为主，若总分相同则按固定座位顺序稳定排序，
        // 避免重放时出现不稳定排名。
        let mut standings = SEAT_ORDER
            .into_iter()
            .filter_map(|seat| {
                let player = self.players_by_seat.get(&seat)?;
                let stats = self.match_runtime.player_stats.get(&seat).cloned().unwrap_or_default();
                Some((
                    seat,
                    FinalStanding {
                        seat: seat as i32,
                        user_id: player.user_id.clone(),
                        display_name: player.display_name.clone(),
                        final_score: player.score,
                        rank: 0,
                        rounds_won: stats.rounds_won,
                        self_draw_wins: stats.self_draw_wins,
                    },
                ))
            })
            .collect::<Vec<_>>();

        standings.sort_by(|(left_seat, left), (right_seat, right)| {
            right
                .final_score
                .cmp(&left.final_score)
                .then_with(|| seat_sort_key(*left_seat).cmp(&seat_sort_key(*right_seat)))
        });

        for (index, (_, standing)) in standings.iter_mut().enumerate() {
            standing.rank = index as u32 + 1;
        }

        MatchSettlementSummary {
            event_seq: self.allocate_event_seq(),
            standings: standings.into_iter().map(|(_, standing)| standing).collect(),
            finished_at_unix_ms: unix_time_ms(),
        }
    }
}

fn seat_sort_key(seat: Seat) -> usize {
    SEAT_ORDER
        .iter()
        .position(|candidate| *candidate == seat)
        .unwrap_or(SEAT_ORDER.len())
}
