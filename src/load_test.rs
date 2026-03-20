use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::{sync::watch, task::JoinSet, time};

use crate::room::{LoadTestRoomResult, LoadTestRoomSpec, LoadTestWinnerPolicy, RoomManager};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum LoadTestRuntimeMode {
    // 第一阶段只压 room task，不压 WebSocket/数据库/规则引擎。
    RoomTaskOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum WinnerPolicy {
    Random,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoadTestConfig {
    pub players: u64,
    pub rooms: u64,
    pub seed: u64,
    // 每手牌最多执行多少次随机出牌；达到阈值后用随机赢家快速收口。
    pub max_actions_per_room: u32,
    // 每个压测房间要完整跑多少手。东风场默认是 4 手。
    pub hands_per_match: u32,
    pub concurrency: usize,
    pub sample_interval_ms: u64,
    pub winner_policy: WinnerPolicy,
    pub runtime_mode: LoadTestRuntimeMode,
    pub json_out: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StressSample {
    pub elapsed_ms: u64,
    pub active_rooms: u64,
    pub completed_rooms: u64,
    pub failed_rooms: u64,
    pub total_actions: u64,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub cumulative_cpu_seconds: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StressRunSummary {
    pub players: u64,
    pub rooms: u64,
    pub configured_concurrency: usize,
    pub peak_active_rooms: u64,
    pub seed: u64,
    pub max_actions_per_room: u32,
    pub hands_per_match: u32,
    pub runtime_mode: LoadTestRuntimeMode,
    pub winner_policy: WinnerPolicy,
    pub completed_rooms: u64,
    pub failed_rooms: u64,
    pub total_actions: u64,
    pub elapsed_ms: u64,
    pub actions_per_sec: f64,
    pub rooms_per_sec: f64,
    pub average_actions_per_room: f64,
    pub peak_rss_bytes: Option<u64>,
    pub end_rss_bytes: Option<u64>,
    pub peak_cpu_percent: Option<f64>,
    pub failure_examples: Vec<String>,
    pub samples: Vec<StressSample>,
}

#[derive(Default)]
struct StressCounters {
    active_rooms: AtomicU64,
    peak_active_rooms: AtomicU64,
    completed_rooms: AtomicU64,
    failed_rooms: AtomicU64,
    total_actions: AtomicU64,
    completed_hands: AtomicU64,
}

struct ProcessSnapshot {
    captured_at: Instant,
    cpu_ticks: u64,
    rss_bytes: u64,
}

// 压测入口直接驱动 RoomManager，不经过 HTTP/WS 网关，
// 这样测到的就是 tokio room task + mpsc + RoomState 本体成本。
pub async fn run_room_stress(config: LoadTestConfig) -> Result<StressRunSummary> {
    validate_config(&config)?;

    let counters = Arc::new(StressCounters::default());
    let samples = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, stop_rx) = watch::channel(false);
    let start = Instant::now();
    // 当前第一阶段只支持 room-task-only 模式，后续如果要接 WebSocket/DB 压测，
    // 可以在这里按 runtime_mode 分流不同的装配方式。
    let manager = RoomManager::load_test(config.seed);
    let sample_interval = Duration::from_millis(config.sample_interval_ms.max(1));
    let sample_task = tokio::spawn(sample_process(
        counters.clone(),
        samples.clone(),
        sample_interval,
        start,
        stop_rx,
    ));

    let mut join_set = JoinSet::new();
    let mut launched_rooms = 0_u64;
    let mut failure_examples = Vec::new();

    while launched_rooms < config.rooms || !join_set.is_empty() {
        while launched_rooms < config.rooms && join_set.len() < config.concurrency {
            let room_index = launched_rooms;
            let room_id = format!("stress-room-{room_index}");
            let spec = LoadTestRoomSpec {
                room_id,
                room_index,
                player_base_index: room_index * 4,
                max_actions: config.max_actions_per_room,
                hands_per_match: config.hands_per_match,
                seed: config.seed ^ room_index.wrapping_mul(0x9e37_79b9_7f4a_7c15),
                winner_policy: match config.winner_policy {
                    WinnerPolicy::Random => LoadTestWinnerPolicy::Random,
                },
            };
            let counters_for_room = counters.clone();
            let manager_for_room = manager.clone();
            let active_after_increment = counters_for_room
                .active_rooms
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            counters_for_room
                .peak_active_rooms
                .fetch_max(active_after_increment, Ordering::Relaxed);
            join_set.spawn(async move { manager_for_room.dispatch_load_test_room(spec).await });
            launched_rooms += 1;
        }

        let Some(joined) = join_set.join_next().await else {
            break;
        };
        counters.active_rooms.fetch_sub(1, Ordering::Relaxed);

        match joined {
            Ok(Ok(room_result)) => {
                record_room_result(&counters, &mut failure_examples, room_result)
            }
            Ok(Err(error)) => {
                counters.failed_rooms.fetch_add(1, Ordering::Relaxed);
                if failure_examples.len() < 16 {
                    failure_examples.push(format!("room dispatch failed: {error}"));
                }
            }
            Err(error) => {
                counters.failed_rooms.fetch_add(1, Ordering::Relaxed);
                if failure_examples.len() < 16 {
                    failure_examples.push(format!("room task join failed: {error}"));
                }
            }
        }
    }

    let _ = stop_tx.send(true);
    let _ = sample_task.await;

    let samples = samples
        .lock()
        .expect("sample lock should not be poisoned")
        .clone();
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let completed_rooms = counters.completed_rooms.load(Ordering::Relaxed);
    let failed_rooms = counters.failed_rooms.load(Ordering::Relaxed);
    let total_actions = counters.total_actions.load(Ordering::Relaxed);
    let peak_active_rooms = counters.peak_active_rooms.load(Ordering::Relaxed);
    let peak_rss_bytes = samples.iter().filter_map(|sample| sample.rss_bytes).max();
    let end_rss_bytes = samples.last().and_then(|sample| sample.rss_bytes);
    let peak_cpu_percent = samples
        .iter()
        .filter_map(|sample| sample.cpu_percent)
        .max_by(f64::total_cmp);
    let elapsed_secs = (elapsed_ms as f64 / 1000.0).max(0.000_001);
    let summary = StressRunSummary {
        players: config.players,
        rooms: config.rooms,
        configured_concurrency: config.concurrency,
        peak_active_rooms,
        seed: config.seed,
        max_actions_per_room: config.max_actions_per_room,
        hands_per_match: config.hands_per_match,
        runtime_mode: config.runtime_mode,
        winner_policy: config.winner_policy,
        completed_rooms,
        failed_rooms,
        total_actions,
        elapsed_ms,
        actions_per_sec: total_actions as f64 / elapsed_secs,
        rooms_per_sec: (completed_rooms + failed_rooms) as f64 / elapsed_secs,
        average_actions_per_room: if completed_rooms == 0 {
            0.0
        } else {
            total_actions as f64 / completed_rooms as f64
        },
        peak_rss_bytes,
        end_rss_bytes,
        peak_cpu_percent,
        failure_examples,
        samples,
    };

    if let Some(path) = &config.json_out {
        let payload =
            serde_json::to_vec_pretty(&summary).context("serialize stress summary json")?;
        fs::write(path, payload)
            .with_context(|| format!("write stress summary json to {}", path.display()))?;
    }

    Ok(summary)
}

pub fn print_human_summary(summary: &StressRunSummary) {
    println!("room stress summary");
    println!("  players: {}", summary.players);
    println!("  rooms: {}", summary.rooms);
    println!(
        "  configured concurrency: {}",
        summary.configured_concurrency
    );
    println!("  peak active rooms: {}", summary.peak_active_rooms);
    println!("  completed rooms: {}", summary.completed_rooms);
    println!("  failed rooms: {}", summary.failed_rooms);
    println!("  total actions: {}", summary.total_actions);
    println!("  elapsed ms: {}", summary.elapsed_ms);
    println!("  actions/sec: {:.2}", summary.actions_per_sec);
    println!("  rooms/sec: {:.2}", summary.rooms_per_sec);
    println!(
        "  average actions/room: {:.2}",
        summary.average_actions_per_room
    );
    println!("  hands/match: {}", summary.hands_per_match);
    if let Some(value) = summary.peak_rss_bytes {
        println!("  peak rss bytes: {}", value);
    }
    if let Some(value) = summary.end_rss_bytes {
        println!("  end rss bytes: {}", value);
    }
    if let Some(value) = summary.peak_cpu_percent {
        println!("  peak cpu percent: {:.2}", value);
    }
    if !summary.failure_examples.is_empty() {
        println!("  failure examples:");
        for failure in &summary.failure_examples {
            println!("    - {}", failure);
        }
    }
}

fn validate_config(config: &LoadTestConfig) -> Result<()> {
    if config.players == 0 {
        bail!("players must be greater than zero");
    }
    if config.players % 4 != 0 {
        bail!("players must be a multiple of 4");
    }
    if config.rooms == 0 {
        bail!("rooms must be greater than zero");
    }
    if config.players != config.rooms * 4 {
        bail!("players must equal rooms * 4 for fixed four-seat rooms");
    }
    if config.max_actions_per_room == 0 {
        bail!("max_actions_per_room must be greater than zero");
    }
    if config.hands_per_match == 0 {
        bail!("hands_per_match must be greater than zero");
    }
    if config.concurrency == 0 {
        bail!("concurrency must be greater than zero");
    }
    Ok(())
}

fn record_room_result(
    counters: &StressCounters,
    failure_examples: &mut Vec<String>,
    room_result: LoadTestRoomResult,
) {
    counters
        .total_actions
        .fetch_add(room_result.actions_executed as u64, Ordering::Relaxed);
    counters
        .completed_hands
        .fetch_add(room_result.hands_completed as u64, Ordering::Relaxed);
    if room_result.success {
        counters.completed_rooms.fetch_add(1, Ordering::Relaxed);
    } else {
        counters.failed_rooms.fetch_add(1, Ordering::Relaxed);
        if failure_examples.len() < 16 {
            failure_examples.push(format!(
                "room {} failed: {}",
                room_result.room_id,
                room_result
                    .failure_reason
                    .unwrap_or_else(|| "unknown failure".to_owned())
            ));
        }
    }
}

// 采样器按固定间隔读取当前进程的 CPU/RSS，并和房间计数器合并成时间序列。
async fn sample_process(
    counters: Arc<StressCounters>,
    samples: Arc<Mutex<Vec<StressSample>>>,
    sample_interval: Duration,
    start: Instant,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut interval = time::interval(sample_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut previous_snapshot = read_process_snapshot();

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let current_snapshot = read_process_snapshot();
                let sample = build_sample(&counters, start, previous_snapshot.as_ref(), current_snapshot.as_ref());
                if let Some(snapshot) = current_snapshot {
                    previous_snapshot = Some(snapshot);
                }
                samples.lock().expect("sample lock should not be poisoned").push(sample);
            }
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    let current_snapshot = read_process_snapshot();
                    let sample = build_sample(&counters, start, previous_snapshot.as_ref(), current_snapshot.as_ref());
                    samples.lock().expect("sample lock should not be poisoned").push(sample);
                    break;
                }
            }
        }
    }
}

