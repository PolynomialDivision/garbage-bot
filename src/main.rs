use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveTime, TimeZone};
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::filter::FilterDefinition,
        events::{
            reaction::{OriginalSyncReactionEvent, ReactionEventContent},
            relation::Annotation,
            room::{
                member::StrippedRoomMemberEvent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
            Mentions,
        },
        OwnedEventId, OwnedServerName, OwnedUserId, RoomOrAliasId,
    },
    Client, Room, RoomState,
};
use mxbot_common::config::{MatrixConfig, SecurityConfig};
use mxbot_common::verify::VerificationService;
use serde::{Deserialize, Serialize};
use tokio::{fs, sync::Mutex, time::sleep};
use tracing::{error, info, warn};
use url::Url;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Config {
    matrix: MatrixConfig,
    bsr: BsrConfig,
    #[serde(default)]
    berlin_recycling: Option<BerlinRecyclingConfig>,
    #[serde(default)]
    reminder: ReminderConfig,
    #[serde(default)]
    waste_labels: HashMap<String, String>,
    #[serde(default)]
    waste_colors: HashMap<String, String>,
    #[serde(default)]
    security: SecurityConfig,
}

#[derive(Deserialize)]
struct BerlinRecyclingConfig {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct BsrConfig {
    schedule_id: Option<String>,
    street: Option<String>,
    number: Option<String>,
    plz: Option<String>,
}

#[derive(Deserialize)]
struct ReminderConfig {
    #[serde(default = "default_reminder_time")]
    reminder_time: String,
    /// How long a reminder keeps retrying after becoming due before it's
    /// given up on as stale. A reminder says "put them out tonight" — once
    /// it's this late, that's no longer true, so delivering it would just be
    /// misleading rather than helpful.
    #[serde(default = "default_max_lateness_minutes")]
    max_lateness_minutes: u32,
}

fn default_reminder_time() -> String {
    "20:00".to_owned()
}

fn default_max_lateness_minutes() -> u32 {
    12 * 60 // 12h: e.g. a 20:00 reminder gives up at 08:00 the next morning.
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            reminder_time: default_reminder_time(),
            max_lateness_minutes: default_max_lateness_minutes(),
        }
    }
}

// ── BSR API ───────────────────────────────────────────────────────────────────

const BSR_PICKUPS: &str = "https://umnewforms.bsr.de/p/de.bsr.adressen.app/abfuhrEvents";
const BSR_STREETS: &str = "https://umnewforms.bsr.de/p/de.bsr.adressen.app/streetNames";
const BSR_SCHEDID: &str = "https://umnewforms.bsr.de/p/de.bsr.adressen.app/plzSet/plzSet";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pickup {
    date: NaiveDate,
    label: String,
}

#[derive(Deserialize)]
struct BsrStreetEntry {
    value: String,
}

#[derive(Deserialize)]
struct BsrScheduleEntry {
    value: String,
    label: String,
}

#[derive(Deserialize)]
struct BsrPickupResponse {
    #[serde(default)]
    dates: HashMap<String, Vec<BsrPickupEntry>>,
}

#[derive(Deserialize)]
struct BsrPickupEntry {
    #[serde(rename = "serviceDate_actual")]
    service_date: String,
    category: String,
}

async fn resolve_schedule_id(
    http: &reqwest::Client,
    street: &str,
    number: &str,
    plz: Option<&str>,
) -> Result<String> {
    let url = Url::parse_with_params(BSR_STREETS, &[("searchQuery", street)])?;
    let streets: Vec<BsrStreetEntry> = http.get(url).send().await?.json().await?;

    if streets.is_empty() {
        return Err(anyhow!("No BSR street found for: {street:?}"));
    }
    let matched = streets
        .iter()
        .find(|s| s.value.to_lowercase() == street.to_lowercase())
        .unwrap_or(&streets[0]);

    let url = Url::parse_with_params(
        BSR_SCHEDID,
        &[("searchQuery", format!("{}:::{}", matched.value, number))],
    )?;
    let results: Vec<BsrScheduleEntry> = http.get(url).send().await?.json().await?;

    if results.is_empty() {
        return Err(anyhow!(
            "No BSR address found for: {} {}",
            matched.value,
            number
        ));
    }
    if results.len() == 1 {
        return Ok(results[0].value.clone());
    }

    // Multiple results — log them all so the user can identify the right one.
    info!(
        "Multiple BSR addresses found for {} {}:",
        matched.value, number
    );
    for (i, e) in results.iter().enumerate() {
        info!("  [{i}] {} (id: {})", e.label, e.value);
    }

    // Prefer the entry whose label contains the configured PLZ.
    if let Some(plz) = plz {
        if let Some(entry) = results.iter().find(|e| e.label.contains(plz)) {
            return Ok(entry.value.clone());
        }
        warn!("PLZ {plz} not found in any result — falling back to first entry");
    }

    Ok(results[0].value.clone())
}

async fn fetch_pickups(
    http: &reqwest::Client,
    schedule_id: &str,
    waste_labels: &HashMap<String, String>,
) -> Result<Vec<Pickup>> {
    let now = Local::now();
    let filter = format!(
        "AddrKey eq '{}' and DateFrom eq datetime'{}-{:02}-01T00:00:00' and DateTo eq datetime'{}-{:02}-01T00:00:00'",
        schedule_id,
        now.year(), now.month(),
        now.year() + 1, now.month(),
    );

    let url = Url::parse_with_params(BSR_PICKUPS, &[("filter", &filter)])?;
    let resp: BsrPickupResponse = http.get(url).send().await?.json().await?;

    let mut pickups: Vec<Pickup> = resp
        .dates
        .values()
        .flatten()
        .filter_map(|e| {
            let date = NaiveDate::parse_from_str(&e.service_date, "%d.%m.%Y").ok()?;
            let label = waste_labels
                .get(&e.category)
                .cloned()
                .unwrap_or_else(|| format!("Unbekannt ({})", e.category));
            Some(Pickup { date, label })
        })
        .collect();

    pickups.sort_by_key(|p| p.date);
    Ok(pickups)
}

// ── Berlin Recycling API ──────────────────────────────────────────────────────

const BR_PORTAL: &str = "https://kundenportal.berlin-recycling.de/";

async fn fetch_berlin_recycling_pickups(
    username: &str,
    password: &str,
    waste_labels: &HashMap<String, String>,
) -> Result<Vec<Pickup>> {
    // Each call creates a fresh session — the portal is stateful via cookies.
    let http = reqwest::Client::builder().cookie_store(true).build()?;

    // Establish session
    http.get(BR_PORTAL).send().await?;

    // Login
    let resp = http
        .post(format!("{BR_PORTAL}Login.aspx/Auth"))
        .json(&serde_json::json!({
            "username": username,
            "password": password,
            "rememberMe": false,
            "encrypted": false,
        }))
        .send()
        .await?;
    resp.error_for_status()?;

    // Verify login didn't redirect away
    let check = http.get(format!("{BR_PORTAL}Default.aspx")).send().await?;
    if check.url().host_str() != Some("kundenportal.berlin-recycling.de") {
        return Err(anyhow!(
            "Berlin Recycling login failed — redirected to {}",
            check.url()
        ));
    }

    // Required portal handshake
    http.post(format!("{BR_PORTAL}Default.aspx/GetDashboard"))
        .header("Content-Type", "application/json")
        .send()
        .await?;

    // Fetch the collection calendar
    let resp = http
        .post(format!("{BR_PORTAL}Default.aspx/GetDatasetTableHead"))
        .json(&serde_json::json!({
            "datasettablecode": "ABFUHRKALENDER",
            "startindex": 0,
            "searchtext": "",
            "rangefilter": "[]",
            "ordername": "",
            "orderdir": "",
            "ClientParameters": "",
            "headrecid": "",
        }))
        .send()
        .await?;

    // Response is double-encoded: outer JSON has a "d" key containing a JSON string.
    let outer: serde_json::Value = resp.json().await?;
    let inner_str = outer["d"]
        .as_str()
        .ok_or_else(|| anyhow!("Berlin Recycling: unexpected response shape (missing 'd')"))?;
    let inner: serde_json::Value = serde_json::from_str(inner_str)?;

    let data = inner["Object"]["data"]
        .as_array()
        .ok_or_else(|| anyhow!("Berlin Recycling: no data array in response"))?;

    let mut pickups: Vec<Pickup> = data
        .iter()
        .filter_map(|d| {
            let date = NaiveDate::parse_from_str(d["Task Date"].as_str()?, "%Y-%m-%d").ok()?;
            let raw = d["Material Description"].as_str()?;
            // Strip leading category code like "5.01 " from the material description.
            let stripped = match raw.split_once(' ') {
                Some((code, rest))
                    if code.contains('.')
                        && code.chars().all(|c| c.is_ascii_digit() || c == '.') =>
                {
                    rest
                }
                _ => raw,
            };
            let label = waste_labels
                .get(stripped)
                .cloned()
                .unwrap_or_else(|| stripped.to_owned());
            Some(Pickup { date, label })
        })
        .collect();

    pickups.sort_by_key(|p| p.date);
    Ok(pickups)
}

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
struct PendingReminder {
    date: NaiveDate,
    pickups: Vec<Pickup>,
    /// Room IDs (as strings) that have already received this reminder —
    /// checked before every send attempt so retries and restarts never
    /// duplicate a delivery.
    #[serde(default)]
    sent_room_ids: HashSet<String>,
    #[serde(default)]
    attempt: u32,
    /// When this reminder became due. Compared against `max_lateness` on
    /// every delivery attempt so a very old, still-unsent reminder is
    /// eventually given up on instead of retried forever. Defaults to "now"
    /// when missing (a state file written before this field existed), which
    /// gives a state-file upgrade a fresh grace period rather than expiring
    /// an in-flight reminder immediately.
    #[serde(default = "Local::now")]
    created_at: DateTime<Local>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    bsr_cache: Vec<Pickup>,
    #[serde(default)]
    br_cache: Vec<Pickup>,
    /// The most recent date whose "tomorrow" reminder has already been
    /// evaluated (sent or found to have no pickups) — prevents refetching
    /// and re-deciding every scheduler tick after the target time has passed
    /// for the day.
    #[serde(default)]
    last_checked_date: Option<NaiveDate>,
    /// The most recent date whose reminder was *fully* delivered. Distinct
    /// from `last_checked_date` so an empty day never looks like a sent one.
    #[serde(default)]
    last_sent_date: Option<NaiveDate>,
    /// The most recent date whose reminder was given up on unsent after
    /// exceeding `max_lateness` — surfaced in `!garbage status` so a silent
    /// (but deliberate) non-delivery is still visible.
    #[serde(default)]
    last_expired_date: Option<NaiveDate>,
    #[serde(default)]
    pending_reminder: Option<PendingReminder>,
}

