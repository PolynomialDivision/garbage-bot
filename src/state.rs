//! Persisted reminder state and the shared runtime state.

use super::*;

// ── Persistent state ─────────────────────────────────────────────────────────
//
// Survives container restarts via a JSON file on the `store` volume (already
// used for the Matrix session/crypto store, so it's already durable). Written
// atomically (tmp file + rename) so a kill mid-write can't corrupt it.

/// A reminder that has been decided (pickups + date) but may not yet be fully
/// delivered to every joined room. Persisted as soon as it's created — before
/// any send is attempted — so a crash between "decided" and "delivered" is
/// recovered on restart instead of silently losing the reminder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingReminder {
    pub(crate) date: NaiveDate,
    pub(crate) pickups: Vec<Pickup>,
    /// Room IDs (as strings) that have already received this reminder —
    /// checked before every send attempt so retries and restarts never
    /// duplicate a delivery.
    #[serde(default)]
    pub(crate) sent_room_ids: HashSet<String>,
    #[serde(default)]
    pub(crate) attempt: u32,
    /// When this reminder became due. Compared against `max_lateness` on
    /// every delivery attempt so a very old, still-unsent reminder is
    /// eventually given up on instead of retried forever. Defaults to "now"
    /// when missing (a state file written before this field existed), which
    /// gives a state-file upgrade a fresh grace period rather than expiring
    /// an in-flight reminder immediately.
    #[serde(default = "reminder_now_utc")]
    pub(crate) created_at: DateTime<chrono::Utc>,
}

/// The most recent reminder that reached at least one room: where it was
/// posted and who confirmed it. Persisted so ✅ taps keep working — and the
/// follow-up stays suppressed — across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PostedReminder {
    pub(crate) date: NaiveDate,
    pub(crate) pickups: Vec<Pickup>,
    /// Room ID → event ID of the reminder message in that room.
    pub(crate) event_ids: HashMap<String, String>,
    pub(crate) sent_at: DateTime<chrono::Utc>,
    #[serde(default)]
    pub(crate) confirmations: Vec<Confirmation>,
    #[serde(default)]
    pub(crate) followup_sent: bool,
    /// When the follow-up went out, so its wording can be re-rendered.
    #[serde(default)]
    pub(crate) followup_at: Option<DateTime<chrono::Utc>>,
    /// Room ID → event ID of the follow-up in that room.
    #[serde(default)]
    pub(crate) followup_event_ids: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Confirmation {
    pub(crate) user_id: String,
    pub(crate) name: String,
    pub(crate) at: DateTime<chrono::Utc>,
    /// The ✅-type reactions this user put on the reminder. Retracting the
    /// last one withdraws the confirmation.
    #[serde(default)]
    pub(crate) reaction_ids: Vec<String>,
}

impl PostedReminder {
    pub(crate) fn is_reminder_event(&self, event_id: &str) -> bool {
        self.event_ids.values().any(|id| id == event_id)
    }

