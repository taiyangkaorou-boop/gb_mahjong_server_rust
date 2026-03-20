// db 模块负责“数据库记录类型 + PostgreSQL 仓储实现 + 对局事件后台写入器”。
// 房间状态机不会直接依赖 SQL 细节，而是只依赖这里暴露出来的抽象边界。
mod models;
mod postgres;

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::warn;

pub use models::{
    MatchEventRecord, MatchRecord, MatchReplayRecord, MatchStatusRecord, NewMatchEventRecord,
    ReplayEntryRecord, ReplayEventKind, ReplayRoundRecord, ReplayWarningRecord, RoomConfigRecord,
    RoomMemberRecord, RoomRecord, RoomSnapshotRecord, RoomStatusRecord, UserRecord,
    UserSessionRecord,
};
pub use postgres::{
    connect_pg_pool, DatabaseConfig, PostgresAuthRepository, PostgresLobbyRepository,
    PostgresMatchEventRepository,
};

#[async_trait]
pub trait MatchEventRepository: Send + Sync {
    // 房间状态机写侧只关心“追加事件”，读侧则补充“读对局摘要 + 读事件流”。
    // 这样 replay API 和事件写入可以复用同一份仓储实现。
    async fn append_event(&self, event: NewMatchEventRecord) -> anyhow::Result<()>;
    async fn list_events(&self, match_id: &str) -> anyhow::Result<Vec<MatchEventRecord>>;
    async fn get_match(&self, match_id: &str) -> anyhow::Result<Option<MatchRecord>>;
}

#[derive(Default)]
pub struct NoopMatchEventRepository;

#[async_trait]
impl MatchEventRepository for NoopMatchEventRepository {
    async fn append_event(&self, _event: NewMatchEventRecord) -> anyhow::Result<()> {
        Ok(())
    }

    async fn list_events(&self, _match_id: &str) -> anyhow::Result<Vec<MatchEventRecord>> {
        Ok(Vec::new())
    }

    async fn get_match(&self, _match_id: &str) -> anyhow::Result<Option<MatchRecord>> {
        Ok(None)
    }
}

#[derive(Clone)]
pub struct MatchQueryService {
    // QueryService 只做“鉴权后的查询视图”，不参与房间实时状态推进。
    repo: Arc<dyn MatchEventRepository>,
}

impl Default for MatchQueryService {
    fn default() -> Self {
        Self::new(Arc::new(NoopMatchEventRepository))
    }
}

impl MatchQueryService {
    pub fn new(repo: Arc<dyn MatchEventRepository>) -> Self {
        Self { repo }
    }

    pub async fn get_match_for_user(
        &self,
        user_id: &str,
        match_id: &str,
    ) -> anyhow::Result<Option<MatchRecord>> {
        let Some(record) = self.repo.get_match(match_id).await? else {
            return Ok(None);
        };
        if !match_contains_user(&record, user_id) {
            return Ok(None);
        }

        Ok(Some(record))
    }

    pub async fn list_events_for_user(
        &self,
        user_id: &str,
        match_id: &str,
    ) -> anyhow::Result<Option<Vec<MatchEventRecord>>> {
        let Some(record) = self.repo.get_match(match_id).await? else {
            return Ok(None);
        };
        if !match_contains_user(&record, user_id) {
            return Ok(None);
        }

        Ok(Some(self.repo.list_events(match_id).await?))
    }

    pub async fn build_replay_for_user(
        &self,
        user_id: &str,
        match_id: &str,
    ) -> anyhow::Result<Option<MatchReplayRecord>> {
        // replay 读取必须沿用与 /matches 和 /events 相同的权限边界，
        // 否则起手私有牌会通过 round_started 事件被非参与者读到。
        let Some(record) = self.repo.get_match(match_id).await? else {
            return Ok(None);
        };
        if !match_contains_user(&record, user_id) {
            return Ok(None);
        }
        if record.status != MatchStatusRecord::Finished {
            // 第一版 replay 只开放给已结束对局，避免把进行中 round_started 里的私有牌泄露出去。
            return Ok(None);
        }

        let events = self.repo.list_events(match_id).await?;
        Ok(Some(build_match_replay(record, events)))
    }
}