async fn load_persisted_state(path: &Path) -> PersistedState {
    match fs::read_to_string(path).await {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(state) => state,
            Err(e) => {
                error!("Reminder state file at {path:?} is corrupt ({e}) — starting fresh. Previously-cached schedules and any pending reminder are lost.");
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

async fn save_persisted_state(path: &Path, state: &PersistedState) {
    let json = match serde_json::to_string_pretty(state) {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to serialize reminder state: {e}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, &json).await {
        error!("Failed to write reminder state tmp file at {tmp:?}: {e}");
        return;
    }
    if let Err(e) = fs::rename(&tmp, path).await {
        error!("Failed to persist reminder state to {path:?}: {e}");
    }
}

/// Operational health of one schedule source, for `!garbage status`/`refresh`.
/// Intentionally not persisted — it's operational telemetry, not data that
/// needs to survive a restart (the pickup caches inside `PersistedState` are
/// what actually needs that guarantee).
#[derive(Debug, Clone, Default)]
struct SourceHealth {
    last_success: Option<DateTime<Local>>,
    last_error: Option<String>,
    consecutive_failures: u32,
}

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct BotState {
    bot_user_id: OwnedUserId,
    admin_users: HashSet<OwnedUserId>,
    allowed_inviters: HashSet<OwnedUserId>,
    verification: VerificationService,
    bsr_schedule_id: String,
    berlin_recycling_creds: Option<(String, String)>,
    waste_labels: HashMap<String, String>,
    waste_colors: HashMap<String, String>,
    reminder_time: NaiveTime,
    max_lateness: chrono::Duration,
    http: reqwest::Client,
    /// Path to the persisted reminder-state JSON file (not the store
    /// *directory* — see `reminder_state_path` in `main`), on the same
    /// bind-mounted volume as the Matrix session store so it survives
    /// container restarts/recreation.
    reminder_state_path: PathBuf,
    // reminder event_id -> set of display names that confirmed
    confirmed: Arc<Mutex<HashMap<OwnedEventId, HashSet<String>>>>,
    persisted: Arc<Mutex<PersistedState>>,
    bsr_health: Arc<Mutex<SourceHealth>>,
    br_health: Arc<Mutex<SourceHealth>>,
}

// ── Message formatting ────────────────────────────────────────────────────────

/// Map a `#RRGGBB` hex color to the closest colored-circle emoji.
fn hex_to_dot(hex: &str) -> &'static str {
    let s = hex.trim_start_matches('#');
    let Ok(n) = u32::from_str_radix(&s[..s.len().min(6)], 16) else {
        return "🗑️";
    };
    if s.len() < 6 {
        return "🗑️";
    }
    let r = ((n >> 16) & 0xff) as f32;
    let g = ((n >> 8) & 0xff) as f32;
    let b = (n & 0xff) as f32;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;

    if chroma < 40.0 {
        return "⚫";
    }

    let hue = if max == r {
        let h = 60.0 * (g - b) / chroma;
        if h < 0.0 {
            h + 360.0
        } else {
            h
        }
    } else if max == g {
        60.0 * ((b - r) / chroma + 2.0)
    } else {
        60.0 * ((r - g) / chroma + 4.0)
    };

    // Brown is dark orange (hue ~15–40°, low brightness).
    match hue as u32 {
        330..=360 | 0..=14 => "🔴",
        15..=40 if max < 180.0 => "🟤",
        15..=75 => "🟡",
        76..=165 => "🟢",
        166..=255 => "🔵",
        _ => "🟣",
    }
}

fn build_reminder(
    pickups: &[Pickup],
    pickup_date: NaiveDate,
    colors: &HashMap<String, String>,
) -> (String, String) {
    let day = pickup_date.format("%A, %d.%m.%Y").to_string();

    let plain_bins: String = pickups
        .iter()
        .map(|p| {
            let dot = colors.get(&p.label).map(|c| hex_to_dot(c)).unwrap_or("🗑️");
            format!("{dot} {}\n", p.label)
        })
        .collect();
    let html_bins: String = pickups
        .iter()
        .map(|p| match colors.get(&p.label) {
            Some(color) => format!(
                "🗑️ <font data-mx-color=\"{color}\"><strong>{}</strong></font><br>",
                p.label
            ),
            None => format!("🗑️ <strong>{}</strong><br>", p.label),
        })
        .collect();

    let plain = format!(
        "@room\n🗑️ Tomorrow ({day}):\n{plain_bins}Put them out tonight · click ✅ when done"
    );
    let html = format!(
        "@room<br>🗑️ Tomorrow <b>{day}</b>:<br>{html_bins}Put them out tonight · click ✅ when done"
    );
    (plain, html)
}

fn strip_variation_selectors(s: &str) -> String {
    s.chars()
        .filter(|&c| !('\u{FE00}'..='\u{FE0F}').contains(&c))
        .collect()
}

const DONE_REACTIONS: &[&str] = &["✅", "✔", "☑"];

// ── Core logic ────────────────────────────────────────────────────────────────

/// Merge freshly-fetched pickups from one source into the cache of
/// previously-seen upcoming ones for that source. Past entries (before
/// `today`) are dropped so the cache doesn't grow forever; everything else
/// survives even if `fetched` is empty (failed refresh) or no longer lists a
/// date the cache already had — a source is never allowed to make a known
/// pickup disappear, only for it to age out once its date has passed.
fn merge_pickup_cache(cache: &[Pickup], fetched: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
    let mut seen: HashSet<(NaiveDate, String)> = HashSet::new();
    let mut merged: Vec<Pickup> = cache
        .iter()
        .chain(fetched.iter())
        .filter(|p| p.date >= today)
        .filter(|p| seen.insert((p.date, p.label.clone())))
        .cloned()
        .collect();
    merged.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.label.cmp(&b.label)));
    merged
}

/// Fetch pickups from all configured sources, merge each into its persisted
/// cache, and return the combined, sorted result. Never fails outright: a
/// source that errors (network, parsing, auth) just keeps its last-known-good
/// cache instead of wiping it, and is reflected in that source's `SourceHealth`
/// for `!garbage status`/`refresh` rather than via an early return that would
/// have skipped every other source too.
/// Refresh one source's cache from the outcome of a fetch attempt: on success,
/// merge the fresh data and reset failure tracking; on failure, merge an empty
/// fetch (a no-op except pruning past dates — see `merge_pickup_cache`) and
/// record the error. Shared by BSR and Berlin Recycling so the "never wipe a
/// known-good schedule" behavior lives in exactly one place.
fn apply_fetch_outcome(
    source_name: &str,
    cache: &[Pickup],
    outcome: Result<Vec<Pickup>>,
    health: &mut SourceHealth,
    today: NaiveDate,
) -> Vec<Pickup> {
    match outcome {
        Ok(fetched) => {
            info!("{source_name}: {} pickup(s) fetched", fetched.len());
            health.last_success = Some(Local::now());
            health.last_error = None;
            health.consecutive_failures = 0;
            merge_pickup_cache(cache, &fetched, today)
        }
        Err(e) => {
            error!("{source_name}: schedule refresh failed, keeping previous schedule: {e}");
            health.last_error = Some(e.to_string());
            health.consecutive_failures += 1;
            merge_pickup_cache(cache, &[], today)
        }
    }
}

async fn fetch_all_pickups(state: &BotState) -> Vec<Pickup> {
    let today = Local::now().date_naive();
    let mut persisted = state.persisted.lock().await;

    let bsr_outcome = fetch_pickups(&state.http, &state.bsr_schedule_id, &state.waste_labels).await;
    persisted.bsr_cache = apply_fetch_outcome(
        "BSR",
        &persisted.bsr_cache,
        bsr_outcome,
        &mut *state.bsr_health.lock().await,
        today,
    );
    let mut pickups = persisted.bsr_cache.clone();

    if let Some((ref user, ref pass)) = state.berlin_recycling_creds {
        let br_outcome = fetch_berlin_recycling_pickups(user, pass, &state.waste_labels).await;
        persisted.br_cache = apply_fetch_outcome(
            "Berlin Recycling",
            &persisted.br_cache,
            br_outcome,
            &mut *state.br_health.lock().await,
            today,
        );
        pickups.extend(persisted.br_cache.iter().cloned());
    }

    pickups.sort_by_key(|p| p.date);
    save_persisted_state(&state.reminder_state_path, &persisted).await;
    pickups
}

