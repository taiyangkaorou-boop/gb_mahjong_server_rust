use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use gb_mahjong_server_rust::load_test::{
    print_human_summary, run_room_stress, LoadTestConfig, LoadTestRuntimeMode, WinnerPolicy,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliWinnerPolicy {
    Random,
}

#[derive(Parser, Debug)]
#[command(name = "room_stress")]
#[command(about = "In-process room-task stress test for GB Mahjong server")]
struct Cli {
    #[arg(long)]
    players: u64,
    #[arg(long)]
    rooms: Option<u64>,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long, default_value_t = 128)]
    max_actions_per_room: u32,
    #[arg(long, default_value_t = 4)]
    hands_per_match: u32,
    #[arg(long, default_value_t = 4096)]
    concurrency: usize,
    #[arg(long, default_value_t = 1000)]
    sample_interval_ms: u64,
    #[arg(long, value_enum, default_value_t = CliWinnerPolicy::Random)]
    winner_policy: CliWinnerPolicy,
    #[arg(long)]
    json_out: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let rooms = match cli.rooms {
        Some(rooms) => {
            if cli.players != rooms * 4 {
                bail!("players must equal rooms * 4 when --rooms is provided");
            }
            rooms
        }
        None => {
            if cli.players % 4 != 0 {
                bail!("players must be a multiple of 4 when --rooms is omitted");
            }
            cli.players / 4
        }
    };

    let config = LoadTestConfig {
        players: cli.players,
        rooms,
        seed: cli.seed,
        max_actions_per_room: cli.max_actions_per_room,
        hands_per_match: cli.hands_per_match,
        concurrency: cli.concurrency,
        sample_interval_ms: cli.sample_interval_ms,
        winner_policy: match cli.winner_policy {
            CliWinnerPolicy::Random => WinnerPolicy::Random,
        },
        runtime_mode: LoadTestRuntimeMode::RoomTaskOnly,
        json_out: cli.json_out,
    };

    let summary = run_room_stress(config).await?;
    print_human_summary(&summary);
    Ok(())
}
