mod app;
mod auth;
mod db;
mod engine;
mod http;
mod lobby;
mod proto;
mod room;
mod ws;

use std::{env, net::SocketAddr, sync::Arc};

use anyhow::Context;
use app::{build_router, AppState};
use auth::{AuthService, DEFAULT_SESSION_TTL_MS};
use db::{
    connect_pg_pool, DatabaseConfig, MatchEventWriter, PostgresAuthRepository,
    PostgresLobbyRepository, PostgresMatchEventRepository,
};
use engine::{RuleEngineHandle, DEFAULT_RULE_ENGINE_ENDPOINT};
use lobby::LobbyService;
use room::RoomManager;
use tokio::{net::TcpListener, signal};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 启动流程保持单向依赖：
    // 日志 -> 规则引擎句柄 -> AppState -> Router -> TCP 监听。
    init_tracing();

    // 游戏服本身不做规则计算，只保留一个可复用的 gRPC 客户端句柄，
    // 由各个房间任务在需要时异步调用规则引擎。
    let rule_engine_endpoint = resolve_rule_engine_endpoint();
    let rule_engine = RuleEngineHandle::tonic(rule_engine_endpoint.clone())
        .with_context(|| format!("configure rule engine client for {rule_engine_endpoint}"))?;
    let database_config = DatabaseConfig::from_env().context("resolve database configuration")?;
    let pool = connect_pg_pool(&database_config)
        .await
        .context("connect postgres pool for gateway services")?;
    let auth_service = AuthService::new(
        Arc::new(PostgresAuthRepository::new(pool.clone())),
        DEFAULT_SESSION_TTL_MS,
    );
    let lobby_service = LobbyService::new(Arc::new(PostgresLobbyRepository::new(pool.clone())));
    let match_event_writer = MatchEventWriter::from_repository(Arc::new(
        PostgresMatchEventRepository::new(pool),
    ));
    let state = AppState::new(
        RoomManager::with_rule_engine_and_event_writer(rule_engine, match_event_writer),
        auth_service,
        lobby_service,
    );
    let app = build_router(state);

    // 默认监听 18080，避免和机器上常见的 8080 开发服务冲突。
    let listen_addr = resolve_listen_addr().context("resolve listen address")?;
    let listener = TcpListener::bind(listen_addr)
        .await
        .context("bind websocket listener")?;

    tracing::info!(%listen_addr, %rule_engine_endpoint, "gb mahjong server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("run axum server")?;

    Ok(())
}

fn init_tracing() {
    // 本地开发默认打开较详细日志，线上可用 RUST_LOG 覆盖。
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,gb_mahjong_server_rust=debug"));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();
}

async fn shutdown_signal() {
    // 同时监听 Ctrl-C 和 SIGTERM，便于本地开发与容器环境都能优雅停机。
    let ctrl_c = async {
        if let Err(error) = signal::ctrl_c().await {
            tracing::error!(?error, "failed to install ctrl-c handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};

        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::error!(?error, "failed to install sigterm handler");
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received");
}

fn resolve_listen_addr() -> anyhow::Result<SocketAddr> {
    // 监听地址统一由环境变量驱动，便于本地、测试和部署环境复用同一套二进制。
    let host = env::var("GB_MAHJONG_HOST").unwrap_or_else(|_| "0.0.0.0".to_owned());

    let port = match env::var("GB_MAHJONG_PORT") {
        Ok(value) => value
            .parse::<u16>()
            .with_context(|| format!("parse GB_MAHJONG_PORT={value}"))?,
        Err(env::VarError::NotPresent) => 18080,
        Err(error) => {
            return Err(anyhow::Error::new(error).context("read GB_MAHJONG_PORT"));
        }
    };

    format!("{host}:{port}")
        .parse()
        .with_context(|| format!("parse listen address {host}:{port}"))
}

fn resolve_rule_engine_endpoint() -> String {
    // 规则引擎地址独立配置，方便把 C++ 服务部署到单独进程或独立机器。
    env::var("GB_MAHJONG_ENGINE_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_RULE_ENGINE_ENDPOINT.to_owned())
}