/// All pickups scheduled on exactly `date`.
fn pickups_for_date(pickups: &[Pickup], date: NaiveDate) -> Vec<Pickup> {
    pickups.iter().filter(|p| p.date == date).cloned().collect()
}

/// Resolve a naive local wall-clock time against a timezone, handling the two
/// DST edge cases `TimeZone::from_local_datetime` can return:
/// - "spring forward" gap (e.g. 02:30 doesn't exist) — nudge forward until it
///   resolves, so a reminder time that falls in the gap fires slightly late
///   instead of never firing.
/// - "fall back" overlap (e.g. 02:30 happens twice) — pick the earlier
///   occurrence, so the reminder fires at the first opportunity.
fn resolve_local<Tz: TimeZone>(tz: &Tz, naive: chrono::NaiveDateTime) -> DateTime<Tz> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(earlier, _later) => earlier,
        LocalResult::None => {
            // DST gaps are at most a couple of hours; try a few nudges before
            // giving up (would only happen for a pathological reminder_time).
            (1..=4)
                .find_map(
                    |h| match tz.from_local_datetime(&(naive + chrono::Duration::hours(h))) {
                        LocalResult::Single(dt) => Some(dt),
                        _ => None,
                    },
                )
                .unwrap_or_else(|| {
                    tz.from_local_datetime(&naive)
                        .earliest()
                        .expect("timezone resolution")
                })
        }
    }
}

/// The next instant `target_time` occurs at or after `now`, in `now`'s
/// timezone — today if `target_time` hasn't passed yet, otherwise tomorrow.
/// DST-aware: computed via `resolve_local` rather than naive datetime
/// subtraction, which would be off by an hour on the two DST-transition days.
fn next_reminder_datetime<Tz: TimeZone>(now: DateTime<Tz>, target_time: NaiveTime) -> DateTime<Tz> {
    let today = now.date_naive();
    let candidate_date = if now.time() < target_time {
        today
    } else {
        today + chrono::Duration::days(1)
    };
    resolve_local(&now.timezone(), candidate_date.and_time(target_time))
}

/// Bounded exponential backoff for reminder-delivery retries: 5s, 10s, 20s...
/// capped at 5 minutes. Deliberately has no max-attempt cutoff — a reminder
/// must keep retrying until delivered, not give up and disappear.
fn backoff_delay(attempt: u32) -> Duration {
    let secs = 5u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_secs(secs.min(300))
}

/// Whether a pending reminder has been retrying for longer than
/// `max_lateness` — the bounded counterpart to `backoff_delay`'s unbounded
/// attempt count. Retries are unlimited *in number* but not in *time*: past
/// this point the reminder's "put them out tonight" content is actively
/// wrong, so delivering it would mislead rather than help.
fn is_pending_expired(
    pending: &PendingReminder,
    now: DateTime<Local>,
    max_lateness: chrono::Duration,
) -> bool {
    now.signed_duration_since(pending.created_at) > max_lateness
}

fn format_duration_minutes(d: chrono::Duration) -> String {
    let total_minutes = d.num_minutes().max(0);
    format!("{}h{:02}m", total_minutes / 60, total_minutes % 60)
}

/// Room IDs (as strings) that still need this reminder sent.
fn rooms_pending_delivery(pending: &PendingReminder, joined_room_ids: &[String]) -> Vec<String> {
    joined_room_ids
        .iter()
        .filter(|id| !pending.sent_room_ids.contains(*id))
        .cloned()
        .collect()
}

/// True once every currently-joined room has received the reminder. False
/// (not true) if there are no joined rooms at all — nothing has actually been
/// delivered yet, so it must not be mistaken for "done".
fn is_fully_delivered(pending: &PendingReminder, joined_room_ids: &[String]) -> bool {
    !joined_room_ids.is_empty()
        && joined_room_ids
            .iter()
            .all(|id| pending.sent_room_ids.contains(id))
}

/// Pure: whether a new reminder should be created right now, and for which
/// date. Returns `None` if it's not due yet, one is already pending, or
/// today's slot was already evaluated (sent or found empty).
fn should_start_new_reminder(
    now_date: NaiveDate,
    now_time: NaiveTime,
    target_time: NaiveTime,
    last_checked_date: Option<NaiveDate>,
    has_pending: bool,
) -> Option<NaiveDate> {
    if has_pending || now_time < target_time {
        return None;
    }
    let target_date = now_date + chrono::Duration::days(1);
    if last_checked_date == Some(target_date) {
        return None;
    }
    Some(target_date)
}

/// Send `plain`/`html` to one room, track it for ✅ confirmations, and seed a
/// ✅ reaction so users have an immediate button to tap. Shared by the
/// persisted/retried production path and the ephemeral `--test` path so
/// there's exactly one place that knows how to deliver a reminder message.
async fn deliver_reminder_to_room(
    state: &BotState,
    room: &Room,
    plain: &str,
    html: &str,
) -> matrix_sdk::Result<OwnedEventId> {
    let mut content = RoomMessageEventContent::text_html(plain.to_owned(), html.to_owned());
    let mut mentions = Mentions::new();
    mentions.room = true;
    content = content.add_mentions(mentions);
    let resp = room.send(content).await?;
    let event_id = resp.response.event_id;

    state
        .confirmed
        .lock()
        .await
        .insert(event_id.clone(), HashSet::new());

    let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), "✅".to_owned()));
    if let Err(e) = room.send(reaction).await {
        warn!(
            "Failed to seed confirmation reaction in {}: {e}",
            room.room_id()
        );
    }
    Ok(event_id)
}

/// Attempt to complete delivery of the persisted pending reminder, if any.
/// Safe to call repeatedly: rooms already in `sent_room_ids` are skipped, so
/// retries (and restart recovery, since `pending_reminder` is loaded straight
/// from disk) never duplicate a send. Only marks the reminder as sent once
/// every currently-joined room has it.
async fn resolve_pending_reminder(state: &BotState, client: &Client) {
    let pending = { state.persisted.lock().await.pending_reminder.clone() };
    let Some(mut pending) = pending else {
        return;
    };

    let now = Local::now();
    if is_pending_expired(&pending, now, state.max_lateness) {
        error!(
            "Reminder for {} expired unsent: still pending after {}, {}/{} room(s) confirmed, {} attempt(s) — giving up rather than deliver stale \"put them out tonight\" info",
            pending.date,
            format_duration_minutes(now - pending.created_at),
            pending.sent_room_ids.len(),
            client.joined_rooms().len(),
            pending.attempt
        );
        let mut persisted = state.persisted.lock().await;
        persisted.pending_reminder = None;
        persisted.last_expired_date = Some(pending.date);
        save_persisted_state(&state.reminder_state_path, &persisted).await;
        return;
    }

    let rooms = client.joined_rooms();
    let joined_ids: Vec<String> = rooms.iter().map(|r| r.room_id().to_string()).collect();
    let targets = rooms_pending_delivery(&pending, &joined_ids);

    if joined_ids.is_empty() {
        warn!(
            "Reminder for {} still pending — no joined rooms yet (attempt {})",
            pending.date,
            pending.attempt + 1
        );
    }

    let (plain, html) = build_reminder(&pending.pickups, pending.date, &state.waste_colors);
    for room in &rooms {
        let room_id = room.room_id().to_string();
        if !targets.contains(&room_id) {
            continue;
        }
        match deliver_reminder_to_room(state, room, &plain, &html).await {
            Ok(event_id) => {
                info!(
                    "Reminder delivered: room={room_id} date={} event_id={event_id}",
                    pending.date
                );
                pending.sent_room_ids.insert(room_id);
            }
            Err(e) => error!(
                "Reminder send failed: room={room_id} date={} attempt={} error={e}",
                pending.date,
                pending.attempt + 1
            ),
        }
    }

    let mut persisted = state.persisted.lock().await;
    if is_fully_delivered(&pending, &joined_ids) {
        info!("Reminder for {} fully delivered", pending.date);
        persisted.last_sent_date = Some(pending.date);
        persisted.pending_reminder = None;
    } else {
        pending.attempt += 1;
        let delay = backoff_delay(pending.attempt);
        warn!(
            "Reminder for {} not yet fully delivered, retrying in {}s (attempt {})",
            pending.date,
            delay.as_secs(),
            pending.attempt
        );
        persisted.pending_reminder = Some(pending);
    }
    save_persisted_state(&state.reminder_state_path, &persisted).await;
}

/// If a new reminder is due, fetch the schedule and persist it as pending
/// *before* attempting any send — so a crash right after this still leaves a
/// recoverable record on restart. `resolve_pending_reminder` does the actual
/// sending, whether called right after this or on a later retry/restart.
async fn maybe_start_reminder(state: &BotState, target_time: NaiveTime) {
    let now = Local::now();
    let (last_checked_date, has_pending) = {
        let persisted = state.persisted.lock().await;
        (
            persisted.last_checked_date,
            persisted.pending_reminder.is_some(),
        )
    };
    let Some(target_date) = should_start_new_reminder(
        now.date_naive(),
        now.time(),
        target_time,
        last_checked_date,
        has_pending,
    ) else {
        return;
    };

    let pickups = fetch_all_pickups(state).await;
    let day_pickups = pickups_for_date(&pickups, target_date);

    let mut persisted = state.persisted.lock().await;
    persisted.last_checked_date = Some(target_date);
    if day_pickups.is_empty() {
        info!("No pickups on {target_date}, skipping reminder.");
    } else {
        info!(
            "Reminder due for {target_date}: {} pickup(s)",
            day_pickups.len()
        );
        persisted.pending_reminder = Some(PendingReminder {
            date: target_date,
            pickups: day_pickups,
            sent_room_ids: HashSet::new(),
            attempt: 0,
            created_at: now,
        });
    }
    save_persisted_state(&state.reminder_state_path, &persisted).await;
}