fn build_sample(
    counters: &StressCounters,
    start: Instant,
    previous_snapshot: Option<&ProcessSnapshot>,
    current_snapshot: Option<&ProcessSnapshot>,
) -> StressSample {
    let cpu_percent = match (previous_snapshot, current_snapshot) {
        (Some(previous), Some(current)) => {
            let elapsed = current
                .captured_at
                .saturating_duration_since(previous.captured_at)
                .as_secs_f64();
            if elapsed <= f64::EPSILON {
                None
            } else {
                let ticks_per_second = clock_ticks_per_second();
                if ticks_per_second <= f64::EPSILON {
                    None
                } else {
                    let cpu_seconds = (current.cpu_ticks.saturating_sub(previous.cpu_ticks)) as f64
                        / ticks_per_second;
                    Some((cpu_seconds / elapsed) * 100.0)
                }
            }
        }
        _ => None,
    };

    let cumulative_cpu_seconds = current_snapshot.map(|snapshot| {
        let ticks_per_second = clock_ticks_per_second();
        if ticks_per_second <= f64::EPSILON {
            0.0
        } else {
            snapshot.cpu_ticks as f64 / ticks_per_second
        }
    });

    StressSample {
        elapsed_ms: start.elapsed().as_millis() as u64,
        active_rooms: counters.active_rooms.load(Ordering::Relaxed),
        completed_rooms: counters.completed_rooms.load(Ordering::Relaxed),
        failed_rooms: counters.failed_rooms.load(Ordering::Relaxed),
        total_actions: counters.total_actions.load(Ordering::Relaxed),
        rss_bytes: current_snapshot.map(|snapshot| snapshot.rss_bytes),
        cpu_percent,
        cumulative_cpu_seconds,
    }
}

