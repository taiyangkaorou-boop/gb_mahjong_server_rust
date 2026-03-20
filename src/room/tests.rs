// room 模块测试单独拆文件，避免核心状态机实现与长测试交错。
// 房间模块测试集中放在独立文件里，
// 便于继续扩展回合流和抢操作场景，而不把主状态机文件继续拉长。
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::{sync::Notify, time::timeout};

    use super::*;
    use crate::{
        db::{MatchEventRepository, MatchEventWriter, MatchRecord, NewMatchEventRecord},
        engine::{MockRuleEngine, RuleEngineHandle},
        proto::{
            client::{player_action_request, server_frame, DeclareWinAction, PassAction},
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

    fn deterministic_wall_factory() -> WallFactory {
        Arc::new(|_, _, _| build_standard_wall(false))
    }

    fn insert_test_player(
        state: &mut RoomState,
        seat: Seat,
        user_id: &str,
        display_name: &str,
    ) -> mpsc::UnboundedReceiver<ServerFrame> {
        let connection_id = format!("conn-{}", seat.as_str_name().to_ascii_lowercase());
        let (connection, outbound_rx) = test_connection(&connection_id);

        state.players_by_seat.insert(
            seat,
            PlayerSlot {
                user_id: user_id.to_owned(),
                display_name: display_name.to_owned(),
                seat,
                status: PlayerStatus::Ready,
                score: INITIAL_SCORE,
                connected: true,
                auto_play_enabled: false,
                connection_id: Some(connection.connection_id().to_owned()),
                outbound_tx: Some(connection.outbound_tx.clone()),
                resume_token: format!("resume-{user_id}"),
            },
        );
        state.player_round_state.insert(seat, PlayerRoundState::default());
        state.user_to_seat.insert(user_id.to_owned(), seat);
        state
            .connection_to_user
            .insert(connection.connection_id().to_owned(), user_id.to_owned());

        outbound_rx
    }

    fn authorized_join(user_id: &str, display_name: &str, seat: Seat) -> AuthorizedJoin {
        AuthorizedJoin {
            user_id: user_id.to_owned(),
            display_name: display_name.to_owned(),
            seat,
        }
    }

    #[derive(Default)]
    struct RecordingMatchEventRepository {
        // 这个测试仓储专门用来观测房间状态机真正尝试写了哪些事件。
        events: Mutex<Vec<NewMatchEventRecord>>,
        notify: Notify,
    }

    impl RecordingMatchEventRepository {
        async fn wait_for_event_count(&self, expected_len: usize) -> Vec<NewMatchEventRecord> {
            loop {
                if let Some(events) = self.snapshot_if_len(expected_len) {
                    return events;
                }

                timeout(Duration::from_secs(1), self.notify.notified())
                    .await
                    .expect("persisted events should arrive in time");
            }
        }

        fn snapshot_if_len(&self, expected_len: usize) -> Option<Vec<NewMatchEventRecord>> {
            let events = self.events.lock().expect("events lock should stay healthy");
            (events.len() >= expected_len).then(|| events.clone())
        }
    }

    #[async_trait]
    impl MatchEventRepository for RecordingMatchEventRepository {
        async fn append_event(&self, event: NewMatchEventRecord) -> anyhow::Result<()> {
            self.events
                .lock()
                .expect("events lock should stay healthy")
                .push(event);
            self.notify.notify_waiters();
            Ok(())
        }

        async fn list_events(&self, _match_id: &str) -> anyhow::Result<Vec<crate::db::MatchEventRecord>> {
            Ok(Vec::new())
        }

        async fn get_match(&self, _match_id: &str) -> anyhow::Result<Option<MatchRecord>> {
            Ok(None)
        }
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
                authorized_join("user-alpha", "Alice", Seat::East),
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
                authorized_join("user-beta", "Bob", Seat::East),
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
                authorized_join("user-gamma", "Carol", Seat::East),
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
                    action: Some(player_action_request::Action::SupplementalKong(
                        crate::proto::client::SupplementalKongAction {
                            tile: Tile::Character1 as i32,
                        },
                    )),
                },
            )
            .await;

        let rejected = recv_frame(&mut outbound_rx).await;

        match rejected.payload {
            Some(server_frame::Payload::ActionRejected(rejected)) => {
                assert_eq!(rejected.request_id, "req-action");
                assert_eq!(rejected.reject_code, RejectCode::PhaseMismatch as i32);
                assert_eq!(rejected.expected_event_seq, 3);
                assert!(rejected.message.contains("WaitingDiscard"));
            }
            other => panic!("expected ActionRejected, got {other:?}"),
        }

        let validate_requests = mock_rule_engine.validate_requests();
        assert!(validate_requests.is_empty());
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
                authorized_join("user-delta", "Delta", Seat::East),
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
                    action: Some(player_action_request::Action::DeclareWin(
                        DeclareWinAction {
                            win_type: crate::proto::client::WinType::Discard as i32,
                            winning_tile: Tile::Character1 as i32,
                            source_seat: Seat::South as i32,
                            source_event_seq: 41,
                        },
                    )),
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

    #[tokio::test]
    async fn start_round_deals_fourteen_to_dealer_and_thirteen_to_others() {
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-epsilon".to_owned(),
            RuleEngineHandle::default(),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");

        state.start_round().expect("round should start successfully");

        assert_eq!(state.phase, GamePhase::WaitingDiscard);
        assert_eq!(state.current_turn_seat, Seat::East);
        assert!(state
            .round_runtime
            .as_ref()
            .expect("runtime should exist")
            .active_claim_window
            .is_none());
        assert_eq!(
            private_tile_count(
                state
                    .player_round_state
                    .get(&Seat::East)
                    .expect("dealer round state should exist"),
            ),
            14
        );
        assert_eq!(
            private_tile_count(
                state
                    .player_round_state
                    .get(&Seat::South)
                    .expect("south round state should exist"),
            ),
            13
        );
        assert_eq!(
            private_tile_count(
                state
                    .player_round_state
                    .get(&Seat::West)
                    .expect("west round state should exist"),
            ),
            13
        );
        assert_eq!(
            private_tile_count(
                state
                    .player_round_state
                    .get(&Seat::North)
                    .expect("north round state should exist"),
            ),
            13
        );

        let sync_frame =
            state.build_sync_state_for(Seat::East, state.current_event_seq(), SyncReason::RoundStart);
        match sync_frame.payload {
            Some(server_frame::Payload::SyncState(sync)) => {
                let snapshot = sync.snapshot.expect("snapshot should be present");
                let self_hand = snapshot.self_hand.expect("self hand should be present");
                assert_eq!(snapshot.phase, GamePhase::WaitingDiscard as i32);
                assert_eq!(snapshot.players.len(), 4);
                assert_eq!(self_hand.concealed_tiles.len(), 13);
                assert_ne!(self_hand.drawn_tile, Tile::Unspecified as i32);

                let south_public = snapshot
                    .players
                    .iter()
                    .find(|player| player.seat == Seat::South as i32)
                    .expect("south public state should exist");
                assert_eq!(south_public.concealed_tile_count, 13);
            }
            other => panic!("expected SyncState, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pass_on_active_claim_window_closes_window_and_advances_turn() {
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-zeta".to_owned(),
            RuleEngineHandle::default(),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");

        state.start_round().expect("round should start successfully");
        let source_event_seq = state.allocate_event_seq();
        state.phase = GamePhase::WaitingClaim;
        state.record_last_action(ResolvedActionRecord {
            event_seq: source_event_seq,
            actor_seat: Seat::East,
            action_kind: crate::proto::client::ActionKind::Discard,
            tile: Some(Tile::Character1),
            replacement_draw: false,
            rob_kong_candidate: false,
        });
        state
            .player_round_state
            .get_mut(&Seat::East)
            .expect("east round state should exist")
            .drawn_tile = None;
        state
            .round_runtime
            .as_mut()
            .expect("runtime should exist")
            .active_claim_window = Some(ActiveClaimWindowState {
            action_window_id: 7,
            source_event_seq,
            source_seat: Seat::East,
            target_tile: Tile::Character1,
            trigger_action_kind: crate::proto::client::ActionKind::Discard,
            eligible_seats: vec![Seat::South],
            options_by_seat: HashMap::from([(
                Seat::South,
                vec![PromptOption {
                    option_id: "pass-only".to_owned(),
                    action_kind: crate::proto::client::ActionKind::Pass as i32,
                    claim_kind: crate::proto::client::ClaimKind::Unspecified as i32,
                    win_type: crate::proto::client::WinType::Unspecified as i32,
                    candidate_tiles: vec![Tile::Character1 as i32],
                    consume_tiles: Vec::new(),
                }],
            )]),
            responded_seats: HashMap::new(),
            deadline_unix_ms: unix_time_ms() + 1_000,
        });

        let request = PlayerActionRequest {
            room_id: "room-zeta".to_owned(),
            match_id: "match-room-zeta".to_owned(),
            round_id: "hand-1".to_owned(),
            expected_event_seq: state.current_event_seq(),
            action_window_id: 7,
            action: Some(player_action_request::Action::Pass(PassAction {
                source_event_seq,
            })),
        };
        let pass = PassAction { source_event_seq };

        state.handle_pass_action(
            "req-pass".to_owned(),
            "conn-seat_south".to_owned(),
            Seat::South,
            &request,
            &pass,
        )
        .await;

        let runtime = state.round_runtime.as_ref().expect("runtime should exist");
        assert!(runtime.active_claim_window.is_none());
        assert_eq!(state.current_turn_seat, Seat::South);
        assert_eq!(state.phase, GamePhase::WaitingDiscard);
        assert!(state
            .player_round_state
            .get(&Seat::South)
            .expect("south round state should exist")
            .drawn_tile
            .is_some());
        let last_action = runtime
            .last_resolved_action
            .as_ref()
            .expect("last action should be recorded");
        assert_eq!(last_action.actor_seat, Seat::South);
        assert_eq!(
            last_action.action_kind,
            crate::proto::client::ActionKind::Draw
        );
    }

    #[tokio::test]
    async fn self_draw_win_calls_calculate_score_and_broadcasts_round_settlement() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: true,
            reject_code: engine_proto::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine_proto::ActionKind::DeclareWin as i32,
            meld_result: None,
            winning_context: Some(engine_proto::WinningContext {
                win_type: engine_proto::WinType::SelfDraw as i32,
                winning_tile: engine_proto::Tile::Character5 as i32,
                completes_standard_hand: true,
                completes_seven_pairs: false,
                completes_thirteen_orphans: false,
                tentative_fan_details: Vec::new(),
                tentative_total_fan: 8,
            }),
            suggested_follow_ups: Vec::new(),
            explanation: "self draw win accepted".to_owned(),
        })
        .with_calculate_response(engine_proto::CalculateScoreResponse {
            total_fan: 8,
            fan_details: vec![engine_proto::FanDetail {
                fan_code: "MEN_QIAN_QING_ZIMO".to_owned(),
                fan_name: "门前清自摸和".to_owned(),
                fan_value: 8,
                count: 1,
                description: "测试番型".to_owned(),
            }],
            score_delta_by_seat: vec![
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::East as i32,
                    delta: 24,
                    final_total: INITIAL_SCORE + 24,
                },
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::South as i32,
                    delta: -8,
                    final_total: INITIAL_SCORE - 8,
                },
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::West as i32,
                    delta: -8,
                    final_total: INITIAL_SCORE - 8,
                },
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::North as i32,
                    delta: -8,
                    final_total: INITIAL_SCORE - 8,
                },
            ],
            settlement_flags: vec![engine_proto::SettlementFlag::SelfDraw as i32],
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-eta".to_owned(),
            RuleEngineHandle::new(mock_rule_engine.clone()),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let mut east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");

        state.start_round().expect("round should start successfully");
        let winning_tile = state
            .player_round_state
            .get(&Seat::East)
            .and_then(|round_state| round_state.drawn_tile)
            .expect("dealer should have a pending drawn tile");
        let source_event_seq = state
            .round_runtime
            .as_ref()
            .and_then(|runtime| runtime.last_resolved_action.as_ref())
            .map(|action| action.event_seq)
            .expect("dealer draw should be recorded");

        state
            .handle_player_action(
                "req-win".to_owned(),
                "conn-seat_east".to_owned(),
                PlayerActionRequest {
                    room_id: "room-eta".to_owned(),
                    match_id: "match-room-eta".to_owned(),
                    round_id: "hand-1".to_owned(),
                    expected_event_seq: state.current_event_seq(),
                    action_window_id: 0,
                    action: Some(player_action_request::Action::DeclareWin(
                        DeclareWinAction {
                            win_type: crate::proto::client::WinType::SelfDraw as i32,
                            winning_tile: winning_tile as i32,
                            source_seat: Seat::East as i32,
                            source_event_seq,
                        },
                    )),
                },
            )
            .await;

        let mut settlement_seen = None;
        for _ in 0..6 {
            let frame = recv_frame(&mut east_rx).await;
            if let Some(server_frame::Payload::RoundSettlement(settlement)) = frame.payload {
                settlement_seen = Some(settlement);
                break;
            }
        }
        let settlement = settlement_seen.expect("round settlement should be broadcast");
        assert_eq!(settlement.room_id, "room-eta");
        assert_eq!(settlement.match_id, "match-room-eta");
        assert_eq!(settlement.win_type, crate::proto::client::WinType::SelfDraw as i32);
        assert_eq!(settlement.winner_seat, Seat::East as i32);
        assert_eq!(settlement.discarder_seat, Seat::Unspecified as i32);
        assert_eq!(settlement.winning_tile, winning_tile as i32);
        assert_eq!(settlement.player_results.len(), 4);
        assert_eq!(settlement.fan_details.len(), 1);
        assert_eq!(settlement.fan_details[0].fan_name, "门前清自摸和");

        assert_eq!(state.phase, GamePhase::RoundSettlement);
        assert_eq!(
            state
                .players_by_seat
                .get(&Seat::East)
                .expect("east player should exist")
                .score,
            INITIAL_SCORE + 24
        );

        let calculate_requests = mock_rule_engine.calculate_requests();
        assert_eq!(calculate_requests.len(), 1);
        let calculate_request = &calculate_requests[0];
        assert_eq!(calculate_request.winner_seat, engine_proto::Seat::East as i32);
        assert_eq!(
            calculate_request.discarder_seat,
            engine_proto::Seat::Unspecified as i32
        );
        assert_eq!(calculate_request.winning_tile, map_tile_to_engine(winning_tile) as i32);
        assert_eq!(calculate_request.hands.len(), 4);
    }

    #[tokio::test]
    async fn supplemental_kong_without_rob_kong_upgrades_meld_and_draws_replacement() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: true,
            reject_code: engine_proto::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine_proto::ActionKind::SupplementalKong as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "supplemental kong accepted".to_owned(),
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-lambda".to_owned(),
            RuleEngineHandle::new(mock_rule_engine.clone()),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let mut east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");
        state.room_config.enable_robbing_kong = false;
        state.start_round().expect("round should start successfully");
        state.player_round_state.get_mut(&Seat::East).unwrap().melds = vec![Meld {
            kind: crate::proto::client::MeldKind::Peng as i32,
            tiles: vec![
                Tile::Character5 as i32,
                Tile::Character5 as i32,
                Tile::Character5 as i32,
            ],
            from_seat: Seat::South as i32,
            claimed_tile: Tile::Character5 as i32,
            concealed: false,
            source_event_seq: 12,
        }];
        state.player_round_state.get_mut(&Seat::East).unwrap().drawn_tile = Some(Tile::Character5);

        state
            .handle_player_action(
                "req-supplemental-kong".to_owned(),
                "conn-seat_east".to_owned(),
                PlayerActionRequest {
                    room_id: "room-lambda".to_owned(),
                    match_id: "match-room-lambda".to_owned(),
                    round_id: "hand-1".to_owned(),
                    expected_event_seq: state.current_event_seq(),
                    action_window_id: 0,
                    action: Some(player_action_request::Action::SupplementalKong(
                        crate::proto::client::SupplementalKongAction {
                            tile: Tile::Character5 as i32,
                        },
                    )),
                },
            )
            .await;

        let east_round_state = state
            .player_round_state
            .get(&Seat::East)
            .expect("east round state should exist");
        assert_eq!(state.phase, GamePhase::WaitingDiscard);
        assert_eq!(state.current_turn_seat, Seat::East);
        assert_eq!(east_round_state.melds.len(), 1);
        assert_eq!(
            east_round_state.melds[0].kind,
            crate::proto::client::MeldKind::SupplementalKong as i32
        );
        assert_eq!(east_round_state.melds[0].tiles.len(), 4);
        assert!(east_round_state.drawn_tile.is_some());

        let validate_requests = mock_rule_engine.validate_requests();
        assert_eq!(validate_requests.len(), 1);

        let mut saw_kong_broadcast = false;
        let mut saw_draw_broadcast = false;
        for _ in 0..6 {
            let frame = recv_frame(&mut east_rx).await;
            if let Some(server_frame::Payload::ActionBroadcast(broadcast)) = frame.payload {
                if broadcast.action_kind
                    == crate::proto::client::ActionKind::SupplementalKong as i32
                {
                    saw_kong_broadcast = true;
                }
                if broadcast.action_kind == crate::proto::client::ActionKind::Draw as i32 {
                    saw_draw_broadcast = true;
                }
            }
            if saw_kong_broadcast && saw_draw_broadcast {
                break;
            }
        }

        assert!(saw_kong_broadcast);
        assert!(saw_draw_broadcast);
    }

    #[tokio::test]
    async fn rob_kong_win_claim_window_blocks_supplemental_kong_until_resolved() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: true,
            reject_code: engine_proto::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine_proto::ActionKind::DeclareWin as i32,
            meld_result: None,
            winning_context: Some(engine_proto::WinningContext {
                win_type: engine_proto::WinType::RobKong as i32,
                winning_tile: engine_proto::Tile::Character5 as i32,
                completes_standard_hand: true,
                completes_seven_pairs: false,
                completes_thirteen_orphans: false,
                tentative_fan_details: Vec::new(),
                tentative_total_fan: 8,
            }),
            suggested_follow_ups: Vec::new(),
            explanation: "rob kong accepted".to_owned(),
        })
        .with_calculate_response(engine_proto::CalculateScoreResponse {
            total_fan: 8,
            fan_details: vec![engine_proto::FanDetail {
                fan_code: "QIANG_GANG_HU".to_owned(),
                fan_name: "抢杠和".to_owned(),
                fan_value: 8,
                count: 1,
                description: "测试抢杠胡".to_owned(),
            }],
            score_delta_by_seat: vec![
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::South as i32,
                    delta: 8,
                    final_total: INITIAL_SCORE + 8,
                },
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::East as i32,
                    delta: -8,
                    final_total: INITIAL_SCORE - 8,
                },
            ],
            settlement_flags: vec![engine_proto::SettlementFlag::RobKongWin as i32],
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-mu".to_owned(),
            RuleEngineHandle::new(mock_rule_engine.clone()),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let mut east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");
        state.start_round().expect("round should start successfully");
        state.player_round_state.get_mut(&Seat::East).unwrap().melds = vec![Meld {
            kind: crate::proto::client::MeldKind::Peng as i32,
            tiles: vec![
                Tile::Character5 as i32,
                Tile::Character5 as i32,
                Tile::Character5 as i32,
            ],
            from_seat: Seat::South as i32,
            claimed_tile: Tile::Character5 as i32,
            concealed: false,
            source_event_seq: 12,
        }];
        state.player_round_state.get_mut(&Seat::East).unwrap().drawn_tile = Some(Tile::Character5);

        state
            .handle_player_action(
                "req-supplemental-kong".to_owned(),
                "conn-seat_east".to_owned(),
                PlayerActionRequest {
                    room_id: "room-mu".to_owned(),
                    match_id: "match-room-mu".to_owned(),
                    round_id: "hand-1".to_owned(),
                    expected_event_seq: state.current_event_seq(),
                    action_window_id: 0,
                    action: Some(player_action_request::Action::SupplementalKong(
                        crate::proto::client::SupplementalKongAction {
                            tile: Tile::Character5 as i32,
                        },
                    )),
                },
            )
            .await;

        let window = state
            .round_runtime
            .as_ref()
            .and_then(|runtime| runtime.active_claim_window.as_ref())
            .cloned()
            .expect("rob-kong claim window should be open");
        assert_eq!(state.phase, GamePhase::ResolvingKong);
        assert_eq!(
            window.trigger_action_kind,
            crate::proto::client::ActionKind::SupplementalKong
        );

        let south_win_request = PlayerActionRequest {
            room_id: "room-mu".to_owned(),
            match_id: "match-room-mu".to_owned(),
            round_id: "hand-1".to_owned(),
            expected_event_seq: state.current_event_seq(),
            action_window_id: window.action_window_id,
            action: Some(player_action_request::Action::DeclareWin(DeclareWinAction {
                win_type: crate::proto::client::WinType::RobKong as i32,
                winning_tile: Tile::Character5 as i32,
                source_seat: Seat::East as i32,
                source_event_seq: window.source_event_seq,
            })),
        };
        state
            .handle_declare_win_action(
                "req-south-win".to_owned(),
                "conn-seat_south".to_owned(),
                Seat::South,
                &south_win_request,
                south_win_request
                    .action
                    .as_ref()
                    .and_then(|action| match action {
                        player_action_request::Action::DeclareWin(declare_win) => Some(declare_win),
                        _ => None,
                    })
                    .expect("south win should be present"),
            )
            .await;

        for seat in [Seat::West, Seat::North] {
            let pass_request = PlayerActionRequest {
                room_id: "room-mu".to_owned(),
                match_id: "match-room-mu".to_owned(),
                round_id: "hand-1".to_owned(),
                expected_event_seq: state.current_event_seq(),
                action_window_id: window.action_window_id,
                action: Some(player_action_request::Action::Pass(PassAction {
                    source_event_seq: window.source_event_seq,
                })),
            };
            state
                .handle_pass_action(
                    format!("req-pass-{}", seat.as_str_name()),
                    format!("conn-{}", seat.as_str_name().to_ascii_lowercase()),
                    seat,
                    &pass_request,
                    pass_request
                        .action
                        .as_ref()
                        .and_then(|action| match action {
                            player_action_request::Action::Pass(pass) => Some(pass),
                            _ => None,
                        })
                        .expect("pass should be present"),
                )
                .await;
        }

        assert_eq!(state.phase, GamePhase::RoundSettlement);
        assert_eq!(
            state.players_by_seat.get(&Seat::South).unwrap().score,
            INITIAL_SCORE + 8
        );
        assert_eq!(
            state.player_round_state.get(&Seat::East).unwrap().melds[0].kind,
            crate::proto::client::MeldKind::Peng as i32
        );

        let calculate_requests = mock_rule_engine.calculate_requests();
        assert_eq!(calculate_requests.len(), 1);
        assert_eq!(calculate_requests[0].winner_seat, engine_proto::Seat::South as i32);

        let mut settlement_seen = false;
        for _ in 0..8 {
            let frame = recv_frame(&mut east_rx).await;
            if let Some(server_frame::Payload::RoundSettlement(settlement)) = frame.payload {
                assert_eq!(settlement.win_type, crate::proto::client::WinType::RobKong as i32);
                settlement_seen = true;
                break;
            }
        }
        assert!(settlement_seen);
    }

    #[tokio::test]
    async fn discard_claim_window_generates_chi_and_peng_by_seat() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: false,
            reject_code: engine_proto::ValidationRejectCode::HandNotComplete as i32,
            derived_action_type: engine_proto::ActionKind::Unspecified as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "not a winning hand".to_owned(),
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-theta".to_owned(),
            RuleEngineHandle::new(mock_rule_engine),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");
        state.round_runtime = Some(RoundRuntimeState {
            live_wall: VecDeque::new(),
            dead_wall: VecDeque::new(),
            active_claim_window: None,
            last_resolved_action: Some(ResolvedActionRecord {
                event_seq: 98,
                actor_seat: Seat::East,
                action_kind: crate::proto::client::ActionKind::Discard,
                tile: Some(Tile::Character5),
                replacement_draw: false,
                rob_kong_candidate: false,
            }),
            turn_index: 1,
            replacement_draw_pending: false,
        });
        state.phase = GamePhase::WaitingDiscard;
        state.player_round_state.get_mut(&Seat::South).unwrap().concealed_tiles = vec![
            Tile::Character4,
            Tile::Character6,
            Tile::Character5,
            Tile::Character5,
        ];
        state.player_round_state.get_mut(&Seat::West).unwrap().concealed_tiles =
            vec![Tile::Character5, Tile::Character5];

        let opened = state
            .open_discard_claim_window(Seat::East, Tile::Character5, 99)
            .await
            .expect("claim window should open");

        assert!(opened);
        let runtime = state.round_runtime.as_ref().expect("runtime should exist");
        let window = runtime
            .active_claim_window
            .as_ref()
            .expect("claim window should exist");
        let south_options = window
            .options_by_seat
            .get(&Seat::South)
            .expect("south options should exist");
        let west_options = window
            .options_by_seat
            .get(&Seat::West)
            .expect("west options should exist");

        assert!(south_options.iter().any(|option| {
            option.claim_kind == crate::proto::client::ClaimKind::Chi as i32
                && option.consume_tiles == vec![Tile::Character4 as i32, Tile::Character6 as i32]
        }));
        assert!(south_options.iter().any(|option| {
            option.claim_kind == crate::proto::client::ClaimKind::Peng as i32
        }));
        assert!(!west_options.iter().any(|option| {
            option.claim_kind == crate::proto::client::ClaimKind::Chi as i32
        }));
        assert!(west_options.iter().any(|option| {
            option.claim_kind == crate::proto::client::ClaimKind::Peng as i32
        }));
    }

    #[tokio::test]
    async fn claim_window_opened_event_uses_distinct_persistence_sequence() {
        let repo = Arc::new(RecordingMatchEventRepository::default());
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: false,
            reject_code: engine_proto::ValidationRejectCode::HandNotComplete as i32,
            derived_action_type: engine_proto::ActionKind::Unspecified as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "not a winning hand".to_owned(),
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-claim-open-persist".to_owned(),
            RuleEngineHandle::new(mock_rule_engine),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::from_repository(repo.clone()),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");
        state.round_runtime = Some(RoundRuntimeState {
            live_wall: VecDeque::new(),
            dead_wall: VecDeque::new(),
            active_claim_window: None,
            last_resolved_action: Some(ResolvedActionRecord {
                event_seq: 98,
                actor_seat: Seat::East,
                action_kind: crate::proto::client::ActionKind::Discard,
                tile: Some(Tile::Character5),
                replacement_draw: false,
                rob_kong_candidate: false,
            }),
            turn_index: 1,
            replacement_draw_pending: false,
        });
        state.phase = GamePhase::WaitingDiscard;
        state.next_event_seq = 100;
        state.player_round_state.get_mut(&Seat::South).unwrap().concealed_tiles =
            vec![Tile::Character5, Tile::Character5];

        let opened = state
            .open_discard_claim_window(Seat::East, Tile::Character5, 99)
            .await
            .expect("claim window should open");

        assert!(opened);
        let events = repo.wait_for_event_count(1).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "claim_window_opened");
        assert_eq!(events[0].event_seq, 100);
        assert_ne!(events[0].event_seq, 99);
        assert_eq!(events[0].event_payload["event_seq"].as_u64(), Some(100));
        assert_eq!(events[0].event_payload["source_event_seq"].as_u64(), Some(99));
    }

    #[tokio::test]
    async fn claim_window_resolved_event_keeps_total_order_before_follow_up_draw() {
        let repo = Arc::new(RecordingMatchEventRepository::default());
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: false,
            reject_code: engine_proto::ValidationRejectCode::HandNotComplete as i32,
            derived_action_type: engine_proto::ActionKind::Unspecified as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "not a winning hand".to_owned(),
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-claim-resolve-persist".to_owned(),
            RuleEngineHandle::new(mock_rule_engine),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::from_repository(repo.clone()),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");
        state.round_runtime = Some(RoundRuntimeState {
            live_wall: VecDeque::from(vec![Tile::Bamboo1]),
            dead_wall: VecDeque::new(),
            active_claim_window: None,
            last_resolved_action: Some(ResolvedActionRecord {
                event_seq: 98,
                actor_seat: Seat::East,
                action_kind: crate::proto::client::ActionKind::Discard,
                tile: Some(Tile::Character5),
                replacement_draw: false,
                rob_kong_candidate: false,
            }),
            turn_index: 1,
            replacement_draw_pending: false,
        });
        state.phase = GamePhase::WaitingDiscard;
        state.current_turn_seat = Seat::East;
        state.next_event_seq = 100;
        state.player_round_state.get_mut(&Seat::South).unwrap().concealed_tiles =
            vec![Tile::Character5, Tile::Character5];

        let opened = state
            .open_discard_claim_window(Seat::East, Tile::Character5, 99)
            .await
            .expect("claim window should open");
        assert!(opened);

        let request = PlayerActionRequest {
            room_id: "room-claim-resolve-persist".to_owned(),
            match_id: "match-room-claim-resolve-persist".to_owned(),
            round_id: "hand-1".to_owned(),
            expected_event_seq: state.current_event_seq(),
            action_window_id: 1,
            action: Some(player_action_request::Action::Pass(PassAction {
                source_event_seq: 99,
            })),
        };
        let pass = match request.action.as_ref().expect("pass action should exist") {
            player_action_request::Action::Pass(pass) => pass,
            other => panic!("expected pass action, got {other:?}"),
        };
        state
            .handle_pass_action(
                "req-pass-south".to_owned(),
                "conn-seat_south".to_owned(),
                Seat::South,
                &request,
                pass,
            )
            .await;

        let events = repo.wait_for_event_count(3).await;
        let event_kinds = events
            .iter()
            .map(|event| (event.event_type.as_str(), event.event_seq))
            .collect::<Vec<_>>();
        assert_eq!(
            event_kinds,
            vec![
                ("claim_window_opened", 100),
                ("claim_window_resolved", 102),
                ("action_broadcast", 103),
            ]
        );
        assert_eq!(
            events[1].event_payload["resolution_kind"].as_str(),
            Some("PASS")
        );
        assert!(events[1].event_payload["winner_seat"].is_null());
        assert_eq!(events[1].event_payload["source_event_seq"].as_u64(), Some(99));
        assert_eq!(events[2].event_payload["action_kind"].as_str(), Some("ACTION_KIND_DRAW"));
    }

    #[tokio::test]
    async fn claim_resolution_prefers_peng_over_chi() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: true,
            reject_code: engine_proto::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine_proto::ActionKind::Peng as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "claim accepted".to_owned(),
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-iota".to_owned(),
            RuleEngineHandle::new(mock_rule_engine),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");
        state.round_runtime = Some(RoundRuntimeState {
            live_wall: VecDeque::new(),
            dead_wall: VecDeque::new(),
            active_claim_window: None,
            last_resolved_action: Some(ResolvedActionRecord {
                event_seq: 98,
                actor_seat: Seat::East,
                action_kind: crate::proto::client::ActionKind::Discard,
                tile: Some(Tile::Character5),
                replacement_draw: false,
                rob_kong_candidate: false,
            }),
            turn_index: 1,
            replacement_draw_pending: false,
        });
        state.phase = GamePhase::WaitingDiscard;
        state.player_round_state.get_mut(&Seat::East).unwrap().discards.push(
            crate::proto::client::DiscardTile {
                tile: Tile::Character5 as i32,
                turn_index: 1,
                claimed: false,
                drawn_and_discarded: false,
                source_event_seq: 99,
            },
        );
        state.player_round_state.get_mut(&Seat::South).unwrap().concealed_tiles =
            vec![Tile::Character4, Tile::Character6];
        state.player_round_state.get_mut(&Seat::West).unwrap().concealed_tiles =
            vec![Tile::Character5, Tile::Character5];
        state.phase = GamePhase::WaitingClaim;
        state
            .round_runtime
            .as_mut()
            .expect("runtime should exist")
            .active_claim_window = Some(ActiveClaimWindowState {
            action_window_id: 1,
            source_event_seq: 99,
            source_seat: Seat::East,
            target_tile: Tile::Character5,
            trigger_action_kind: crate::proto::client::ActionKind::Discard,
            eligible_seats: vec![Seat::South, Seat::West],
            options_by_seat: HashMap::from([
                (
                    Seat::South,
                    vec![PromptOption {
                        option_id: "south-chi".to_owned(),
                        action_kind: crate::proto::client::ActionKind::Chi as i32,
                        claim_kind: crate::proto::client::ClaimKind::Chi as i32,
                        win_type: crate::proto::client::WinType::Unspecified as i32,
                        candidate_tiles: vec![
                            Tile::Character4 as i32,
                            Tile::Character5 as i32,
                            Tile::Character6 as i32,
                        ],
                        consume_tiles: vec![Tile::Character4 as i32, Tile::Character6 as i32],
                    }],
                ),
                (
                    Seat::West,
                    vec![PromptOption {
                        option_id: "west-peng".to_owned(),
                        action_kind: crate::proto::client::ActionKind::Peng as i32,
                        claim_kind: crate::proto::client::ClaimKind::Peng as i32,
                        win_type: crate::proto::client::WinType::Unspecified as i32,
                        candidate_tiles: vec![Tile::Character5 as i32],
                        consume_tiles: vec![Tile::Character5 as i32, Tile::Character5 as i32],
                    }],
                ),
            ]),
            responded_seats: HashMap::new(),
            deadline_unix_ms: unix_time_ms() + 1_000,
        });

        let south_request = PlayerActionRequest {
            room_id: "room-iota".to_owned(),
            match_id: "match-room-iota".to_owned(),
            round_id: "hand-1".to_owned(),
            expected_event_seq: state.current_event_seq(),
            action_window_id: 1,
            action: Some(player_action_request::Action::Claim(crate::proto::client::ClaimAction {
                claim_kind: crate::proto::client::ClaimKind::Chi as i32,
                target_tile: Tile::Character5 as i32,
                consume_tiles: vec![Tile::Character4 as i32, Tile::Character6 as i32],
                source_seat: Seat::East as i32,
                source_event_seq: 99,
            })),
        };
        state
            .handle_claim_action(
                "req-south-chi".to_owned(),
                "conn-seat_south".to_owned(),
                Seat::South,
                &south_request,
                south_request
                    .action
                    .as_ref()
                    .and_then(|action| match action {
                        player_action_request::Action::Claim(claim) => Some(claim),
                        _ => None,
                    })
                    .expect("south claim should be present"),
            )
            .await;

        let west_request = PlayerActionRequest {
            room_id: "room-iota".to_owned(),
            match_id: "match-room-iota".to_owned(),
            round_id: "hand-1".to_owned(),
            expected_event_seq: state.current_event_seq(),
            action_window_id: 1,
            action: Some(player_action_request::Action::Claim(crate::proto::client::ClaimAction {
                claim_kind: crate::proto::client::ClaimKind::Peng as i32,
                target_tile: Tile::Character5 as i32,
                consume_tiles: vec![Tile::Character5 as i32, Tile::Character5 as i32],
                source_seat: Seat::East as i32,
                source_event_seq: 99,
            })),
        };
        state
            .handle_claim_action(
                "req-west-peng".to_owned(),
                "conn-seat_west".to_owned(),
                Seat::West,
                &west_request,
                west_request
                    .action
                    .as_ref()
                    .and_then(|action| match action {
                        player_action_request::Action::Claim(claim) => Some(claim),
                        _ => None,
                    })
                    .expect("west claim should be present"),
            )
            .await;

        assert_eq!(state.phase, GamePhase::WaitingDiscard);
        assert_eq!(state.current_turn_seat, Seat::West);
        let west_round_state = state
            .player_round_state
            .get(&Seat::West)
            .expect("west round state should exist");
        assert_eq!(west_round_state.melds.len(), 1);
        assert_eq!(
            west_round_state.melds[0].kind,
            crate::proto::client::MeldKind::Peng as i32
        );
        assert_eq!(west_round_state.concealed_tiles.len(), 0);
        let east_discard = state
            .player_round_state
            .get(&Seat::East)
            .and_then(|round_state| round_state.discards.last())
            .expect("east discard should exist");
        assert!(east_discard.claimed);
    }

    #[tokio::test]
    async fn claim_resolution_prefers_win_over_peng_and_chi() {
        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: true,
            reject_code: engine_proto::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine_proto::ActionKind::DeclareWin as i32,
            meld_result: None,
            winning_context: Some(engine_proto::WinningContext {
                win_type: engine_proto::WinType::Discard as i32,
                winning_tile: engine_proto::Tile::Character5 as i32,
                completes_standard_hand: true,
                completes_seven_pairs: false,
                completes_thirteen_orphans: false,
                tentative_fan_details: Vec::new(),
                tentative_total_fan: 8,
            }),
            suggested_follow_ups: Vec::new(),
            explanation: "claim accepted".to_owned(),
        })
        .with_calculate_response(engine_proto::CalculateScoreResponse {
            total_fan: 8,
            fan_details: vec![engine_proto::FanDetail {
                fan_code: "PING_HU".to_owned(),
                fan_name: "平胡".to_owned(),
                fan_value: 8,
                count: 1,
                description: "测试胡牌优先级".to_owned(),
            }],
            score_delta_by_seat: vec![
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::South as i32,
                    delta: 8,
                    final_total: INITIAL_SCORE + 8,
                },
                engine_proto::SeatScoreDelta {
                    seat: engine_proto::Seat::East as i32,
                    delta: -8,
                    final_total: INITIAL_SCORE - 8,
                },
            ],
            settlement_flags: vec![engine_proto::SettlementFlag::DiscardWin as i32],
        });
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-kappa".to_owned(),
            RuleEngineHandle::new(mock_rule_engine.clone()),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let mut east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");
        state.round_runtime = Some(RoundRuntimeState {
            live_wall: VecDeque::new(),
            dead_wall: VecDeque::new(),
            active_claim_window: None,
            last_resolved_action: Some(ResolvedActionRecord {
                event_seq: 98,
                actor_seat: Seat::East,
                action_kind: crate::proto::client::ActionKind::Discard,
                tile: Some(Tile::Character5),
                replacement_draw: false,
                rob_kong_candidate: false,
            }),
            turn_index: 1,
            replacement_draw_pending: false,
        });
        state.phase = GamePhase::WaitingClaim;
        state.player_round_state.get_mut(&Seat::East).unwrap().discards.push(
            crate::proto::client::DiscardTile {
                tile: Tile::Character5 as i32,
                turn_index: 1,
                claimed: false,
                drawn_and_discarded: false,
                source_event_seq: 99,
            },
        );
        state.player_round_state.get_mut(&Seat::South).unwrap().concealed_tiles =
            vec![Tile::Character4, Tile::Character6, Tile::Dot1];
        state.player_round_state.get_mut(&Seat::West).unwrap().concealed_tiles =
            vec![Tile::Character5, Tile::Character5, Tile::Dot2];
        state
            .round_runtime
            .as_mut()
            .expect("runtime should exist")
            .active_claim_window = Some(ActiveClaimWindowState {
            action_window_id: 1,
            source_event_seq: 99,
            source_seat: Seat::East,
            target_tile: Tile::Character5,
            trigger_action_kind: crate::proto::client::ActionKind::Discard,
            eligible_seats: vec![Seat::South, Seat::West],
            options_by_seat: HashMap::from([
                (
                    Seat::South,
                    vec![
                        PromptOption {
                            option_id: "south-chi".to_owned(),
                            action_kind: crate::proto::client::ActionKind::Chi as i32,
                            claim_kind: crate::proto::client::ClaimKind::Chi as i32,
                            win_type: crate::proto::client::WinType::Unspecified as i32,
                            candidate_tiles: vec![
                                Tile::Character4 as i32,
                                Tile::Character5 as i32,
                                Tile::Character6 as i32,
                            ],
                            consume_tiles: vec![Tile::Character4 as i32, Tile::Character6 as i32],
                        },
                        PromptOption {
                            option_id: "south-win".to_owned(),
                            action_kind: crate::proto::client::ActionKind::DeclareWin as i32,
                            claim_kind: crate::proto::client::ClaimKind::DiscardWin as i32,
                            win_type: crate::proto::client::WinType::Discard as i32,
                            candidate_tiles: vec![Tile::Character5 as i32],
                            consume_tiles: Vec::new(),
                        },
                    ],
                ),
                (
                    Seat::West,
                    vec![PromptOption {
                        option_id: "west-peng".to_owned(),
                        action_kind: crate::proto::client::ActionKind::Peng as i32,
                        claim_kind: crate::proto::client::ClaimKind::Peng as i32,
                        win_type: crate::proto::client::WinType::Unspecified as i32,
                        candidate_tiles: vec![Tile::Character5 as i32],
                        consume_tiles: vec![Tile::Character5 as i32, Tile::Character5 as i32],
                    }],
                ),
            ]),
            responded_seats: HashMap::new(),
            deadline_unix_ms: unix_time_ms() + 1_000,
        });

        let west_request = PlayerActionRequest {
            room_id: "room-kappa".to_owned(),
            match_id: "match-room-kappa".to_owned(),
            round_id: "hand-1".to_owned(),
            expected_event_seq: state.current_event_seq(),
            action_window_id: 1,
            action: Some(player_action_request::Action::Claim(crate::proto::client::ClaimAction {
                claim_kind: crate::proto::client::ClaimKind::Peng as i32,
                target_tile: Tile::Character5 as i32,
                consume_tiles: vec![Tile::Character5 as i32, Tile::Character5 as i32],
                source_seat: Seat::East as i32,
                source_event_seq: 99,
            })),
        };
        state
            .handle_claim_action(
                "req-west-peng".to_owned(),
                "conn-seat_west".to_owned(),
                Seat::West,
                &west_request,
                west_request
                    .action
                    .as_ref()
                    .and_then(|action| match action {
                        player_action_request::Action::Claim(claim) => Some(claim),
                        _ => None,
                    })
                    .expect("west claim should be present"),
            )
            .await;

        let south_request = PlayerActionRequest {
            room_id: "room-kappa".to_owned(),
            match_id: "match-room-kappa".to_owned(),
            round_id: "hand-1".to_owned(),
            expected_event_seq: state.current_event_seq(),
            action_window_id: 1,
            action: Some(player_action_request::Action::DeclareWin(DeclareWinAction {
                win_type: crate::proto::client::WinType::Discard as i32,
                winning_tile: Tile::Character5 as i32,
                source_seat: Seat::East as i32,
                source_event_seq: 99,
            })),
        };
        state
            .handle_declare_win_action(
                "req-south-win".to_owned(),
                "conn-seat_south".to_owned(),
                Seat::South,
                &south_request,
                south_request
                    .action
                    .as_ref()
                    .and_then(|action| match action {
                        player_action_request::Action::DeclareWin(declare_win) => Some(declare_win),
                        _ => None,
                    })
                    .expect("south win should be present"),
            )
            .await;

        let mut settlement_seen = false;
        for _ in 0..6 {
            let frame = recv_frame(&mut east_rx).await;
            if let Some(server_frame::Payload::RoundSettlement(settlement)) = frame.payload {
                assert_eq!(settlement.winner_seat, Seat::South as i32);
                assert_eq!(settlement.discarder_seat, Seat::East as i32);
                settlement_seen = true;
                break;
            }
        }

        assert!(settlement_seen, "round settlement should be broadcast");
        assert_eq!(state.phase, GamePhase::RoundSettlement);
        assert_eq!(
            state.players_by_seat.get(&Seat::South).unwrap().score,
            INITIAL_SCORE + 8
        );
        assert!(
            state
                .player_round_state
                .get(&Seat::West)
                .unwrap()
                .melds
                .is_empty()
        );
    }


    #[tokio::test]
    async fn draw_game_broadcasts_round_settlement_with_draw_flag() {
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-draw".to_owned(),
            RuleEngineHandle::default(),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let mut east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");

        state.start_round().expect("round should start successfully");
        state
            .round_runtime
            .as_mut()
            .expect("runtime should exist")
            .live_wall
            .clear();

        state.advance_turn_after_passes(Seat::East);

        let settlement = match recv_frame(&mut east_rx).await.payload {
            Some(server_frame::Payload::RoundSettlement(settlement)) => settlement,
            other => panic!("expected draw RoundSettlement, got {other:?}"),
        };

        assert_eq!(state.phase, GamePhase::RoundSettlement);
        assert_eq!(settlement.win_type, crate::proto::client::WinType::Unspecified as i32);
        assert_eq!(settlement.winner_seat, Seat::Unspecified as i32);
        assert!(settlement
            .settlement_flags
            .contains(&(crate::proto::client::SettlementFlag::DrawGame as i32)));
        assert!(settlement.player_results.iter().all(|result| result.round_delta == 0));
    }

    #[tokio::test]
    async fn advance_after_round_settlement_starts_next_hand_when_match_continues() {
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-next-hand".to_owned(),
            RuleEngineHandle::default(),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");

        state.start_round().expect("round should start successfully");
        state.phase = GamePhase::RoundSettlement;
        state.record_round_outcome(Some(Seat::East), true);

        state.advance_after_round_settlement(1).await;

        assert_eq!(state.phase, GamePhase::WaitingDiscard);
        assert_eq!(state.hand_number, 2);
        assert_eq!(state.dealer_seat, Seat::East);
        assert_eq!(state.dealer_streak, 1);
        assert_eq!(state.current_turn_seat, Seat::East);
        assert!(state.round_runtime.is_some());
        assert!(state
            .player_round_state
            .get(&Seat::East)
            .expect("east round state should exist")
            .drawn_tile
            .is_some());
    }

    #[tokio::test]
    async fn advance_after_round_settlement_finishes_match_and_broadcasts_match_settlement() {
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-match-finish".to_owned(),
            RuleEngineHandle::default(),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let mut east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");

        state.start_round().expect("round should start successfully");
        state.phase = GamePhase::RoundSettlement;
        state.hand_number = MATCH_TOTAL_HANDS;
        state.players_by_seat.get_mut(&Seat::South).unwrap().score = 31_000;
        state.players_by_seat.get_mut(&Seat::East).unwrap().score = 28_000;
        state.players_by_seat.get_mut(&Seat::West).unwrap().score = 24_000;
        state.players_by_seat.get_mut(&Seat::North).unwrap().score = 17_000;
        state.record_round_outcome(Some(Seat::South), false);

        state
            .advance_after_round_settlement(MATCH_TOTAL_HANDS)
            .await;

        let settlement = match recv_frame(&mut east_rx).await.payload {
            Some(server_frame::Payload::MatchSettlement(settlement)) => settlement,
            other => panic!("expected MatchSettlement, got {other:?}"),
        };

        assert_eq!(state.phase, GamePhase::MatchSettlement);
        assert_eq!(settlement.match_id, "match-room-match-finish");
        assert_eq!(settlement.standings.len(), 4);
        assert_eq!(settlement.standings[0].user_id, "user-south");
        assert_eq!(settlement.standings[0].rank, 1);
        assert!(state.match_runtime.last_match_settlement.is_some());
    }

    #[tokio::test]
    async fn resume_after_match_settlement_resends_sync_and_match_settlement() {
        let (command_tx, _command_rx) = mpsc::channel(ROOM_COMMAND_BUFFER);
        let mut state = RoomState::new(
            "room-resume-finish".to_owned(),
            RuleEngineHandle::default(),
            deterministic_wall_factory(),
            command_tx,
            MatchEventWriter::noop(),
            RoomRuntimeMode::Production,
        );

        let _east_rx = insert_test_player(&mut state, Seat::East, "user-east", "East");
        let _south_rx = insert_test_player(&mut state, Seat::South, "user-south", "South");
        let _west_rx = insert_test_player(&mut state, Seat::West, "user-west", "West");
        let _north_rx = insert_test_player(&mut state, Seat::North, "user-north", "North");

        state.start_round().expect("round should start successfully");
        state.phase = GamePhase::RoundSettlement;
        state.hand_number = MATCH_TOTAL_HANDS;
        state.players_by_seat.get_mut(&Seat::East).unwrap().score = 32_000;
        state.players_by_seat.get_mut(&Seat::South).unwrap().score = 26_000;
        state.players_by_seat.get_mut(&Seat::West).unwrap().score = 24_000;
        state.players_by_seat.get_mut(&Seat::North).unwrap().score = 18_000;
        state.record_round_outcome(Some(Seat::East), true);
        state
            .advance_after_round_settlement(MATCH_TOTAL_HANDS)
            .await;

        let (resume_connection, mut resume_rx) = test_connection("conn-resume-east");
        state.handle_resume(
            "req-resume".to_owned(),
            resume_connection,
            ResumeSessionRequest {
                room_id: "room-resume-finish".to_owned(),
                user_id: "user-east".to_owned(),
                resume_token: "resume-user-east".to_owned(),
                last_received_event_seq: 0,
            },
        );

        match recv_frame(&mut resume_rx).await.payload {
            Some(server_frame::Payload::SyncState(sync)) => {
                let snapshot = sync.snapshot.expect("snapshot should be present");
                assert_eq!(snapshot.phase, GamePhase::MatchSettlement as i32);
            }
            other => panic!("expected SyncState on resume, got {other:?}"),
        }

        match recv_frame(&mut resume_rx).await.payload {
            Some(server_frame::Payload::MatchSettlement(settlement)) => {
                assert_eq!(settlement.standings[0].user_id, "user-east");
            }
            other => panic!("expected MatchSettlement on resume, got {other:?}"),
        }
    }

}