async fn run_scheduler(state: BotState, client: Client) {
    let target_time = state.reminder_time;
    loop {
        // Order matters: create today's pending reminder first (if due), then
        // attempt delivery in the same tick — a freshly-created reminder
        // doesn't wait for the next tick just to make its first send attempt.
        maybe_start_reminder(&state, target_time).await;
        resolve_pending_reminder(&state, &client).await;

        let pending_attempt = {
            let persisted = state.persisted.lock().await;
            persisted.pending_reminder.as_ref().map(|p| p.attempt)
        };
        let sleep_dur = match pending_attempt {
            Some(attempt) => backoff_delay(attempt),
            None => {
                let now = Local::now();
                let target_dt = next_reminder_datetime(now, target_time);
                Duration::from_secs((target_dt - now).num_seconds().max(1) as u64)
            }
        };
        info!(
            "Scheduler sleeping {}s (pending={})",
            sleep_dur.as_secs(),
            pending_attempt.is_some()
        );
        sleep(sleep_dur).await;
    }
}

/// One-shot `--test` path: send the next upcoming reminder immediately and
/// don't loop. Deliberately does not touch `state.persisted`'s pending/sent
/// tracking — it's a manual dev trigger, not part of the daily production
/// cadence, and shouldn't affect what the real scheduler considers "already
/// sent".
async fn send_test_reminder(state: &BotState, client: &Client) {
    let pickups = fetch_all_pickups(state).await;
    let today = Local::now().date_naive();
    let Some(next_date) = pickups
        .iter()
        .filter(|p| p.date >= today)
        .map(|p| p.date)
        .min()
    else {
        warn!("No upcoming pickups found");
        return;
    };
    let day_pickups = pickups_for_date(&pickups, next_date);
    info!("Test mode: using pickup date {next_date}");

    // Wait up to 30s for the first sync to populate rooms.
    let mut tries = 0u32;
    let rooms = loop {
        let joined = client.joined_rooms();
        if !joined.is_empty() {
            break joined;
        }
        if tries >= 60 {
            warn!("No joined rooms after 30s — bot not invited yet?");
            return;
        }
        sleep(Duration::from_millis(500)).await;
        tries += 1;
    };

    let (plain, html) = build_reminder(&day_pickups, next_date, &state.waste_colors);
    for room in rooms {
        match deliver_reminder_to_room(state, &room, &plain, &html).await {
            Ok(event_id) => info!(
                "Sent test reminder to {}, event_id={}",
                room.room_id(),
                event_id
            ),
            Err(e) => error!("Failed to send test reminder to {}: {e}", room.room_id()),
        }
    }
}

// ── !garbage command ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GarbageCommand {
    /// `!garbage` — next upcoming pickup for each waste type.
    NextPerType,
    /// `!garbage next` — only the single nearest pickup event.
    NextEvent,
    /// `!garbage week` — every pickup in the next 7 days.
    Week,
    /// `!garbage status` — source health, reminder/scheduler state.
    Status,
    /// `!garbage refresh` (admin-only) — force a schedule refresh now.
    Refresh,
    /// `!garbage version` — deployed build version.
    Version,
    /// `!garbage help` (or an unrecognized subcommand) — usage text.
    Help,
}

fn parse_garbage_command(body: &str) -> Option<GarbageCommand> {
    let lower = body.trim().to_ascii_lowercase();
    if lower == "!garbage" {
        return Some(GarbageCommand::NextPerType);
    }
    let rest = lower.strip_prefix("!garbage ")?.trim();
    Some(match rest {
        "" => GarbageCommand::NextPerType,
        "next" => GarbageCommand::NextEvent,
        "week" => GarbageCommand::Week,
        "status" => GarbageCommand::Status,
        "refresh" => GarbageCommand::Refresh,
        "version" => GarbageCommand::Version,
        _ => GarbageCommand::Help,
    })
}

/// Remove duplicate (date, label) pairs — the BSR and Berlin Recycling feeds
/// can list the same pickup twice if their configured labels collide.
fn dedup_pickups(pickups: Vec<Pickup>) -> Vec<Pickup> {
    let mut seen: HashSet<(NaiveDate, String)> = HashSet::new();
    pickups
        .into_iter()
        .filter(|p| seen.insert((p.date, p.label.clone())))
        .collect()
}

/// Earliest upcoming date per waste type, sorted chronologically
/// (ties broken by label for a stable order).
fn next_pickup_per_type(pickups: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
    let mut next_by_label: HashMap<String, NaiveDate> = HashMap::new();
    for p in pickups {
        if p.date < today {
            continue;
        }
        next_by_label
            .entry(p.label.clone())
            .and_modify(|d| {
                if p.date < *d {
                    *d = p.date;
                }
            })
            .or_insert(p.date);
    }
    let mut result: Vec<Pickup> = next_by_label
        .into_iter()
        .map(|(label, date)| Pickup { date, label })
        .collect();
    result.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.label.cmp(&b.label)));
    result
}

/// All waste types collected on the single nearest upcoming date.
fn next_event(pickups: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
    let Some(next_date) = pickups
        .iter()
        .filter(|p| p.date >= today)
        .map(|p| p.date)
        .min()
    else {
        return Vec::new();
    };
    let mut entries: Vec<Pickup> = pickups
        .iter()
        .filter(|p| p.date == next_date)
        .cloned()
        .collect();
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    entries
}

/// All pickups in the next 7 days (today through today + 6), chronological.
fn pickups_this_week(pickups: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
    let end = today + chrono::Duration::days(6);
    let mut entries: Vec<Pickup> = pickups
        .iter()
        .filter(|p| p.date >= today && p.date <= end)
        .cloned()
        .collect();
    entries.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.label.cmp(&b.label)));
    entries
}

