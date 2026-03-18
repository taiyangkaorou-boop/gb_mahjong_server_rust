mod app;
mod engine;
mod proto;
mod room;
mod ws;

use std::{env, net::SocketAddr};

use anyhow::Context;
use app::{build_router, AppState};
use engine::{RuleEngineHandle, DEFAULT_RULE_ENGINE_ENDPOINT};
use room::RoomManager;
use tokio::{net::TcpListener, signal};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let rule_engine_endpoint = resolve_rule_engine_endpoint();
    let rule_engine = RuleEngineHandle::tonic(rule_engine_endpoint.clone())
        .with_context(|| format!("configure rule engine client for {rule_engine_endpoint}"))?;
    let state = AppState::with_room_manager(RoomManager::with_rule_engine(rule_engine));
    let app = build_router(state);

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
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,gb_mahjong_server_rust=debug"));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();
}

async fn shutdown_signal() {
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
    env::var("GB_MAHJONG_ENGINE_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_RULE_ENGINE_ENDPOINT.to_owned())
}