    /// "Alice (21:14), Bob (21:20)" — who confirmed, in order.
    pub(crate) fn confirmed_by(&self) -> Option<String> {
        (!self.confirmations.is_empty()).then(|| {
            self.confirmations
                .iter()
                .map(|c| {
                    format!(
                        "{} ({})",
                        c.name,
                        c.at.with_timezone(&REMINDER_TZ).format("%H:%M")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PersistedState {
    #[serde(default)]
    pub(crate) bsr_cache: Vec<Pickup>,
    #[serde(default)]
    pub(crate) br_cache: Vec<Pickup>,
    /// The most recent date whose "tomorrow" reminder has already been
    /// evaluated (sent or found to have no pickups) — prevents refetching
    /// and re-deciding every scheduler tick after the target time has passed
    /// for the day.
    #[serde(default)]
    pub(crate) last_checked_date: Option<NaiveDate>,
    /// The most recent date whose reminder was *fully* delivered. Distinct
    /// from `last_checked_date` so an empty day never looks like a sent one.
    #[serde(default)]
    pub(crate) last_sent_date: Option<NaiveDate>,
    /// The most recent date whose reminder was given up on unsent after
    /// exceeding `max_lateness` — surfaced in `!garbage status` so a silent
    /// (but deliberate) non-delivery is still visible.
    #[serde(default)]
    pub(crate) last_expired_date: Option<NaiveDate>,
    #[serde(default)]
    pub(crate) pending_reminder: Option<PendingReminder>,
    #[serde(default)]
    pub(crate) posted: Option<PostedReminder>,
}

pub(crate) async fn load_persisted_state(path: &Path) -> PersistedState {
    match fs::read_to_string(path).await {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(state) => state,
            Err(e) => {
                let backup = path.with_file_name(format!(
                    "reminder_state.corrupt-{}.json",
                    chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
                ));
                match fs::rename(path, &backup).await {
                    Ok(()) => error!("Reminder state file at {path:?} is corrupt ({e}); moved it to {backup:?} and starting with empty state"),
                    Err(rename_error) => error!("Reminder state file at {path:?} is corrupt ({e}); failed to preserve it at {backup:?}: {rename_error}. Starting with empty state"),
                }
                PersistedState::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
        Err(e) => {
            error!("Failed to read reminder state file at {path:?} ({e}) — starting fresh.");
            PersistedState::default()
        }
    }
}

pub(crate) async fn save_persisted_state(path: &Path, state: &PersistedState) {
    if let Err(e) = mxbot_common::persist::save_json_atomic(path, state).await {
        error!("Failed to persist reminder state to {path:?}: {e:#}");
    }
}

/// Operational health of one schedule source, for `!garbage status`/`refresh`.
/// Intentionally not persisted — it's operational telemetry, not data that
/// needs to survive a restart (the pickup caches inside `PersistedState` are
/// what actually needs that guarantee).
#[derive(Debug, Clone, Default)]
pub(crate) struct SourceHealth {
    pub(crate) last_success: Option<DateTime<Tz>>,
    pub(crate) last_error: Option<String>,
    pub(crate) consecutive_failures: u32,
}

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct BotState {
    pub(crate) bot: Bot,
    pub(crate) bsr_schedule_id: String,
    pub(crate) berlin_recycling_creds: Option<(String, String)>,
    pub(crate) waste_labels: HashMap<String, String>,
    pub(crate) waste_colors: HashMap<String, String>,
    /// Reminder time and lateness, adjustable with `!admin set`.
    pub(crate) settings: Settings<ReminderSettings>,
    pub(crate) http: reqwest::Client,
    /// Path to the persisted reminder-state JSON file (not the store
    /// *directory* — see `reminder_state_path` in `main`), on the same
    /// bind-mounted volume as the Matrix session store so it survives
    /// container restarts/recreation.
    pub(crate) reminder_state_path: PathBuf,
    /// Serializes edits of the reminder message, so two quick ✅ taps cannot
    /// leave the older text as the final edit.
    pub(crate) edit_lock: Arc<Mutex<()>>,
    pub(crate) persisted: Arc<Mutex<PersistedState>>,
    pub(crate) bsr_health: Arc<Mutex<SourceHealth>>,
    pub(crate) br_health: Arc<Mutex<SourceHealth>>,
}

/// Runtime settings; defaults come from `[reminder]` in the config file.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct ReminderSettings {
    pub(crate) reminder_time: TimeOfDay,
    pub(crate) max_lateness_minutes: u32,
    pub(crate) followup_minutes: u32,
}

impl BotState {
    pub(crate) fn reminder_time(&self) -> NaiveTime {
        let t = self.settings.get().reminder_time;
        NaiveTime::from_hms_opt(t.hour, t.minute, 0).expect("TimeOfDay is validated")
    }

    pub(crate) fn max_lateness(&self) -> chrono::Duration {
        chrono::Duration::minutes(self.settings.get().max_lateness_minutes as i64)
    }

    pub(crate) fn followup_after(&self) -> Option<chrono::Duration> {
        let minutes = self.settings.get().followup_minutes;
        (minutes > 0).then(|| chrono::Duration::minutes(minutes as i64))
    }
}