/// Compact date for mobile display, always including the weekday for easy
/// reading (`Fri, 24 Jul`), with the year appended only when it differs
/// from the current year.
fn format_relative_date(date: NaiveDate, today: NaiveDate) -> String {
    if date.year() == today.year() {
        date.format("%a, %-d %b").to_string()
    } else {
        date.format("%a, %-d %b %Y").to_string()
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn garbage_empty_message() -> (String, String) {
    let text = "🗑️ No upcoming collections found.".to_owned();
    (text.clone(), text)
}

fn garbage_help_message() -> (String, String) {
    let plain = "🗑️ !garbage — next per type\n\
                  !garbage next — next collection\n\
                  !garbage week — next 7 days\n\
                  !garbage status — source/reminder health\n\
                  !garbage refresh — force refresh (admin)\n\
                  !garbage version — build version"
        .to_owned();
    let html = plain.replace('\n', "<br>");
    (plain, html)
}

fn garbage_version_message() -> (String, String) {
    let text = format!("🗑️ garbage-bot v{}", env!("CARGO_PKG_VERSION"));
    (text.clone(), text)
}

/// One-line human summary of a source's health for `!garbage status`.
fn describe_source_health(health: &SourceHealth, cached_count: usize) -> String {
    match (&health.last_success, &health.last_error) {
        (Some(t), None) => format!(
            "ok · {cached_count} cached · refreshed {}",
            t.format("%d.%m %H:%M")
        ),
        (Some(t), Some(e)) => format!(
            "degraded ({e}) · {cached_count} cached from {} · {} failure(s) since",
            t.format("%d.%m %H:%M"),
            health.consecutive_failures
        ),
        (None, Some(e)) => format!("failing ({e}) · {cached_count} cached · never succeeded"),
        (None, None) => "not checked yet".to_owned(),
    }
}

/// One-line human summary of the reminder scheduler's current state.
fn describe_reminder_state(persisted: &PersistedState) -> String {
    let base = match &persisted.pending_reminder {
        Some(p) => format!(
            "pending for {} · attempt {} · {} room(s) confirmed",
            p.date,
            p.attempt,
            p.sent_room_ids.len()
        ),
        None => match persisted.last_sent_date {
            Some(d) => format!("last sent {d}"),
            None => "none sent yet".to_owned(),
        },
    };
    match persisted.last_expired_date {
        Some(d) => format!("{base} · ⚠ {d} expired unsent"),
        None => base,
    }
}

async fn build_status_message(state: &BotState) -> (String, String) {
    let persisted = state.persisted.lock().await;
    let bsr_health = state.bsr_health.lock().await.clone();

    let mut lines = vec!["🗑️ Status".to_owned()];
    lines.push(format!(
        "BSR: {}",
        describe_source_health(&bsr_health, persisted.bsr_cache.len())
    ));
    if state.berlin_recycling_creds.is_some() {
        let br_health = state.br_health.lock().await.clone();
        lines.push(format!(
            "Paper: {}",
            describe_source_health(&br_health, persisted.br_cache.len())
        ));
    }
    lines.push(format!("Reminder: {}", describe_reminder_state(&persisted)));

    let plain = lines.join("\n");
    let html = escape_html(&plain).replace('\n', "<br>");
    (plain, html)
}

/// Render a compact one-line-per-pickup list, optimized for small phone
/// screens: no table, no long heading, `·` as separator, waste-type emoji
/// derived from `waste_colors` (same mapping the daily reminder uses).
fn render_garbage_list(
    header: &str,
    entries: &[Pickup],
    today: NaiveDate,
    colors: &HashMap<String, String>,
) -> (String, String) {
    if entries.is_empty() {
        return garbage_empty_message();
    }

    let mut plain = format!("🗑️ {header}\n\n");
    let mut html = format!("🗑️ <b>{}</b><br><br>", escape_html(header));

    for (i, p) in entries.iter().enumerate() {
        let dot = colors.get(&p.label).map(|c| hex_to_dot(c)).unwrap_or("🗑️");
        let rel = format_relative_date(p.date, today);
        plain.push_str(&format!("{dot} {} · {rel}", p.label));
        html.push_str(&format!("{dot} {} · {rel}", escape_html(&p.label)));
        if i + 1 < entries.len() {
            plain.push('\n');
            html.push_str("<br>");
        }
    }

    (plain, html)
}

fn build_garbage_reply(
    cmd: GarbageCommand,
    pickups: &[Pickup],
    today: NaiveDate,
    colors: &HashMap<String, String>,
) -> (String, String) {
    match cmd {
        GarbageCommand::NextPerType => render_garbage_list(
            "Next collections",
            &next_pickup_per_type(pickups, today),
            today,
            colors,
        ),
        GarbageCommand::NextEvent => render_garbage_list(
            "Next collection",
            &next_event(pickups, today),
            today,
            colors,
        ),
        GarbageCommand::Week => render_garbage_list(
            "Next 7 days",
            &pickups_this_week(pickups, today),
            today,
            colors,
        ),
        GarbageCommand::Status | GarbageCommand::Refresh | GarbageCommand::Version => {
            unreachable!("handled directly in handle_garbage_command")
        }
        GarbageCommand::Help => garbage_help_message(),
    }
}

async fn handle_garbage_command(
    state: BotState,
    room: Room,
    cmd: GarbageCommand,
    sender: OwnedUserId,
) {
    let (plain, html) = match cmd {
        GarbageCommand::Help => garbage_help_message(),
        GarbageCommand::Version => garbage_version_message(),
        GarbageCommand::Status => build_status_message(&state).await,
        GarbageCommand::Refresh => {
            if !state.admin_users.contains(&sender) {
                let msg = "⛔ Admin only.".to_owned();
                (msg.clone(), msg)
            } else {
                info!("!garbage refresh requested by {sender}");
                fetch_all_pickups(&state).await;
                build_status_message(&state).await
            }
        }
        GarbageCommand::NextPerType | GarbageCommand::NextEvent | GarbageCommand::Week => {
            let pickups = dedup_pickups(fetch_all_pickups(&state).await);
            let today = Local::now().date_naive();
            build_garbage_reply(cmd, &pickups, today, &state.waste_colors)
        }
    };

    if let Err(e) = room
        .send(RoomMessageEventContent::text_html(plain, html))
        .await
    {
        error!("Failed to send !garbage reply: {e}");
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // matrix-sdk's own info/debug logging is very chatty (full sync bodies,
    // crypto internals); at that volume it can rotate our own — much rarer —
    // logs out of the container's capped log file before anyone reads them.
    // Keep our crate at `info` and everything else at `warn` by default,
    // same pattern as the other bots; RUST_LOG still overrides this.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "garbage_bot=info,matrix_sdk=warn".parse().unwrap()),
        )
        .init();

    let test_mode = std::env::args().any(|a| a == "--test");
    let config_path = std::env::args()
        .find(|a| a.ends_with(".toml"))
        .unwrap_or_else(|| "config.toml".to_owned());

    let config_str = fs::read_to_string(&config_path).await?;
    let mut config: Config = toml::from_str(&config_str)?;

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
                let http = reqwest::Client::new();
                let id = resolve_schedule_id(&http, street, number, plz).await?;
                info!("Resolved schedule_id: {id}");
                id
            }
        };

    let reminder_time_str = config.reminder.reminder_time.clone();
    let reminder_time = {
        let parts: Vec<&str> = reminder_time_str.splitn(2, ':').collect();
        let invalid =
            || anyhow!("Invalid reminder_time: {reminder_time_str} (expected HH:MM, 00:00-23:59)");
        if parts.len() != 2 {
            return Err(invalid());
        }
        let hour: u32 = parts[0].parse().map_err(|_| invalid())?;
        let minute: u32 = parts[1].parse().map_err(|_| invalid())?;
        NaiveTime::from_hms_opt(hour, minute, 0).ok_or_else(invalid)?
    };
    let max_lateness = chrono::Duration::minutes(config.reminder.max_lateness_minutes as i64);

    let store_path =
        PathBuf::from(std::env::var("STORE_PATH").unwrap_or_else(|_| "store".to_owned()));
    let (client, user_id) = mxbot_common::session::build_and_restore(
        &config.matrix,
        &store_path,
        config.security.encryption_strategy.into(),
    )
    .await?;

    let admin_users: HashSet<OwnedUserId> = config
        .security
        .admin_users
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    let allowed_inviters: HashSet<OwnedUserId> = config
        .security
        .allowed_inviters
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    let verification = VerificationService::allowlisted_tofu_from_config(
        client.clone(),
        &config.security.verification,
        &config.security.allowed_inviters,
    );
    verification.install_handlers();

    if admin_users.is_empty() {
        warn!("No admin_users configured — !reset-trust and !garbage refresh are disabled");
    } else {
        info!("Admin users: {admin_users:?}");
    }

    if allowed_inviters.is_empty() {
        warn!("No allowed_inviters configured — bot will accept invites from anyone");
    } else {
        info!("Allowed inviters: {allowed_inviters:?}");
    }

    let berlin_recycling_creds = config.berlin_recycling.map(|br| (br.username, br.password));
    if berlin_recycling_creds.is_some() {
        info!("Berlin Recycling credentials configured — paper pickups enabled");
    }

    let reminder_state_path = store_path.join("reminder_state.json");
    let persisted = load_persisted_state(&reminder_state_path).await;
    if let Some(ref pending) = persisted.pending_reminder {
        info!(
            "Restart recovery: found pending reminder for {} (attempt {}, {} room(s) already confirmed) — will resume delivery",
            pending.date,
            pending.attempt,
            pending.sent_room_ids.len()
        );
    }

    let state = BotState {
        bot_user_id: user_id,
        admin_users,
        allowed_inviters,
        verification,
        bsr_schedule_id: schedule_id,
        berlin_recycling_creds,
        waste_labels: config.waste_labels,
        waste_colors: config.waste_colors,
        reminder_time,
        max_lateness,
        http: reqwest::Client::new(),
        reminder_state_path,
        confirmed: Arc::new(Mutex::new(HashMap::new())),
        persisted: Arc::new(Mutex::new(persisted)),
        bsr_health: Arc::new(Mutex::new(SourceHealth::default())),
        br_health: Arc::new(Mutex::new(SourceHealth::default())),
    };

    // Auto-join invited rooms (only from allowed_inviters)
    client.add_event_handler({
        let state = state.clone();
        move |ev: StrippedRoomMemberEvent, room: Room, client: Client| {
            let state = state.clone();
            async move {
                if ev.state_key != state.bot_user_id {
                    return;
                }
                if !state.allowed_inviters.is_empty() && !state.allowed_inviters.contains(&ev.sender) {
                    warn!("Rejecting invite from {} (not in allowed_inviters)", ev.sender);
                    room.leave().await.ok();
                    return;
                }
                info!("Accepted invite from {} to {}", ev.sender, room.room_id());
                let room_id = room.room_id().to_owned();
                let mut via: Vec<OwnedServerName> = vec![ev.sender.server_name().to_owned()];
                if let Some(s) = room_id.server_name() {
                    let s = s.to_owned();
                    if !via.contains(&s) {
                        via.push(s);
                    }
                }
                let room_or_alias = match RoomOrAliasId::parse(room_id.as_str()) {
                    Ok(id) => id,
                    Err(e) => {
                        error!("Invalid room ID {room_id}: {e}");
                        return;
                    }
                };
                tokio::spawn(async move {
                    let mut delay = 2u64;
                    const MAX_ATTEMPTS: u32 = 8;
                    for attempt in 1..=MAX_ATTEMPTS {
                        match client.join_room_by_id_or_alias(&room_or_alias, &via).await {
                            Ok(_) => {
                                info!("Joined {room_id}");
                                return;
                            }
                            Err(ref e) if mxbot_common::verify::is_join_terminal(e) => {
                                warn!("Join failed (terminal) for {room_id}: {e}");
                                return;
                            }
                            Err(e) if attempt == MAX_ATTEMPTS => {
                                warn!("Join failed after {MAX_ATTEMPTS} attempts for {room_id}: {e}");
                            }
                            Err(e) => {
                                warn!("Join attempt {attempt}/{MAX_ATTEMPTS} failed for {room_id}: {e}; retry in {delay}s");
                                sleep(Duration::from_secs(delay)).await;
                                delay = (delay * 2).min(300);
                            }
                        }
                    }
                });
            }
        }
    });

    // In-room messages: verification requests are handled by mxbot-common.
    client.add_event_handler({
        let state = state.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room| {
            let state = state.clone();
            async move {
                if let MessageType::VerificationRequest(_) = &ev.content.msgtype {
                    return;
                }

                if ev.sender == state.bot_user_id || room.state() != RoomState::Joined {
                    return;
                }

                let MessageType::Text(ref text) = ev.content.msgtype else {
                    return;
                };
                let raw = text.body.trim();

                if state
                    .verification
                    .handle_admin_command(&ev.sender, &state.admin_users, raw)
                    .await
                {
                    return;
                }
                if let Some(cmd) = parse_garbage_command(raw) {
                    tokio::spawn(handle_garbage_command(
                        state.clone(),
                        room,
                        cmd,
                        ev.sender.clone(),
                    ));
                }
            }
        }
    });

    // Reactions
    client.add_event_handler({
        let state = state.clone();
        move |ev: OriginalSyncReactionEvent, room: Room| {
            let state = state.clone();
            async move {
                if room.state() != RoomState::Joined {
                    return;
                }
                tokio::spawn(handle_reaction(state, room, ev));
            }
        }
    });

    // Scheduler
    tokio::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            if test_mode {
                info!("Test mode: sending next upcoming reminder immediately");
                send_test_reminder(&state, &client).await;
                info!("Test reminder sent. Waiting for reactions (Ctrl+C to quit)...");
            } else {
                run_scheduler(state, client).await;
            }
        }
    });

    info!("Starting sync...");
    let filter = FilterDefinition::with_lazy_loading();
    client
        .sync_once(SyncSettings::default().filter(filter.clone().into()))
        .await?;

    // Drain pending invites from prior sessions.
    let invited = client.invited_rooms();
    if !invited.is_empty() {
        info!(
            "Pending invite(s) found after initial sync — joining {} room(s)",
            invited.len()
        );
        for room in invited {
            let room_id = room.room_id().to_owned();
            let via: Vec<OwnedServerName> = room_id
                .server_name()
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default();
            match RoomOrAliasId::parse(room_id.as_str()) {
                Ok(room_or_alias) => {
                    match client.join_room_by_id_or_alias(&room_or_alias, &via).await {
                        Ok(_) => info!("Joined pending invite room {room_id}"),
                        Err(e) => warn!("Failed to join pending invite room {room_id}: {e}"),
                    }
                }
                Err(e) => warn!("Invalid room ID in pending invite {room_id}: {e}"),
            }
        }
    }

    loop {
        match client
            .sync(SyncSettings::default().filter(filter.clone().into()))
            .await
        {
            Ok(()) => warn!("Sync loop exited cleanly — reconnecting"),
            Err(e) => warn!("Sync loop error: {e} — reconnecting in 5s"),
        }
        sleep(Duration::from_secs(5)).await;
    }
}