pub fn build_match_replay(
    match_record: MatchRecord,
    mut events: Vec<MatchEventRecord>,
) -> MatchReplayRecord {
    // replay 统一按 event_seq 排序，避免被输入顺序或 created_at 抖动影响。
    events.sort_by_key(|event| event.event_seq);

    let mut rounds = Vec::<ReplayRoundRecord>::new();
    let mut round_indexes = HashMap::<String, usize>::new();
    let mut terminal_entry = None;
    let mut warnings = Vec::<ReplayWarningRecord>::new();

    for event in events {
        let event_kind = map_replay_event_kind(&event.event_type);
        let (summary, invalid_payload, mut entry_warnings, round_metadata) =
            build_replay_summary(&event, event_kind);

        let entry = ReplayEntryRecord {
            event_seq: event.event_seq,
            round_id: event.round_id.clone(),
            event_type: event.event_type.clone(),
            event_kind,
            actor_user_id: event.actor_user_id.clone(),
            actor_seat: event.actor_seat.clone(),
            created_at_unix_ms: event.created_at_unix_ms,
            invalid_payload,
            summary,
            raw_event_payload: event.event_payload.clone(),
        };

        warnings.append(&mut entry_warnings);

        if matches!(event_kind, ReplayEventKind::MatchSettlement) {
            terminal_entry = Some(entry);
            continue;
        }

        let round_id = entry
            .round_id
            .clone()
            .unwrap_or_else(|| format!("orphan-round-{}", entry.event_seq));
        let round_index = *round_indexes.entry(round_id.clone()).or_insert_with(|| {
            rounds.push(ReplayRoundRecord {
                round_id: round_id.clone(),
                hand_number: None,
                prevailing_wind: None,
                dealer_seat: None,
                start_event_seq: None,
                settlement_event_seq: None,
                entries: Vec::new(),
            });
            rounds.len() - 1
        });
        let round = &mut rounds[round_index];

        if entry.round_id.is_none() {
            warnings.push(ReplayWarningRecord {
                event_seq: Some(entry.event_seq),
                event_type: entry.event_type.clone(),
                message: format!("event did not carry round_id and was grouped into {round_id}"),
            });
        }

        if let Some(round_metadata) = round_metadata {
            if round.hand_number.is_none() {
                round.hand_number = round_metadata.hand_number;
            }
            if round.prevailing_wind.is_none() {
                round.prevailing_wind = round_metadata.prevailing_wind;
            }
            if round.dealer_seat.is_none() {
                round.dealer_seat = round_metadata.dealer_seat;
            }
            round.start_event_seq.get_or_insert(entry.event_seq);
        }

        if matches!(entry.event_kind, ReplayEventKind::RoundStarted) {
            round.start_event_seq.get_or_insert(entry.event_seq);
        }
        if matches!(entry.event_kind, ReplayEventKind::RoundSettlement) {
            round.settlement_event_seq = Some(entry.event_seq);
        }

        round.entries.push(entry);
    }

    MatchReplayRecord {
        match_record,
        rounds,
        terminal_entry,
        warnings,
    }
}