// 第一阶段优先依赖 Linux /proc，避免为了压测再引入额外系统监控依赖。
fn read_process_snapshot() -> Option<ProcessSnapshot> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string("/proc/self/stat").ok()?;
        let (_, tail) = stat.rsplit_once(") ")?;
        let fields: Vec<&str> = tail.split_whitespace().collect();
        if fields.len() <= 21 {
            return None;
        }

        let utime = fields.get(11)?.parse::<u64>().ok()?;
        let stime = fields.get(12)?.parse::<u64>().ok()?;
        let rss_pages = fields.get(21)?.parse::<i64>().ok()?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let rss_bytes = if rss_pages <= 0 || page_size <= 0 {
            0
        } else {
            rss_pages as u64 * page_size as u64
        };

        Some(ProcessSnapshot {
            captured_at: Instant::now(),
            cpu_ticks: utime + stime,
            rss_bytes,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn clock_ticks_per_second() -> f64 {
    #[cfg(target_os = "linux")]
    {
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks <= 0 {
            100.0
        } else {
            ticks as f64
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        100.0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    use async_trait::async_trait;

    use super::*;
    use crate::{
        db::{MatchEventRepository, MatchEventWriter, NewMatchEventRecord},
        engine::{MockRuleEngine, RuleEngineHandle},
        proto::engine as engine_proto,
    };

    #[test]
    fn load_test_config_requires_four_player_rooms() {
        let error = validate_config(&LoadTestConfig {
            players: 6,
            rooms: 1,
            seed: 1,
            max_actions_per_room: 16,
            hands_per_match: 4,
            concurrency: 1,
            sample_interval_ms: 10,
            winner_policy: WinnerPolicy::Random,
            runtime_mode: LoadTestRuntimeMode::RoomTaskOnly,
            json_out: None,
        })
        .expect_err("config should reject non-four-player room counts");

        assert!(error.to_string().contains("multiple of 4"));
    }

    #[tokio::test]
    async fn single_room_stress_run_is_deterministic_for_seed() {
        let config = LoadTestConfig {
            players: 4,
            rooms: 1,
            seed: 42,
            max_actions_per_room: 32,
            hands_per_match: 4,
            concurrency: 1,
            sample_interval_ms: 10,
            winner_policy: WinnerPolicy::Random,
            runtime_mode: LoadTestRuntimeMode::RoomTaskOnly,
            json_out: None,
        };

        let first = run_room_stress(config.clone())
            .await
            .expect("first stress run should succeed");
        let second = run_room_stress(config)
            .await
            .expect("second stress run should succeed");

        assert_eq!(first.total_actions, second.total_actions);
        assert_eq!(first.completed_rooms, second.completed_rooms);
        assert_eq!(first.failed_rooms, second.failed_rooms);
    }

    #[tokio::test]
    async fn small_stress_run_finishes_rooms() {
        let summary = run_room_stress(LoadTestConfig {
            players: 400,
            rooms: 100,
            seed: 7,
            max_actions_per_room: 24,
            hands_per_match: 4,
            concurrency: 16,
            sample_interval_ms: 10,
            winner_policy: WinnerPolicy::Random,
            runtime_mode: LoadTestRuntimeMode::RoomTaskOnly,
            json_out: None,
        })
        .await
        .expect("small stress run should succeed");

        assert_eq!(summary.completed_rooms, 100);
        assert_eq!(summary.failed_rooms, 0);
        assert_eq!(summary.hands_per_match, 4);
        assert!(summary.total_actions > 0);
    }

    #[tokio::test]
    async fn load_test_room_does_not_touch_rule_engine_or_match_events() {
        #[derive(Default)]
        struct CountingEventRepository {
            writes: AtomicU64,
        }

        #[async_trait]
        impl MatchEventRepository for CountingEventRepository {
            async fn append_event(&self, _event: NewMatchEventRecord) -> anyhow::Result<()> {
                self.writes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }

            async fn list_events(
                &self,
                _match_id: &str,
            ) -> anyhow::Result<Vec<crate::db::MatchEventRecord>> {
                Ok(Vec::new())
            }

            async fn get_match(
                &self,
                _match_id: &str,
            ) -> anyhow::Result<Option<crate::db::MatchRecord>> {
                Ok(None)
            }
        }

        let mock_rule_engine = MockRuleEngine::new(engine_proto::ValidateActionResponse {
            is_legal: true,
            reject_code: engine_proto::ValidationRejectCode::Unspecified as i32,
            derived_action_type: engine_proto::ActionKind::Unspecified as i32,
            meld_result: None,
            winning_context: None,
            suggested_follow_ups: Vec::new(),
            explanation: "load-test noop".to_owned(),
        });
        let event_repo = Arc::new(CountingEventRepository::default());
        let manager = RoomManager::load_test_with_dependencies(
            99,
            RuleEngineHandle::new(mock_rule_engine.clone()),
            MatchEventWriter::from_repository(event_repo.clone()),
        );

        let result = manager
            .dispatch_load_test_room(LoadTestRoomSpec {
                room_id: "load-test-room".to_owned(),
                room_index: 0,
                player_base_index: 0,
                max_actions: 16,
                hands_per_match: 4,
                seed: 99,
                winner_policy: LoadTestWinnerPolicy::Random,
            })
            .await
            .expect("load-test room should run");

        assert!(result.success);
        assert_eq!(result.hands_completed, 4);
        assert!(mock_rule_engine.validate_requests().is_empty());
        assert!(mock_rule_engine.calculate_requests().is_empty());
        assert_eq!(event_repo.writes.load(Ordering::Relaxed), 0);
    }
}
