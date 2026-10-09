mod commands;
mod config;
mod confirm;
mod message;
mod schedule;
mod sources;
mod state;
#[cfg(test)]
mod tests;

use commands::*;
use config::*;
use confirm::*;
use message::*;
use schedule::*;
use sources::*;
use state::*;

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, LocalResult, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;
use futures_util::FutureExt;
use mxbot_common::{
    admin::Dispatch,
    config::{MatrixConfig, SecurityConfig, TimeOfDay},
    matrix_sdk::{
        self,
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::{
                reaction::{OriginalSyncReactionEvent, ReactionEventContent},
                relation::Annotation,
                relation::Reply,
                room::message::{
                    MessageType, OriginalSyncRoomMessageEvent, Relation, ReplacementMetadata,
                    RoomMessageEventContent,
                },
                room::redaction::OriginalSyncRoomRedactionEvent,
                Mentions,
            },
            OwnedEventId, OwnedTransactionId, OwnedUserId,
        },
        Room, RoomState,
    },
    settings::Settings,
    Bot,
};
use serde::{Deserialize, Serialize};
use tokio::{fs, sync::Mutex, time::sleep};
use tracing::{error, info, warn};
use url::Url;

/// Garbage collection dates and reminder wall-clock times are local Berlin
/// times. Do not use `chrono::Local`: containers frequently retain an UTC
/// `/etc/localtime` even when a `TZ` environment variable is present.
const REMINDER_TZ: Tz = chrono_tz::Europe::Berlin;

fn reminder_now() -> DateTime<Tz> {
    chrono::Utc::now().with_timezone(&REMINDER_TZ)
}

fn reminder_now_utc() -> DateTime<chrono::Utc> {
    reminder_now().to_utc()
}

/// Matrix handlers run independently from the sync loop. Do not let a panic
/// in one disappear silently while the process otherwise stays healthy.
fn spawn_logged<F>(name: &'static str, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        if std::panic::AssertUnwindSafe(future)
            .catch_unwind()
            .await
            .is_err()
        {
            error!(task = name, "Background task panicked");
        }
    });
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("garbage_bot");

    let test_mode = std::env::args().any(|a| a == "--test");
    let mut config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;

    // Resolve BSR schedule_id if only street/number given
    let schedule_id =
        match config.bsr.schedule_id.take() {
            Some(id) => id,
            None => {
                let street =
                    config.bsr.street.as_deref().ok_or_else(|| {
                        anyhow!("Config: set [bsr] schedule_id OR street + number")
                    })?;
                let number =
                    config.bsr.number.as_deref().ok_or_else(|| {
                        anyhow!("Config: set [bsr] schedule_id OR street + number")
                    })?;
                let plz = config.bsr.plz.as_deref();
                info!(
                    "Resolving BSR schedule_id for {street} {number}{}...",
                    plz.map(|p| format!(", PLZ {p}")).unwrap_or_default()
                );
                let http = http_client();
                let id = resolve_schedule_id(&http, street, number, plz).await?;
                info!("Resolved schedule_id: {id}");
                id
            }
        };

    let store_path = mxbot_common::config::store_path_from_env();
    let settings = Settings::load(
        store_path.join("settings.json"),
        ReminderSettings {
            reminder_time: config.reminder.reminder_time,
            max_lateness_minutes: config.reminder.max_lateness_minutes,
            followup_minutes: config.reminder.followup_minutes,
        },
    )
    .await?;

    let bot = Bot::builder("garbage-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .settings(settings.store().clone())
        .admin_help(
            "!garbage status · !garbage refresh\n\
             Settings: reminder_time (HH:MM, Europe/Berlin), max_lateness_minutes,\n\
             followup_minutes (nudge when nobody confirmed; 0 = off)",
        )
        .start(&config.matrix, &config.security)
        .await?;

    let berlin_recycling_creds = config.berlin_recycling.map(|br| (br.username, br.password));
    if berlin_recycling_creds.is_some() {
        info!("Berlin Recycling credentials configured, paper pickups enabled");
    }

    let reminder_state_path = store_path.join("reminder_state.json");
    let persisted = load_persisted_state(&reminder_state_path).await;
    if let Some(ref pending) = persisted.pending_reminder {
        info!(
            "Restart recovery: found pending reminder for {} (attempt {}, {} room(s) already confirmed): will resume delivery",
            pending.date,
            pending.attempt,
            pending.sent_room_ids.len()
        );
    }

    let state = BotState {
        bot: bot.clone(),
        bsr_schedule_id: schedule_id,
        berlin_recycling_creds,
        waste_labels: config.waste_labels,
        waste_colors: config.waste_colors,
        settings,
        http: http_client(),
        reminder_state_path,
        edit_lock: Arc::new(Mutex::new(())),
        persisted: Arc::new(Mutex::new(persisted)),
        bsr_health: Arc::new(Mutex::new(SourceHealth::default())),
        br_health: Arc::new(Mutex::new(SourceHealth::default())),
    };

    // In-room messages: admin console first, then !garbage commands.
    bot.client.add_event_handler({
        let state = state.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| {
            let state = state.clone();
            async move {
                if ev.sender == state.bot.user_id || room.state() != RoomState::Joined {
                    return;
                }
                if state
                    .bot
                    .admin
                    .handle(&room, &ev, encryption.as_ref())
                    .await
                    == Dispatch::Handled
                {
                    return;
                }

                let MessageType::Text(ref text) = ev.content.msgtype else {
                    return;
                };
                if let Some(cmd) = parse_garbage_command(text.body.trim()) {
                    spawn_logged(
                        "garbage-command",
                        handle_garbage_command(state.clone(), room, cmd, ev.sender.clone()),
                    );
                }
            }
        }
    });

    // Reactions
    bot.client.add_event_handler({
        let state = state.clone();
        move |ev: OriginalSyncReactionEvent, room: Room| {
            let state = state.clone();
            async move {
                if room.state() != RoomState::Joined {
                    return;
                }
                spawn_logged("reaction-handler", handle_reaction(state, room, ev));
            }
        }
    });

    // Retracted reactions
    bot.client.add_event_handler({
        let state = state.clone();
        move |ev: OriginalSyncRoomRedactionEvent, room: Room| {
            let state = state.clone();
            async move {
                if room.state() != RoomState::Joined || ev.sender == state.bot.user_id {
                    return;
                }
                spawn_logged("redaction-handler", handle_redaction(state, ev));
            }
        }
    });

    // Scheduler
    spawn_logged("reminder-scheduler", {
        let state = state.clone();
        async move {
            if test_mode {
                info!("Test mode: sending next upcoming reminder immediately");
                send_test_reminder(&state).await;
                info!("Test reminder sent. Waiting for reactions (Ctrl+C to quit)...");
            } else {
                run_scheduler(state).await;
            }
        }
    });

    info!("Starting Matrix sync...");
    bot.initial_sync().await;
    bot.run().await
}