fn match_contains_user(record: &MatchRecord, user_id: &str) -> bool {
    if record.created_by_user_id.as_deref() == Some(user_id) {
        return true;
    }

    record
        .seating_snapshot
        .as_object()
        .map(|seats| {
            seats.values().any(|seat| {
                seat.get("user_id")
                    .and_then(Value::as_str)
                    .map(|value| value == user_id)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[derive(Default)]
struct ReplayRoundMetadata {
    hand_number: Option<u32>,
    prevailing_wind: Option<String>,
    dealer_seat: Option<String>,
}

fn map_replay_event_kind(event_type: &str) -> ReplayEventKind {
    match event_type {
        "round_started" => ReplayEventKind::RoundStarted,
        "action_broadcast" => ReplayEventKind::ActionBroadcast,
        "claim_window_opened" => ReplayEventKind::ClaimWindowOpened,
        "claim_window_resolved" => ReplayEventKind::ClaimWindowResolved,
        "round_settlement" => ReplayEventKind::RoundSettlement,
        "match_settlement" => ReplayEventKind::MatchSettlement,
        _ => ReplayEventKind::Unknown,
    }
}

fn build_replay_summary(
    event: &MatchEventRecord,
    event_kind: ReplayEventKind,
) -> (
    Value,
    bool,
    Vec<ReplayWarningRecord>,
    Option<ReplayRoundMetadata>,
) {
    match event_kind {
        ReplayEventKind::RoundStarted => build_round_started_summary(event),
        ReplayEventKind::ActionBroadcast => build_action_broadcast_summary(event),
        ReplayEventKind::ClaimWindowOpened => build_claim_window_opened_summary(event),
        ReplayEventKind::ClaimWindowResolved => build_claim_window_resolved_summary(event),
        ReplayEventKind::RoundSettlement => build_round_settlement_summary(event),
        ReplayEventKind::MatchSettlement => build_match_settlement_summary(event),
        ReplayEventKind::Unknown => {
            let warning = ReplayWarningRecord {
                event_seq: Some(event.event_seq),
                event_type: event.event_type.clone(),
                message: "unknown event_type was preserved as raw replay entry".to_owned(),
            };
            (
                json!({
                    "type": "unknown",
                    "reason": format!("unsupported event_type: {}", event.event_type),
                }),
                false,
                vec![warning],
                None,
            )
        }
    }
}

fn build_round_started_summary(
    event: &MatchEventRecord,
) -> (
    Value,
    bool,
    Vec<ReplayWarningRecord>,
    Option<ReplayRoundMetadata>,
) {
    let payload = &event.event_payload;
    let hand_number = payload_u32(payload, "hand_number");
    let prevailing_wind = payload_string(payload, "prevailing_wind");
    let dealer_seat = payload_string(payload, "dealer_seat");
    let current_turn_seat = payload_string(payload, "current_turn_seat");
    let invalid_payload = hand_number.is_none()
        || prevailing_wind.is_none()
        || dealer_seat.is_none()
        || current_turn_seat.is_none();

    let summary = json!({
        "type": "round_started",
        "phase": payload_string(payload, "phase"),
        "prevailing_wind": prevailing_wind,
        "dealer_seat": dealer_seat,
        "current_turn_seat": current_turn_seat,
        "hand_number": hand_number,
        "dealer_streak": payload_u32(payload, "dealer_streak"),
        "wall_tiles_remaining": payload_u32(payload, "wall_tiles_remaining"),
        "dead_wall_tiles_remaining": payload_u32(payload, "dead_wall_tiles_remaining"),
        "players": payload_array(payload, "players"),
    });

    let warnings = invalid_payload
        .then(|| ReplayWarningRecord {
            event_seq: Some(event.event_seq),
            event_type: event.event_type.clone(),
            message: "round_started payload missed one or more required fields".to_owned(),
        })
        .into_iter()
        .collect();

    (
        summary,
        invalid_payload,
        warnings,
        Some(ReplayRoundMetadata {
            hand_number,
            prevailing_wind: payload_string(payload, "prevailing_wind"),
            dealer_seat: payload_string(payload, "dealer_seat"),
        }),
    )
}

fn build_action_broadcast_summary(
    event: &MatchEventRecord,
) -> (
    Value,
    bool,
    Vec<ReplayWarningRecord>,
    Option<ReplayRoundMetadata>,
) {
    let payload = &event.event_payload;
    let action_kind = payload_string(payload, "action_kind");
    let invalid_payload = action_kind.is_none();

    let summary = json!({
        "type": "action_broadcast",
        "action_kind": action_kind,
        "tile": payload_string(payload, "tile"),
        "draw_tile": payload_string(payload, "draw_tile"),
        "replacement_draw": payload_bool(payload, "replacement_draw"),
        "target_tile": payload_string(payload, "target_tile"),
        "source_seat": payload_string(payload, "source_seat"),
        "discarder_seat": payload_string(payload, "discarder_seat"),
        "win_type": payload_string(payload, "win_type"),
        "source_event_seq": payload_u64(payload, "source_event_seq"),
        "consume_tiles": payload_string_vec(payload, "consume_tiles"),
        "resulting_meld": payload.get("resulting_meld").cloned(),
        "tsumogiri": payload_bool(payload, "tsumogiri"),
        "wall_tiles_remaining": payload_u32(payload, "wall_tiles_remaining"),
    });

    let warnings = invalid_payload
        .then(|| ReplayWarningRecord {
            event_seq: Some(event.event_seq),
            event_type: event.event_type.clone(),
            message: "action_broadcast payload missed action_kind".to_owned(),
        })
        .into_iter()
        .collect();

    (summary, invalid_payload, warnings, None)
}

fn build_claim_window_opened_summary(
    event: &MatchEventRecord,
) -> (
    Value,
    bool,
    Vec<ReplayWarningRecord>,
    Option<ReplayRoundMetadata>,
) {
    let payload = &event.event_payload;
    let action_window_id = payload_u64(payload, "action_window_id");
    let source_event_seq = payload_u64(payload, "source_event_seq");
    let source_seat = payload_string(payload, "source_seat");
    let target_tile = payload_string(payload, "target_tile");
    let invalid_payload = action_window_id.is_none()
        || source_event_seq.is_none()
        || source_seat.is_none()
        || target_tile.is_none();

    let summary = json!({
        "type": "claim_window_opened",
        "action_window_id": action_window_id,
        "source_event_seq": source_event_seq,
        "source_seat": source_seat,
        "target_tile": target_tile,
        "trigger_action_kind": payload_string(payload, "trigger_action_kind"),
        "eligible_seats": payload_string_vec(payload, "eligible_seats"),
        "options_by_seat": payload_array(payload, "options_by_seat"),
        "deadline_unix_ms": payload_u64(payload, "deadline_unix_ms"),
    });

    let warnings = invalid_payload
        .then(|| ReplayWarningRecord {
            event_seq: Some(event.event_seq),
            event_type: event.event_type.clone(),
            message: "claim_window_opened payload missed one or more required fields".to_owned(),
        })
        .into_iter()
        .collect();

    (summary, invalid_payload, warnings, None)
}

fn build_claim_window_resolved_summary(
    event: &MatchEventRecord,
) -> (
    Value,
    bool,
    Vec<ReplayWarningRecord>,
    Option<ReplayRoundMetadata>,
) {
    let payload = &event.event_payload;
    let action_window_id = payload_u64(payload, "action_window_id");
    let resolution_kind = payload_string(payload, "resolution_kind");
    let invalid_payload = action_window_id.is_none() || resolution_kind.is_none();

    let summary = json!({
        "type": "claim_window_resolved",
        "action_window_id": action_window_id,
        "source_event_seq": payload_u64(payload, "source_event_seq"),
        "source_seat": payload_string(payload, "source_seat"),
        "target_tile": payload_string(payload, "target_tile"),
        "resolution_kind": resolution_kind,
        "winner_seat": payload_string(payload, "winner_seat"),
        "responses": payload_array(payload, "responses"),
    });

    let warnings = invalid_payload
        .then(|| ReplayWarningRecord {
            event_seq: Some(event.event_seq),
            event_type: event.event_type.clone(),
            message: "claim_window_resolved payload missed one or more required fields".to_owned(),
        })
        .into_iter()
        .collect();

    (summary, invalid_payload, warnings, None)
}

fn build_round_settlement_summary(
    event: &MatchEventRecord,
) -> (
    Value,
    bool,
    Vec<ReplayWarningRecord>,
    Option<ReplayRoundMetadata>,
) {
    let payload = &event.event_payload;
    let settlement_flags = payload_string_vec(payload, "settlement_flags");
    let draw_game = settlement_flags
        .iter()
        .any(|flag| flag == "SETTLEMENT_FLAG_DRAW_GAME");
    let invalid_payload = payload
        .get("player_results")
        .and_then(Value::as_array)
        .is_none();

    let summary = json!({
        "type": "round_settlement",
        "winner_seat": payload_string(payload, "winner_seat"),
        "discarder_seat": payload_string(payload, "discarder_seat"),
        "win_type": payload_string(payload, "win_type"),
        "winning_tile": payload_string(payload, "winning_tile"),
        "player_results": payload_array(payload, "player_results"),
        "fan_details": payload_array(payload, "fan_details"),
        "settlement_flags": settlement_flags,
        "wall_tiles_remaining": payload_u32(payload, "wall_tiles_remaining"),
        "draw_game": draw_game,
    });

    let warnings = invalid_payload
        .then(|| ReplayWarningRecord {
            event_seq: Some(event.event_seq),
            event_type: event.event_type.clone(),
            message: "round_settlement payload missed player_results array".to_owned(),
        })
        .into_iter()
        .collect();

    (summary, invalid_payload, warnings, None)
}

fn build_match_settlement_summary(
    event: &MatchEventRecord,
) -> (
    Value,
    bool,
    Vec<ReplayWarningRecord>,
    Option<ReplayRoundMetadata>,
) {
    let payload = &event.event_payload;
    // match_settlement 当前写库时把正式结果包在 final_result 下，
    // replay 这里统一向外摊平，避免前端再记内部落库细节。
    let final_result = payload
        .get("final_result")
        .and_then(Value::as_object)
        .cloned();
    let finished_at_unix_ms = final_result
        .as_ref()
        .and_then(|value| value.get("finished_at_unix_ms"))
        .and_then(Value::as_u64);
    let standings = final_result
        .as_ref()
        .and_then(|value| value.get("standings"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let invalid_payload = final_result.is_none();

    let summary = json!({
        "type": "match_settlement",
        "finished_at_unix_ms": finished_at_unix_ms,
        "standings": standings,
    });

    let warnings = invalid_payload
        .then(|| ReplayWarningRecord {
            event_seq: Some(event.event_seq),
            event_type: event.event_type.clone(),
            message: "match_settlement payload missed final_result object".to_owned(),
        })
        .into_iter()
        .collect();

    (summary, invalid_payload, warnings, None)
}

fn payload_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn payload_u64(payload: &Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(Value::as_u64)
}

fn payload_u32(payload: &Value, key: &str) -> Option<u32> {
    payload_u64(payload, key).and_then(|value| u32::try_from(value).ok())
}

fn payload_bool(payload: &Value, key: &str) -> Option<bool> {
    payload.get(key).and_then(Value::as_bool)
}

fn payload_array(payload: &Value, key: &str) -> Vec<Value> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn payload_string_vec(payload: &Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[derive(Clone, Default)]
pub struct MatchEventWriter {
    // RoomState 不直接 await 数据库写入，而是把事件顺序送进后台写入器。
    tx: Option<mpsc::UnboundedSender<NewMatchEventRecord>>,
}

impl MatchEventWriter {
    pub fn noop() -> Self {
        // 测试或不接数据库的场景下使用空写入器，避免让房间循环阻塞在持久化依赖上。
        Self::default()
    }

    pub fn from_repository(repo: Arc<dyn MatchEventRepository>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<NewMatchEventRecord>();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                // 后台顺序写入数据库；单条事件失败只记日志，不反向打断房间任务。
                if let Err(error) = repo.append_event(event).await {
                    warn!(?error, "failed to persist match event");
                }
            }
        });

        Self { tx: Some(tx) }
    }

    pub fn append(&self, event: NewMatchEventRecord) {
        let Some(tx) = &self.tx else {
            return;
        };

        // 这里只负责把事件送进后台队列，不在房间主循环里 await 数据库 I/O。
        if let Err(error) = tx.send(event) {
            warn!(?error, "failed to enqueue match event for persistence");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct StaticMatchRepository {
        record: Mutex<Option<MatchRecord>>,
        events: Mutex<Vec<MatchEventRecord>>,
    }

    impl StaticMatchRepository {
        fn seed(&self, record: MatchRecord, events: Vec<MatchEventRecord>) {
            *self.record.lock().expect("record lock should be healthy") = Some(record);
            *self.events.lock().expect("events lock should be healthy") = events;
        }
    }

    #[async_trait]
    impl MatchEventRepository for StaticMatchRepository {
        async fn append_event(&self, _event: NewMatchEventRecord) -> anyhow::Result<()> {
            Ok(())
        }

        async fn list_events(&self, _match_id: &str) -> anyhow::Result<Vec<MatchEventRecord>> {
            Ok(self
                .events
                .lock()
                .expect("events lock should be healthy")
                .clone())
        }

        async fn get_match(&self, _match_id: &str) -> anyhow::Result<Option<MatchRecord>> {
            Ok(self
                .record
                .lock()
                .expect("record lock should be healthy")
                .clone())
        }
    }

    fn fixture_match_record(user_id: &str) -> MatchRecord {
        MatchRecord {
            match_id: "match-1".to_owned(),
            room_id: "room-1".to_owned(),
            status: MatchStatusRecord::Finished,
            ruleset_id: "gb_mahjong_cn_v1".to_owned(),
            config_snapshot: json!({ "seat_count": 4 }),
            seating_snapshot: json!({
                "EAST": { "user_id": user_id, "display_name": "Alice" },
                "SOUTH": { "user_id": "user-b", "display_name": "Bob" },
            }),
            current_round_id: Some("round-1".to_owned()),
            prevailing_wind: Some("SEAT_EAST".to_owned()),
            dealer_seat: Some("SEAT_EAST".to_owned()),
            started_at_unix_ms: Some(100),
            ended_at_unix_ms: Some(200),
            created_by_user_id: Some(user_id.to_owned()),
            winner_user_id: Some(user_id.to_owned()),
            last_event_seq: 5,
            final_result: None,
            created_at_unix_ms: 100,
            updated_at_unix_ms: 200,
        }
    }

    fn fixture_event(
        event_seq: i64,
        event_type: &str,
        round_id: Option<&str>,
        payload: Value,
    ) -> MatchEventRecord {
        MatchEventRecord {
            event_id: event_seq,
            match_id: "match-1".to_owned(),
            round_id: round_id.map(ToOwned::to_owned),
            event_seq,
            event_type: event_type.to_owned(),
            actor_user_id: Some("user-a".to_owned()),
            actor_seat: Some("SEAT_EAST".to_owned()),
            request_id: None,
            correlation_id: None,
            causation_event_seq: None,
            event_payload: payload,
            created_at_unix_ms: 1_700_000_000_000 + event_seq as u64,
        }
    }

    #[test]
    fn build_match_replay_sorts_events_and_separates_match_settlement() {
        let record = fixture_match_record("user-a");
        let replay = build_match_replay(
            record,
            vec![
                fixture_event(
                    3,
                    "action_broadcast",
                    Some("round-1"),
                    json!({
                        "action_kind": "ACTION_KIND_DISCARD",
                        "tile": "TILE_CHARACTER_1",
                    }),
                ),
                fixture_event(
                    1,
                    "round_started",
                    Some("round-1"),
                    json!({
                        "phase": "GAME_PHASE_WAITING_DISCARD",
                        "prevailing_wind": "SEAT_EAST",
                        "dealer_seat": "SEAT_EAST",
                        "current_turn_seat": "SEAT_EAST",
                        "hand_number": 1,
                        "dealer_streak": 0,
                        "wall_tiles_remaining": 70,
                        "dead_wall_tiles_remaining": 14,
                        "players": [],
                    }),
                ),
                fixture_event(
                    4,
                    "round_settlement",
                    Some("round-1"),
                    json!({
                        "winner_seat": "SEAT_EAST",
                        "discarder_seat": Value::Null,
                        "win_type": "WIN_TYPE_SELF_DRAW",
                        "winning_tile": "TILE_CHARACTER_1",
                        "player_results": [],
                        "fan_details": [],
                        "settlement_flags": ["SETTLEMENT_FLAG_SELF_DRAW"],
                        "wall_tiles_remaining": 40,
                    }),
                ),
                fixture_event(
                    5,
                    "match_settlement",
                    None,
                    json!({
                        "final_result": {
                            "finished_at_unix_ms": 999,
                            "standings": [{ "user_id": "user-a", "rank": 1 }],
                        }
                    }),
                ),
            ],
        );

        assert_eq!(replay.rounds.len(), 1);
        assert_eq!(replay.rounds[0].round_id, "round-1");
        assert_eq!(replay.rounds[0].hand_number, Some(1));
        assert_eq!(replay.rounds[0].start_event_seq, Some(1));
        assert_eq!(replay.rounds[0].settlement_event_seq, Some(4));
        let event_seqs = replay.rounds[0]
            .entries
            .iter()
            .map(|entry| entry.event_seq)
            .collect::<Vec<_>>();
        assert_eq!(event_seqs, vec![1, 3, 4]);
        assert_eq!(
            replay.terminal_entry.as_ref().map(|entry| entry.event_seq),
            Some(5)
        );
    }

    #[test]
    fn build_match_replay_marks_invalid_payload_without_panicking() {
        let replay = build_match_replay(
            fixture_match_record("user-a"),
            vec![fixture_event(
                2,
                "round_started",
                Some("round-bad"),
                json!({
                    "phase": "GAME_PHASE_WAITING_DISCARD",
                    "players": [],
                }),
            )],
        );

        assert_eq!(replay.rounds.len(), 1);
        assert!(replay.rounds[0].entries[0].invalid_payload);
        assert_eq!(replay.warnings.len(), 1);
        assert_eq!(replay.warnings[0].event_seq, Some(2));
    }

    #[tokio::test]
    async fn match_query_service_builds_replay_for_authorized_user() {
        let repo = Arc::new(StaticMatchRepository::default());
        repo.seed(
            fixture_match_record("user-a"),
            vec![fixture_event(
                1,
                "round_started",
                Some("round-1"),
                json!({
                    "phase": "GAME_PHASE_WAITING_DISCARD",
                    "prevailing_wind": "SEAT_EAST",
                    "dealer_seat": "SEAT_EAST",
                    "current_turn_seat": "SEAT_EAST",
                    "hand_number": 1,
                    "dealer_streak": 0,
                    "wall_tiles_remaining": 70,
                    "dead_wall_tiles_remaining": 14,
                    "players": [],
                }),
            )],
        );
        let service = MatchQueryService::new(repo);

        let replay = service
            .build_replay_for_user("user-a", "match-1")
            .await
            .expect("replay query should succeed")
            .expect("authorized user should see replay");

        assert_eq!(replay.rounds.len(), 1);
        assert_eq!(
            replay.rounds[0].entries[0].event_kind,
            ReplayEventKind::RoundStarted
        );
    }

    #[tokio::test]
    async fn match_query_service_hides_replay_from_non_member() {
        let repo = Arc::new(StaticMatchRepository::default());
        repo.seed(fixture_match_record("user-a"), Vec::new());
        let service = MatchQueryService::new(repo);

        let replay = service
            .build_replay_for_user("outsider", "match-1")
            .await
            .expect("replay query should succeed");

        assert!(replay.is_none());
    }

    #[tokio::test]
    async fn match_query_service_hides_replay_for_active_match() {
        let repo = Arc::new(StaticMatchRepository::default());
        let mut record = fixture_match_record("user-a");
        record.status = MatchStatusRecord::Active;
        repo.seed(
            record,
            vec![fixture_event(
                1,
                "round_started",
                Some("round-1"),
                json!({
                    "phase": "GAME_PHASE_WAITING_DISCARD",
                    "prevailing_wind": "SEAT_EAST",
                    "dealer_seat": "SEAT_EAST",
                    "current_turn_seat": "SEAT_EAST",
                    "hand_number": 1,
                    "dealer_streak": 0,
                    "wall_tiles_remaining": 70,
                    "dead_wall_tiles_remaining": 14,
                    "players": [],
                }),
            )],
        );
        let service = MatchQueryService::new(repo);

        let replay = service
            .build_replay_for_user("user-a", "match-1")
            .await
            .expect("replay query should succeed");

        assert!(replay.is_none());
    }
}