// ── Reaction handler ──────────────────────────────────────────────────────────

async fn handle_reaction(state: BotState, room: Room, ev: OriginalSyncReactionEvent) {
    // Ignore the bot's own seed reaction — it only exists to show users the button.
    if ev.sender == state.bot_user_id {
        return;
    }

    let related_id = &ev.content.relates_to.event_id;
    let key = strip_variation_selectors(&ev.content.relates_to.key);

    let mut confirmed = state.confirmed.lock().await;
    let Some(names) = confirmed.get_mut(related_id) else {
        return; // not a tracked reminder
    };

    if !DONE_REACTIONS.contains(&key.as_str()) {
        return;
    }

    let display_name = room
        .get_member(ev.sender.as_ref())
        .await
        .ok()
        .flatten()
        .and_then(|m| m.display_name().map(str::to_owned))
        .unwrap_or_else(|| ev.sender.to_string());

    if !names.insert(display_name.clone()) {
        return; // already confirmed
    }

    let count = names.len();
    let all_names = {
        let mut sorted: Vec<_> = names.iter().cloned().collect();
        sorted.sort();
        sorted.join(", ")
    };
    drop(confirmed);

    let msg = format!("✅ {display_name} put the bins out! ({count} confirmed: {all_names})");
    info!("Confirmation from {display_name} (total: {count})");

    if let Err(e) = room.send(RoomMessageEventContent::text_plain(msg)).await {
        error!("Failed to send confirmation: {e}");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 20).unwrap() // fixed: a Monday
    }

    fn colors() -> HashMap<String, String> {
        HashMap::from([
            ("Organic".to_owned(), "#8B4513".to_owned()),
            ("Paper".to_owned(), "#4682B4".to_owned()),
            ("Residual".to_owned(), "#808080".to_owned()),
            ("Recycling".to_owned(), "#DAA520".to_owned()),
        ])
    }

    fn pickup(days_from_today: i64, label: &str) -> Pickup {
        Pickup {
            date: today() + chrono::Duration::days(days_from_today),
            label: label.to_owned(),
        }
    }

    // ── Command parsing ───────────────────────────────────────────────────────

    #[test]
    fn parses_bare_command_as_next_per_type() {
        assert_eq!(
            parse_garbage_command("!garbage"),
            Some(GarbageCommand::NextPerType)
        );
        assert_eq!(
            parse_garbage_command("  !garbage  "),
            Some(GarbageCommand::NextPerType)
        );
        assert_eq!(
            parse_garbage_command("!garbage "),
            Some(GarbageCommand::NextPerType)
        );
    }

    #[test]
    fn parses_subcommands_case_insensitively() {
        assert_eq!(
            parse_garbage_command("!garbage next"),
            Some(GarbageCommand::NextEvent)
        );
        assert_eq!(
            parse_garbage_command("!GARBAGE NEXT"),
            Some(GarbageCommand::NextEvent)
        );
        assert_eq!(
            parse_garbage_command("!garbage week"),
            Some(GarbageCommand::Week)
        );
        assert_eq!(
            parse_garbage_command("!garbage help"),
            Some(GarbageCommand::Help)
        );
    }

    #[test]
    fn unknown_subcommand_falls_back_to_help() {
        assert_eq!(
            parse_garbage_command("!garbage bogus"),
            Some(GarbageCommand::Help)
        );
    }

    #[test]
    fn ignores_unrelated_messages() {
        assert_eq!(parse_garbage_command("hello"), None);
        assert_eq!(parse_garbage_command("!garbagefoo"), None);
        assert_eq!(parse_garbage_command("!reset-trust @x:y"), None);
    }

    // ── Relative date formatting ──────────────────────────────────────────────

    #[test]
    fn relative_date_always_shows_weekday_and_date() {
        assert_eq!(format_relative_date(today(), today()), "Mon, 20 Jul");
        assert_eq!(
            format_relative_date(today() + chrono::Duration::days(1), today()),
            "Tue, 21 Jul"
        );
        assert_eq!(
            format_relative_date(today() + chrono::Duration::days(6), today()),
            "Sun, 26 Jul"
        );
    }

    #[test]
    fn relative_date_further_out_uses_compact_date_without_year() {
        // today() + 7 days = 2026-07-27, same year as today() -> no year shown.
        let d = today() + chrono::Duration::days(7);
        assert_eq!(format_relative_date(d, today()), "Mon, 27 Jul");
    }

    #[test]
    fn relative_date_next_year_includes_year() {
        let d = NaiveDate::from_ymd_opt(2027, 1, 3).unwrap();
        assert_eq!(format_relative_date(d, today()), "Sun, 3 Jan 2027");
    }

    // ── Selection / grouping logic ────────────────────────────────────────────

    #[test]
    fn next_per_type_picks_earliest_future_date_per_label_and_sorts() {
        let pickups = vec![
            pickup(10, "Paper"),
            pickup(-1, "Residual"), // past — must be excluded
            pickup(1, "Organic"),
            pickup(3, "Paper"), // later than the 10-day one is irrelevant; earlier wins
            pickup(6, "Recycling"),
        ];
        let result = next_pickup_per_type(&pickups, today());
        let labels: Vec<&str> = result.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, vec!["Organic", "Paper", "Recycling"]);
        assert_eq!(result[1].date, today() + chrono::Duration::days(3));
    }

    #[test]
    fn next_per_type_excludes_labels_with_no_future_date() {
        let pickups = vec![pickup(-3, "Residual"), pickup(2, "Paper")];
        let result = next_pickup_per_type(&pickups, today());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "Paper");
    }

    #[test]
    fn next_per_type_today_counts_as_upcoming() {
        let pickups = vec![pickup(0, "Organic")];
        let result = next_pickup_per_type(&pickups, today());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn next_event_groups_same_day_multi_type() {
        let pickups = vec![
            pickup(2, "Organic"),
            pickup(2, "Paper"),
            pickup(5, "Residual"),
        ];
        let result = next_event(&pickups, today());
        let labels: Vec<&str> = result.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, vec!["Organic", "Paper"]);
    }

    #[test]
    fn next_event_empty_when_nothing_upcoming() {
        let pickups = vec![pickup(-2, "Organic")];
        assert!(next_event(&pickups, today()).is_empty());
    }

    #[test]
    fn week_includes_today_through_day_six_only() {
        let pickups = vec![
            pickup(0, "A"),
            pickup(6, "B"),
            pickup(7, "C"),
            pickup(-1, "D"),
        ];
        let result = pickups_this_week(&pickups, today());
        let labels: Vec<&str> = result.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, vec!["A", "B"]);
    }

    #[test]
    fn dedup_removes_identical_date_label_pairs() {
        let pickups = vec![pickup(2, "Paper"), pickup(2, "Paper"), pickup(2, "Organic")];
        let result = dedup_pickups(pickups);
        assert_eq!(result.len(), 2);
    }

    // ── Empty states ──────────────────────────────────────────────────────────

    #[test]
    fn empty_pickups_produce_empty_state_message() {
        let (plain, html) = render_garbage_list("Next collections", &[], today(), &colors());
        assert_eq!(plain, "🗑️ No upcoming collections found.");
        assert_eq!(html, "🗑️ No upcoming collections found.");
    }

    #[test]
    fn no_config_at_all_yields_empty_state_via_build_reply() {
        let (plain, _) = build_garbage_reply(GarbageCommand::NextPerType, &[], today(), &colors());
        assert_eq!(plain, "🗑️ No upcoming collections found.");
    }

    // ── Compact Matrix output ─────────────────────────────────────────────────

    #[test]
    fn render_is_compact_one_line_per_entry_no_table() {
        let pickups = vec![
            pickup(1, "Organic"),
            pickup(4, "Paper"),
            pickup(7, "Residual"),
        ];
        let (plain, html) = render_garbage_list("Next collections", &pickups, today(), &colors());

        assert!(plain.starts_with("🗑️ Next collections\n\n"));
        assert_eq!(plain.lines().count(), 5); // header + blank + 3 entries
        assert!(!plain.contains('|')); // no table-like formatting
        assert!(plain.contains("🟤 Organic · Tue, 21 Jul"));
        assert!(plain.contains("🔵 Paper · Fri, 24 Jul"));
        assert!(plain.contains("⚫ Residual · Mon, 27 Jul"));

        assert!(!html.contains("<table"));
        assert!(html.contains("<b>Next collections</b>"));
        assert!(html.matches("<br>").count() >= 4); // header break + between 3 entries
    }

    #[test]
    fn unknown_waste_color_falls_back_to_generic_trash_emoji() {
        let pickups = vec![pickup(1, "Mystery")];
        let (plain, _) = render_garbage_list("Next collections", &pickups, today(), &colors());
        assert!(plain.contains("🗑️ Mystery · Tue, 21 Jul"));
    }

    #[test]
    fn html_escapes_label_content() {
        let pickups = vec![pickup(1, "A & <B>")];
        let (_, html) = render_garbage_list("Next collections", &pickups, today(), &colors());
        assert!(html.contains("A &amp; &lt;B&gt;"));
        assert!(!html.contains("<B>"));
    }

    #[test]
    fn help_message_is_short_and_lists_all_subcommands() {
        let (plain, _) = garbage_help_message();
        assert!(plain.contains("!garbage"));
        assert!(plain.contains("next"));
        assert!(plain.contains("week"));
        assert!(plain.contains("status"));
        assert!(plain.contains("refresh"));
        assert!(plain.contains("version"));
        assert!(plain.len() < 350);
    }

    // ── Pickup cache merging ─────────────────────────────────────────────────
    // Regression coverage for a source dropping an imminent pickup from its
    // response a day or two before it happens (observed on the Berlin
    // Recycling portal): a date we've already seen once must keep showing up
    // even if a later fetch no longer lists it — for either source, since a
    // failed BSR refresh must not wipe a known-good BSR schedule either.

    #[test]
    fn merge_with_empty_cache_returns_fetched_future_dates() {
        let fetched = vec![pickup(1, "Paper"), pickup(8, "Paper")];
        let merged = merge_pickup_cache(&[], &fetched, today());
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_keeps_a_cached_date_the_source_stopped_listing() {
        // Yesterday's fetch saw tomorrow's pickup; today's fetch — because
        // the portal already dropped it — no longer includes it.
        let cache = vec![pickup(1, "Paper"), pickup(8, "Paper")];
        let fetched = vec![pickup(8, "Paper")]; // tomorrow's entry vanished from the source
        let merged = merge_pickup_cache(&cache, &fetched, today());

        assert_eq!(merged.len(), 2);
        assert!(merged
            .iter()
            .any(|p| p.date == today() + chrono::Duration::days(1)));
    }

    #[test]
    fn merge_drops_past_cached_entries_instead_of_resurrecting_them() {
        let cache = vec![pickup(-7, "Paper"), pickup(1, "Paper")];
        let merged = merge_pickup_cache(&cache, &[], today());

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].date, today() + chrono::Duration::days(1));
    }

    #[test]
    fn merge_deduplicates_a_date_present_in_both_cache_and_fetch() {
        let cache = vec![pickup(1, "Paper")];
        let fetched = vec![pickup(1, "Paper")];
        let merged = merge_pickup_cache(&cache, &fetched, today());
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn merge_on_failed_fetch_falls_back_to_cache() {
        // fetch_all_pickups passes an empty slice on a failed Berlin Recycling
        // fetch; previously-cached upcoming dates must still come through.
        let cache = vec![pickup(1, "Paper"), pickup(8, "Paper")];
        let merged = merge_pickup_cache(&cache, &[], today());
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_result_is_sorted_by_date_then_label() {
        let cache = vec![pickup(8, "Residual"), pickup(1, "Paper")];
        let fetched = vec![pickup(1, "Organic")];
        let merged = merge_pickup_cache(&cache, &fetched, today());

        let dates: Vec<_> = merged.iter().map(|p| (p.date, p.label.as_str())).collect();
        assert_eq!(
            dates,
            vec![
                (today() + chrono::Duration::days(1), "Organic"),
                (today() + chrono::Duration::days(1), "Paper"),
                (today() + chrono::Duration::days(8), "Residual"),
            ]
        );
    }

    #[test]
    fn pickups_for_date_filters_exact_match_only() {
        let pickups = vec![pickup(0, "A"), pickup(1, "B"), pickup(1, "C")];
        let result = pickups_for_date(&pickups, today() + chrono::Duration::days(1));
        let labels: Vec<&str> = result.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, vec!["B", "C"]);
    }

    // ── Reminder scheduling: DST-aware timing ─────────────────────────────────
    // Uses chrono-tz's real IANA rules for Europe/Berlin so these are exact and
    // don't depend on the host machine's configured timezone. This is the
    // reliability fix for a real bug: computing the next reminder instant via
    // naive datetime subtraction is off by exactly one hour on the two annual
    // DST-transition days, because it silently assumes every day is 24h.

    #[test]
    fn next_reminder_datetime_same_day_before_target() {
        use chrono_tz::Europe::Berlin;
        // 2026-07-20 is a normal summer (CEST) day, no DST transition.
        let now = Berlin.with_ymd_and_hms(2026, 7, 20, 10, 0, 0).unwrap();
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let next = next_reminder_datetime(now, target);
        assert_eq!(
            next,
            Berlin.with_ymd_and_hms(2026, 7, 20, 20, 0, 0).unwrap()
        );
    }

    #[test]
    fn next_reminder_datetime_rolls_to_tomorrow_after_target() {
        use chrono_tz::Europe::Berlin;
        let now = Berlin.with_ymd_and_hms(2026, 7, 20, 20, 0, 1).unwrap();
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let next = next_reminder_datetime(now, target);
        assert_eq!(
            next,
            Berlin.with_ymd_and_hms(2026, 7, 21, 20, 0, 0).unwrap()
        );
    }

    #[test]
    fn next_reminder_datetime_correct_across_spring_forward() {
        use chrono_tz::Europe::Berlin;
        // Europe/Berlin springs forward 2026-03-29 02:00 CET -> 03:00 CEST.
        // A naive-subtraction implementation would compute one hour too many
        // (treating the 23-hour day as 24h), firing the reminder an hour late.
        let now = Berlin.with_ymd_and_hms(2026, 3, 28, 20, 0, 0).unwrap();
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let next = next_reminder_datetime(now, target);
        let expected = Berlin.with_ymd_and_hms(2026, 3, 29, 20, 0, 0).unwrap();
        assert_eq!(next, expected);
        assert_eq!((next - now).num_minutes(), 23 * 60); // 23h day, not 24h
    }

    #[test]
    fn next_reminder_datetime_correct_across_fall_back() {
        use chrono_tz::Europe::Berlin;
        // Europe/Berlin falls back 2026-10-25 03:00 CEST -> 02:00 CET (25h day).
        let now = Berlin.with_ymd_and_hms(2026, 10, 24, 20, 0, 0).unwrap();
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let next = next_reminder_datetime(now, target);
        let expected = Berlin.with_ymd_and_hms(2026, 10, 25, 20, 0, 0).unwrap();
        assert_eq!(next, expected);
        assert_eq!((next - now).num_minutes(), 25 * 60); // 25h day, not 24h
    }

    #[test]
    fn resolve_local_picks_earlier_occurrence_in_fall_back_overlap() {
        use chrono::Offset;
        use chrono_tz::Europe::Berlin;
        // 02:30 on 2026-10-25 happens twice (CEST then CET); must resolve to
        // the first occurrence so a reminder due in that window fires ASAP.
        let naive = NaiveDate::from_ymd_opt(2026, 10, 25)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        let resolved = resolve_local(&Berlin, naive);
        assert_eq!(resolved.naive_local(), naive);
        assert_eq!(resolved.offset().fix().local_minus_utc(), 2 * 3600); // still CEST
    }

    #[test]
    fn resolve_local_nudges_forward_through_spring_forward_gap() {
        use chrono_tz::Europe::Berlin;
        // 02:30 on 2026-03-29 never happens (clocks jump 02:00 -> 03:00).
        let naive = NaiveDate::from_ymd_opt(2026, 3, 29)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        let resolved = resolve_local(&Berlin, naive);
        // Must resolve to *something* after the gap, not panic or go backwards.
        assert!(resolved.naive_local() >= naive);
        assert_eq!(resolved.naive_local().date(), naive.date());
    }

    // ── Reminder scheduling: backoff and due-check ────────────────────────────

    #[test]
    fn backoff_delay_is_bounded_exponential() {
        assert_eq!(backoff_delay(0), Duration::from_secs(5));
        assert_eq!(backoff_delay(1), Duration::from_secs(10));
        assert_eq!(backoff_delay(2), Duration::from_secs(20));
        assert_eq!(backoff_delay(10), Duration::from_secs(300)); // capped, never gives up
    }

    #[test]
    fn should_not_start_before_target_time() {
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let now_time = NaiveTime::from_hms_opt(19, 59, 59).unwrap();
        assert_eq!(
            should_start_new_reminder(today(), now_time, target, None, false),
            None
        );
    }

    #[test]
    fn should_start_exactly_at_target_time_for_tomorrow() {
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let result = should_start_new_reminder(today(), target, target, None, false);
        assert_eq!(result, Some(today() + chrono::Duration::days(1)));
    }

    #[test]
    fn should_not_start_a_second_pending_reminder() {
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        assert_eq!(
            should_start_new_reminder(today(), target, target, None, true),
            None
        );
    }

    #[test]
    fn should_not_restart_a_reminder_already_checked_today() {
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let tomorrow = today() + chrono::Duration::days(1);
        assert_eq!(
            should_start_new_reminder(today(), target, target, Some(tomorrow), false),
            None
        );
    }

    #[test]
    fn should_start_again_once_the_date_rolls_over() {
        // last_checked_date is yesterday's target — a new day means a new check.
        let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
        let stale_checked_date = today(); // was "tomorrow" relative to yesterday
        let result =
            should_start_new_reminder(today(), target, target, Some(stale_checked_date), false);
        assert_eq!(result, Some(today() + chrono::Duration::days(1)));
    }

    // ── Reminder delivery: duplicate prevention across rooms/restarts ────────

    fn pending(sent: &[&str], attempt: u32) -> PendingReminder {
        pending_at(sent, attempt, Local::now())
    }

    fn pending_at(sent: &[&str], attempt: u32, created_at: DateTime<Local>) -> PendingReminder {
        PendingReminder {
            date: today() + chrono::Duration::days(1),
            pickups: vec![pickup(1, "Organic")],
            sent_room_ids: sent.iter().map(|s| s.to_string()).collect(),
            attempt,
            created_at,
        }
    }

    #[test]
    fn rooms_pending_delivery_excludes_already_sent_rooms() {
        let p = pending(&["!a:x"], 0);
        let joined = vec!["!a:x".to_owned(), "!b:x".to_owned()];
        assert_eq!(rooms_pending_delivery(&p, &joined), vec!["!b:x".to_owned()]);
    }

    #[test]
    fn rooms_pending_delivery_empty_once_all_rooms_sent() {
        let p = pending(&["!a:x", "!b:x"], 2);
        let joined = vec!["!a:x".to_owned(), "!b:x".to_owned()];
        assert!(rooms_pending_delivery(&p, &joined).is_empty());
    }

    #[test]
    fn is_fully_delivered_true_only_when_every_joined_room_has_it() {
        let joined = vec!["!a:x".to_owned(), "!b:x".to_owned()];
        assert!(!is_fully_delivered(&pending(&["!a:x"], 1), &joined));
        assert!(is_fully_delivered(&pending(&["!a:x", "!b:x"], 1), &joined));
    }

    #[test]
    fn is_fully_delivered_false_with_no_joined_rooms() {
        // Not "vacuously delivered" — there must be an actual room that got it.
        assert!(!is_fully_delivered(&pending(&[], 0), &[]));
    }

    #[test]
    fn is_fully_delivered_survives_restart_via_persisted_sent_room_ids() {
        // Simulates: room A succeeded before a restart, room B is new/retried
        // after. sent_room_ids (loaded straight from disk) must remember A.
        let p = pending(&["!a:x"], 3);
        let joined = vec!["!a:x".to_owned(), "!b:x".to_owned()];
        assert_eq!(rooms_pending_delivery(&p, &joined), vec!["!b:x".to_owned()]);
        assert!(!is_fully_delivered(&p, &joined));
    }

    // ── Persisted state: load/save round trip and corruption handling ────────

    #[tokio::test]
    async fn persisted_state_missing_file_yields_default() {
        let dir = std::env::temp_dir().join(format!("garbage-bot-test-{}", uuid_ish()));
        let path = dir.join("reminder_state.json");
        let state = load_persisted_state(&path).await;
        assert!(state.bsr_cache.is_empty());
        assert!(state.pending_reminder.is_none());
    }

    #[tokio::test]
    async fn persisted_state_corrupt_file_yields_default_not_panic() {
        let dir = std::env::temp_dir().join(format!("garbage-bot-test-{}", uuid_ish()));
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("reminder_state.json");
        fs::write(&path, b"{ not valid json").await.unwrap();

        let state = load_persisted_state(&path).await;
        assert!(state.bsr_cache.is_empty());
        assert!(state.pending_reminder.is_none());
    }

    #[tokio::test]
    async fn persisted_state_round_trips_through_save_and_load() {
        let dir = std::env::temp_dir().join(format!("garbage-bot-test-{}", uuid_ish()));
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("reminder_state.json");

        let created_at = Local::now();
        let original = PersistedState {
            bsr_cache: vec![pickup(1, "Organic")],
            last_sent_date: Some(today()),
            last_expired_date: Some(today() - chrono::Duration::days(3)),
            pending_reminder: Some(pending_at(&["!a:x"], 2, created_at)),
            ..Default::default()
        };

        save_persisted_state(&path, &original).await;
        let loaded = load_persisted_state(&path).await;

        assert_eq!(loaded.bsr_cache, original.bsr_cache);
        assert_eq!(loaded.last_sent_date, original.last_sent_date);
        assert_eq!(loaded.last_expired_date, original.last_expired_date);
        let loaded_pending = loaded.pending_reminder.expect("pending reminder");
        assert_eq!(loaded_pending.date, pending(&["!a:x"], 2).date);
        assert_eq!(loaded_pending.attempt, 2);
        assert!(loaded_pending.sent_room_ids.contains("!a:x"));
        // RFC3339 round-trips to millisecond precision or better.
        assert!(
            (loaded_pending.created_at - created_at)
                .num_milliseconds()
                .abs()
                < 1000
        );
    }

    #[tokio::test]
    async fn persisted_state_migrates_pre_pending_reminder_fields_format() {
        // Simulates a state file written before `created_at`/`last_expired_date`
        // existed. Everything but the new fields must still load correctly.
        let dir = std::env::temp_dir().join(format!("garbage-bot-test-{}", uuid_ish()));
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("reminder_state.json");
        let old_format = format!(
            r#"{{
                "bsr_cache": [{{"date": "{}", "label": "Residual"}}],
                "br_cache": [],
                "last_checked_date": "{}",
                "last_sent_date": null,
                "pending_reminder": {{
                    "date": "{}",
                    "pickups": [{{"date": "{}", "label": "Residual"}}],
                    "sent_room_ids": ["!a:x"],
                    "attempt": 1
                }}
            }}"#,
            today(),
            today(),
            today() + chrono::Duration::days(1),
            today() + chrono::Duration::days(1),
        );
        fs::write(&path, old_format).await.unwrap();

        let loaded = load_persisted_state(&path).await;
        assert_eq!(loaded.bsr_cache.len(), 1);
        assert_eq!(loaded.last_expired_date, None); // new field, absent -> default
        let pending = loaded
            .pending_reminder
            .expect("pending reminder should load");
        assert_eq!(pending.attempt, 1);
        assert!(pending.sent_room_ids.contains("!a:x"));
        // created_at is new — defaults to "now" rather than failing to parse
        // or silently expiring an in-flight reminder on the next tick.
        assert!((Local::now() - pending.created_at).num_seconds().abs() < 5);
    }

    // ── Reminder lateness cap ─────────────────────────────────────────────────

    #[test]
    fn is_pending_expired_false_within_max_lateness() {
        let max_lateness = chrono::Duration::hours(12);
        let now = Local::now();
        let p = pending_at(&[], 3, now - chrono::Duration::hours(11));
        assert!(!is_pending_expired(&p, now, max_lateness));
    }

    #[test]
    fn is_pending_expired_true_beyond_max_lateness() {
        let max_lateness = chrono::Duration::hours(12);
        let now = Local::now();
        let p = pending_at(&[], 20, now - chrono::Duration::hours(13));
        assert!(is_pending_expired(&p, now, max_lateness));
    }

    #[test]
    fn is_pending_expired_fresh_reminder_never_expired() {
        let max_lateness = chrono::Duration::minutes(0); // pathological but valid config
        let now = Local::now();
        let p = pending_at(&[], 0, now);
        assert!(!is_pending_expired(&p, now, max_lateness)); // exactly at the boundary, not over it
    }

    // ── Source-fetch outcome handling ─────────────────────────────────────────

    #[test]
    fn apply_fetch_outcome_success_merges_cache_and_clears_health_error() {
        let mut health = SourceHealth {
            last_error: Some("previous failure".to_owned()),
            consecutive_failures: 3,
            ..Default::default()
        };
        let cache = vec![pickup(1, "Organic")];
        let fetched = Ok(vec![pickup(2, "Paper")]);
        let merged = apply_fetch_outcome("BSR", &cache, fetched, &mut health, today());

        assert_eq!(merged.len(), 2);
        assert!(health.last_error.is_none());
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.last_success.is_some());
    }

    #[test]
    fn apply_fetch_outcome_failure_preserves_cache_and_records_error() {
        let mut health = SourceHealth::default();
        let cache = vec![pickup(1, "Organic"), pickup(2, "Paper")];
        let fetched: Result<Vec<Pickup>> = Err(anyhow!("dns error"));
        let merged = apply_fetch_outcome("BSR", &cache, fetched, &mut health, today());

        assert_eq!(merged, cache); // known-good schedule untouched
        assert_eq!(health.last_error.as_deref(), Some("dns error"));
        assert_eq!(health.consecutive_failures, 1);
        assert!(health.last_success.is_none());
    }

    fn uuid_ish() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
