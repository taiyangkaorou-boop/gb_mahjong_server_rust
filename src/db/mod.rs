// db 模块负责“数据库记录类型 + PostgreSQL 仓储实现 + 对局事件后台写入器”。
// 房间状态机不会直接依赖 SQL 细节，而是只依赖这里暴露出来的抽象边界。
mod models;
mod postgres;

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::warn;

pub use models::{
    MatchEventRecord, MatchRecord, MatchStatusRecord, NewMatchEventRecord, RoomConfigRecord,
    RoomMemberRecord, RoomRecord, RoomSnapshotRecord, RoomStatusRecord, UserRecord,
    UserSessionRecord,
};
pub use postgres::{
    connect_pg_pool, DatabaseConfig, PostgresAuthRepository, PostgresLobbyRepository,
    PostgresMatchEventRepository,
};

#[async_trait]
pub trait MatchEventRepository: Send + Sync {
    // 房间状态机只关心“追加事件”和“按顺序读取事件流”两个能力。
    async fn append_event(&self, event: NewMatchEventRecord) -> anyhow::Result<()>;
    async fn list_events(&self, match_id: &str) -> anyhow::Result<Vec<MatchEventRecord>>;
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
