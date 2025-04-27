use std::{env, ffi::OsString, path::Path, process::exit, sync::Arc, time::Duration};

use anyhow::{Context as _, Error, Result};
use dashmap::DashMap;
use futures::lock::Mutex;
use hashbrown::HashMap;
use jwalk::WalkDir;
use logging::initialize_logging;
use serenity::{client::Client, model::gateway::GatewayIntents, prelude::TypeMapKey};
use songbird::{
    input::{cached::Memory, File},
    SerenityInit,
};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions, PgPool,
};
use tracing::log::LevelFilter;
use utils::RateLimiter;
use voicevox::Voicevox;

use crate::{
    audio::{
        cache::{ConstCacheable, PredefinedUtterance},
        processor::SongbirdAudioProcessor,
        VoicevoxAudioRepository,
    },
    speaker::Speaker,
};

mod audio;
mod character_converter;
mod commands;
mod database;
mod event_handler;
mod regex;
mod speaker;
mod utils;

struct VoicevoxClient;

impl TypeMapKey for VoicevoxClient {
    type Value = Arc<Mutex<Voicevox>>;
}

#[tokio::main]
async fn main() {
    initialize_logging();

    let token = match env::var("DISCORD_TOKEN") {
        Ok(token) => token,
        Err(error) => {
            tracing::error!("failed to fetch environment variable DISCORD_TOKEN\nError: {error:?}");
            exit(1);
        },
    };

    let kanatrans_host = match env::var("KANATRANS_HOST") {
        Ok(token) => token,
        Err(error) => {
            tracing::error!("failed to fetch environment variable KANATRANS_HOST\nError: {error:?}");
            exit(1);
        },
    };

    let kanatrans_port = match env::var("KANATRANS_PORT")
        .map_err(Error::from)
        .and_then(|port| port.parse::<u16>().map_err(Error::from))
    {
        Ok(token) => token,
        Err(error) => {
            tracing::error!("failed to fetch environment variable KANATRANS_PORT\nError: {error:?}");
            exit(1);
        },
    };

    let ss_direcotry = match env::var("SS_DIRECTORY") {
        Ok(ss_direcotry) => ss_direcotry,
        Err(error) => {
            tracing::error!("failed to fetch environment variable SS_DIRECTORY\nError: {error:?}");
            "".to_string()
        },
    };

    let pool = match set_up_database().await {
        Ok(pool) => pool,
        Err(error) => {
            tracing::error!("failed to set up postgres\nError: {error:?}");
            exit(1);
        },
    };

    let voicevox = match set_up_voicevox().await {
        Ok(voicevox) => voicevox,
        Err(error) => {
            tracing::error!("failed to set up voicevox client\nError: {error:?}");
            exit(1);
        },
    };

    let speaker = match Speaker::build(&voicevox).await {
        Ok(speaker) => speaker,
        Err(error) => {
            tracing::error!("failed to build speaker\nError: {error:?}");
            exit(1);
        },
    };

    let audio_repository = VoicevoxAudioRepository::new(
        voicevox.audio_generator.clone(),
        SongbirdAudioProcessor,
        ConstCacheable::<PredefinedUtterance>::new(),
    );

    if !ss_direcotry.is_empty() && !Path::new(&ss_direcotry).exists() {
        tracing::error!("{} is not exists.", ss_direcotry);
        exit(1);
    };
    let sounds: DashMap<OsString, Memory> = DashMap::new();
    if !ss_direcotry.is_empty() {
        for entry in WalkDir::new(ss_direcotry).into_iter().flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "mp3" || ext == "wav" || ext == "opus" || path.file_stem().is_some() {
                    let file = File::new(path.clone());
                    match Memory::new(file.into()).await {
                        Ok(memory) => {
                            sounds.insert(path.file_stem().unwrap().to_owned(), memory);
                        },
                        Err(error) => {
                            tracing::error!("{error:?}");
                            continue;
                        },
                    };
                }
            }
        }

        tracing::info!("{} files found!", sounds.len());
    };

    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    let mut client = match Client::builder(token, intents)
        .event_handler(event_handler::Handler {
            database: pool,
            speaker,
            audio_repository,
            connections: Arc::new(Mutex::new(HashMap::new())),
            kanatrans_host,
            kanatrans_port,
            sounds: Arc::new(sounds),
            rate_limiter: RateLimiter::new(2, 3, 20, 60, 1.5, 1),
        })
        .register_songbird()
        .await
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!("failed to build serenity client\nError: {error:?}");
            exit(1);
        },
    };

    {
        let mut data = client.data.write().await;

        data.insert::<VoicevoxClient>(Arc::new(Mutex::new(voicevox)));
    }

    tokio::spawn(async move {
        if let Err(error) = client.start().await {
            tracing::error!("failed to start client\nError: {error:?}");
            exit(1);
        }
    });

    wait_for_signal().await
}

async fn set_up_database() -> Result<PgPool> {
    let pg_options = PgConnectOptions::new()
        .log_statements(LevelFilter::Debug)
        .log_slow_statements(LevelFilter::Warn, Duration::from_millis(500));

    PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(pg_options)
        .await
        .map_err(Error::msg)
}

async fn set_up_voicevox() -> Result<Voicevox> {
    let voicevox_host = env::var("VOICEVOX_HOST").context("failed to fetch environment variable VOICEVOX_HOST")?;
    Voicevox::build(&voicevox_host).context("failed to build voicevox client")
}

pub(crate) async fn wait_for_signal() {
    wait_for_signal_impl().await
}

/// Waits for a signal that requests a graceful shutdown, like SIGTERM or SIGINT.
#[cfg(unix)]
async fn wait_for_signal_impl() {
    use tokio::signal::unix::{signal, SignalKind};

    // Infos here:
    // https://www.gnu.org/software/libc/manual/html_node/Termination-Signals.html
    let mut signal_terminate = signal(SignalKind::terminate()).unwrap();
    let mut signal_interrupt = signal(SignalKind::interrupt()).unwrap();

    tokio::select! {
        _ = signal_terminate.recv() => tracing::debug!("Received SIGTERM."),
        _ = signal_interrupt.recv() => tracing::debug!("Received SIGINT."),
    };
}

/// Waits for a signal that requests a graceful shutdown, Ctrl-C (SIGINT).
#[cfg(windows)]
async fn wait_for_signal_impl() {
    use tokio::signal::windows;

    // Infos here:
    // https://learn.microsoft.com/en-us/windows/console/handlerroutine
    let mut signal_c = windows::ctrl_c().unwrap();
    let mut signal_break = windows::ctrl_break().unwrap();
    let mut signal_close = windows::ctrl_close().unwrap();
    let mut signal_shutdown = windows::ctrl_shutdown().unwrap();

    tokio::select! {
        _ = signal_c.recv() => tracing::debug!("Received CTRL_C."),
        _ = signal_break.recv() => tracing::debug!("Received CTRL_BREAK."),
        _ = signal_close.recv() => tracing::debug!("Received CTRL_CLOSE."),
        _ = signal_shutdown.recv() => tracing::debug!("Received CTRL_SHUTDOWN."),
    };
}
