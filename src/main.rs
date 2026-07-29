use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use chrono::{Datelike, Local, NaiveDate, NaiveTime};
use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedServerName, OwnedUserId, RoomOrAliasId,
        api::client::filter::FilterDefinition,
        events::{
            Mentions,
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            reaction::{OriginalSyncReactionEvent, ReactionEventContent},
            relation::Annotation,
            room::{
                member::StrippedRoomMemberEvent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
    },
};
use mxbot_common::config::{MatrixConfig, SecurityConfig};
use serde::Deserialize;
use url::Url;
use tokio::{fs, sync::Mutex, time::sleep};
use tracing::{error, info, warn};

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
}

fn default_reminder_time() -> String {
    "20:00".to_owned()
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self { reminder_time: default_reminder_time() }
    }
}

// ── BSR API ───────────────────────────────────────────────────────────────────

const BSR_PICKUPS: &str =
    "https://umnewforms.bsr.de/p/de.bsr.adressen.app/abfuhrEvents";
const BSR_STREETS: &str =
    "https://umnewforms.bsr.de/p/de.bsr.adressen.app/streetNames";
const BSR_SCHEDID: &str =
    "https://umnewforms.bsr.de/p/de.bsr.adressen.app/plzSet/plzSet";

#[derive(Debug, Clone)]
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

async fn resolve_schedule_id(http: &reqwest::Client, street: &str, number: &str, plz: Option<&str>) -> Result<String> {
    let url = Url::parse_with_params(BSR_STREETS, &[("searchQuery", street)])?;
    let streets: Vec<BsrStreetEntry> = http.get(url).send().await?.json().await?;

    if streets.is_empty() {
        return Err(anyhow!("No BSR street found for: {street:?}"));
    }
    let matched = streets
        .iter()
        .find(|s| s.value.to_lowercase() == street.to_lowercase())
        .unwrap_or(&streets[0]);

    let url = Url::parse_with_params(BSR_SCHEDID, &[("searchQuery", format!("{}:::{}", matched.value, number))])?;
    let results: Vec<BsrScheduleEntry> = http.get(url).send().await?.json().await?;

    if results.is_empty() {
        return Err(anyhow!("No BSR address found for: {} {}", matched.value, number));
    }
    if results.len() == 1 {
        return Ok(results[0].value.clone());
    }

    // Multiple results — log them all so the user can identify the right one.
    info!("Multiple BSR addresses found for {} {}:", matched.value, number);
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

async fn fetch_berlin_recycling_pickups(username: &str, password: &str, waste_labels: &HashMap<String, String>) -> Result<Vec<Pickup>> {
    // Each call creates a fresh session — the portal is stateful via cookies.
    let http = reqwest::Client::builder()
        .cookie_store(true)
        .build()?;

    // Establish session
    http.get(BR_PORTAL).send().await?;

    // Login
    let resp = http.post(format!("{BR_PORTAL}Login.aspx/Auth"))
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
        return Err(anyhow!("Berlin Recycling login failed — redirected to {}", check.url()));
    }

    // Required portal handshake
    http.post(format!("{BR_PORTAL}Default.aspx/GetDashboard"))
        .header("Content-Type", "application/json")
        .send()
        .await?;

    // Fetch the collection calendar
    let resp = http.post(format!("{BR_PORTAL}Default.aspx/GetDatasetTableHead"))
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
    let inner_str = outer["d"].as_str()
        .ok_or_else(|| anyhow!("Berlin Recycling: unexpected response shape (missing 'd')"))?;
    let inner: serde_json::Value = serde_json::from_str(inner_str)?;

    let data = inner["Object"]["data"].as_array()
        .ok_or_else(|| anyhow!("Berlin Recycling: no data array in response"))?;

    let mut pickups: Vec<Pickup> = data.iter()
        .filter_map(|d| {
            let date = NaiveDate::parse_from_str(d["Task Date"].as_str()?, "%Y-%m-%d").ok()?;
            let raw = d["Material Description"].as_str()?;
            // Strip leading category code like "5.01 " from the material description.
            let stripped = match raw.split_once(' ') {
                Some((code, rest)) if code.contains('.') && code.chars().all(|c| c.is_ascii_digit() || c == '.') => rest,
                _ => raw,
            };
            let label = waste_labels.get(stripped).cloned().unwrap_or_else(|| stripped.to_owned());
            Some(Pickup { date, label })
        })
        .collect();

    pickups.sort_by_key(|p| p.date);
    Ok(pickups)
}

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct BotState {
    bot_user_id: OwnedUserId,
    admin_users: HashSet<OwnedUserId>,
    allowed_inviters: HashSet<OwnedUserId>,
    reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>>,
    bsr_schedule_id: String,
    berlin_recycling_creds: Option<(String, String)>,
    waste_labels: HashMap<String, String>,
    waste_colors: HashMap<String, String>,
    reminder_hour: u32,
    reminder_minute: u32,
    http: reqwest::Client,
    // reminder event_id -> set of display names that confirmed
    confirmed: Arc<Mutex<HashMap<OwnedEventId, HashSet<String>>>>,
    // Last-known upcoming Berlin Recycling pickups. The portal removes an
    // imminent pickup from its calendar a day or two before it happens
    // (presumably once the collection route is finalized), so a same-day
    // fetch can silently lose it. We keep every future date we've ever
    // seen and merge it back in, so a pickup we already confirmed once
    // doesn't disappear just because the source stopped listing it.
    br_cache: Arc<Mutex<Vec<Pickup>>>,
}

// ── Message formatting ────────────────────────────────────────────────────────

/// Map a `#RRGGBB` hex color to the closest colored-circle emoji.
fn hex_to_dot(hex: &str) -> &'static str {
    let s = hex.trim_start_matches('#');
    let Ok(n) = u32::from_str_radix(&s[..s.len().min(6)], 16) else {
        return "🗑️";
    };
    if s.len() < 6 { return "🗑️"; }
    let r = ((n >> 16) & 0xff) as f32;
    let g = ((n >>  8) & 0xff) as f32;
    let b = ( n        & 0xff) as f32;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;

    if chroma < 40.0 { return "⚫"; }

    let hue = if max == r {
        let h = 60.0 * (g - b) / chroma;
        if h < 0.0 { h + 360.0 } else { h }
    } else if max == g {
        60.0 * ((b - r) / chroma + 2.0)
    } else {
        60.0 * ((r - g) / chroma + 4.0)
    };

    // Brown is dark orange (hue ~15–40°, low brightness).
    match hue as u32 {
        330..=360 | 0..=14              => "🔴",
        15..=40 if max < 180.0          => "🟤",
        15..=75                         => "🟡",
        76..=165                        => "🟢",
        166..=255                       => "🔵",
        _                               => "🟣",
    }
}

fn build_reminder(pickups: &[Pickup], pickup_date: NaiveDate, colors: &HashMap<String, String>) -> (String, String) {
    let day = pickup_date.format("%A, %d.%m.%Y").to_string();

    let plain_bins: String = pickups.iter().map(|p| {
        let dot = colors.get(&p.label).map(|c| hex_to_dot(c)).unwrap_or("🗑️");
        format!("{dot} {}\n", p.label)
    }).collect();
    let html_bins: String = pickups.iter().map(|p| {
        match colors.get(&p.label) {
            Some(color) => format!("🗑️ <font data-mx-color=\"{color}\"><strong>{}</strong></font><br>", p.label),
            None        => format!("🗑️ <strong>{}</strong><br>", p.label),
        }
    }).collect();

    let plain = format!(
        "@room\n🗑️ Tomorrow ({day}):\n{plain_bins}Put them out tonight · click ✅ when done"
    );
    let html = format!(
        "@room<br>🗑️ Tomorrow <b>{day}</b>:<br>{html_bins}Put them out tonight · click ✅ when done"
    );
    (plain, html)
}

fn strip_variation_selectors(s: &str) -> String {
    s.chars().filter(|&c| !('\u{FE00}'..='\u{FE0F}').contains(&c)).collect()
}

const DONE_REACTIONS: &[&str] = &["✅", "✔", "☑"];

// ── Core logic ────────────────────────────────────────────────────────────────

/// Merge freshly-fetched Berlin Recycling pickups with the cache of
/// previously-seen upcoming ones. Past entries (before `today`) are dropped
/// from the cache so it doesn't grow forever; everything else survives even
/// if `fetched` no longer contains it.
fn merge_berlin_recycling(cache: &[Pickup], fetched: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
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

/// Fetch and merge pickups from all configured sources, sorted by date.
/// A BSR failure is fatal (it's the primary source); a Berlin Recycling
/// failure is logged and skipped so the reminder still fires with BSR data.
///
/// The Berlin Recycling portal drops an imminent pickup from its calendar
/// response a day or two before it happens (seemingly once the collection
/// route is finalized), so a same-day fetch can silently lose it. We merge
/// each fetch into `state.br_cache` and keep reporting a date we've already
/// seen until it's in the past, even if a later fetch no longer lists it.
async fn fetch_all_pickups(state: &BotState) -> Result<Vec<Pickup>> {
    let mut pickups = fetch_pickups(&state.http, &state.bsr_schedule_id, &state.waste_labels).await?;
    let today = Local::now().date_naive();

    if let Some((ref user, ref pass)) = state.berlin_recycling_creds {
        let mut cache = state.br_cache.lock().await;
        match fetch_berlin_recycling_pickups(user, pass, &state.waste_labels).await {
            Ok(br_pickups) => {
                info!("Berlin Recycling: {} pickup(s) fetched", br_pickups.len());
                *cache = merge_berlin_recycling(&cache, &br_pickups, today);
            }
            Err(e) => {
                error!("Failed to fetch Berlin Recycling data: {e}");
                *cache = merge_berlin_recycling(&cache, &[], today);
                if !cache.is_empty() {
                    warn!("Berlin Recycling: falling back to {} cached pickup(s)", cache.len());
                }
            }
        }
        pickups.extend(cache.iter().cloned());
        pickups.sort_by_key(|p| p.date);
    }

    Ok(pickups)
}

async fn check_and_remind(state: &BotState, client: &Client, test_mode: bool) {
    let pickups = match fetch_all_pickups(state).await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to fetch BSR data: {e}");
            return;
        }
    };

    let today = Local::now().date_naive();
    let tomorrow = today + chrono::Duration::days(1);

    let (pickup_date, day_pickups) = if test_mode {
        let future: Vec<_> = pickups.iter().filter(|p| p.date >= today).collect();
        if future.is_empty() {
            warn!("No upcoming pickups found");
            return;
        }
        let next_date = future[0].date;
        let on_day: Vec<Pickup> = pickups.iter().filter(|p| p.date == next_date).cloned().collect();
        info!("Test mode: using pickup date {next_date}");
        (next_date, on_day)
    } else {
        let on_day: Vec<Pickup> = pickups.iter().filter(|p| p.date == tomorrow).cloned().collect();
        (tomorrow, on_day)
    };

    if day_pickups.is_empty() {
        info!("No pickups on {pickup_date}, skipping reminder.");
        return;
    }

    // Wait up to 30s for the first sync to populate rooms (needed in --test mode)
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

    let (plain, html) = build_reminder(&day_pickups, pickup_date, &state.waste_colors);
    for room in rooms {
        let mut content = RoomMessageEventContent::text_html(plain.clone(), html.clone());
        let mut mentions = Mentions::new();
        mentions.room = true;
        content = content.add_mentions(mentions);
        match room.send(content).await {
            Ok(resp) => {
                let event_id = resp.response.event_id;
                info!("Sent reminder to {}, event_id={}", room.room_id(), event_id);
                state.confirmed.lock().await.insert(event_id.clone(), HashSet::new());
                // React with ✅ so users see a button they can click immediately.
                let reaction = ReactionEventContent::new(
                    Annotation::new(event_id, "✅".to_owned()),
                );
                if let Err(e) = room.send(reaction).await {
                    warn!("Failed to react to reminder: {e}");
                }
            }
            Err(e) => error!("Failed to send reminder to {}: {e}", room.room_id()),
        }
    }
}

async fn scheduler_loop(state: BotState, client: Client, test_mode: bool) {
    if test_mode {
        info!("Test mode: sending next upcoming reminder immediately");
        check_and_remind(&state, &client, true).await;
        info!("Test reminder sent. Waiting for reactions (Ctrl+C to quit)...");
        return;
    }

    loop {
        let now = Local::now();
        let target = NaiveTime::from_hms_opt(state.reminder_hour, state.reminder_minute, 0)
            .expect("invalid reminder time");
        let today = now.date_naive();
        let next_dt = if now.time() < target {
            today.and_time(target)
        } else {
            (today + chrono::Duration::days(1)).and_time(target)
        };
        let secs = (next_dt - now.naive_local()).num_seconds().max(0) as u64;
        info!(
            "Next reminder in {secs}s (at {:02}:{:02})",
            state.reminder_hour, state.reminder_minute
        );
        sleep(Duration::from_secs(secs)).await;
        check_and_remind(&state, &client, false).await;
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
    let Some(next_date) = pickups.iter().filter(|p| p.date >= today).map(|p| p.date).min() else {
        return Vec::new();
    };
    let mut entries: Vec<Pickup> = pickups.iter().filter(|p| p.date == next_date).cloned().collect();
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

/// Compact relative date for mobile display: `today` / `tomorrow` / `in N days`
/// for the coming week, otherwise a short localized date (`Fri, 24 Jul`),
/// with the year appended only when it differs from the current year.
fn format_relative_date(date: NaiveDate, today: NaiveDate) -> String {
    let days = (date - today).num_days();
    match days {
        0 => "today".to_owned(),
        1 => "tomorrow".to_owned(),
        2..=6 => format!("in {days} days"),
        _ if date.year() == today.year() => date.format("%a, %-d %b").to_string(),
        _ => date.format("%a, %-d %b %Y").to_string(),
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
                  !garbage week — next 7 days"
        .to_owned();
    let html = plain.replace('\n', "<br>");
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

fn build_garbage_reply(cmd: GarbageCommand, pickups: &[Pickup], today: NaiveDate, colors: &HashMap<String, String>) -> (String, String) {
    match cmd {
        GarbageCommand::NextPerType => render_garbage_list("Next collections", &next_pickup_per_type(pickups, today), today, colors),
        GarbageCommand::NextEvent => render_garbage_list("Next collection", &next_event(pickups, today), today, colors),
        GarbageCommand::Week => render_garbage_list("Next 7 days", &pickups_this_week(pickups, today), today, colors),
        GarbageCommand::Help => garbage_help_message(),
    }
}

async fn handle_garbage_command(state: BotState, room: Room, cmd: GarbageCommand) {
    let (plain, html) = if cmd == GarbageCommand::Help {
        garbage_help_message()
    } else {
        match fetch_all_pickups(&state).await {
            Ok(pickups) => {
                let pickups = dedup_pickups(pickups);
                let today = Local::now().date_naive();
                build_garbage_reply(cmd, &pickups, today, &state.waste_colors)
            }
            Err(e) => {
                error!("!garbage: failed to fetch pickups: {e}");
                garbage_empty_message()
            }
        }
    };

    if let Err(e) = room.send(RoomMessageEventContent::text_html(plain, html)).await {
        error!("Failed to send !garbage reply: {e}");
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let test_mode = std::env::args().any(|a| a == "--test");
    let config_path = std::env::args()
        .find(|a| a.ends_with(".toml"))
        .unwrap_or_else(|| "config.toml".to_owned());

    let config_str = fs::read_to_string(&config_path).await?;
    let mut config: Config = toml::from_str(&config_str)?;

    // Resolve BSR schedule_id if only street/number given
    let schedule_id = match config.bsr.schedule_id.take() {
        Some(id) => id,
        None => {
            let street = config.bsr.street.as_deref()
                .ok_or_else(|| anyhow!("Config: set [bsr] schedule_id OR street + number"))?;
            let number = config.bsr.number.as_deref()
                .ok_or_else(|| anyhow!("Config: set [bsr] schedule_id OR street + number"))?;
            let plz = config.bsr.plz.as_deref();
            info!("Resolving BSR schedule_id for {street} {number}{}...",
                plz.map(|p| format!(", PLZ {p}")).unwrap_or_default());
            let http = reqwest::Client::new();
            let id = resolve_schedule_id(&http, street, number, plz).await?;
            info!("Resolved schedule_id: {id}");
            id
        }
    };

    let reminder_time = config.reminder.reminder_time.clone();
    let (reminder_hour, reminder_minute) = {
        let parts: Vec<&str> = reminder_time.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(anyhow!("Invalid reminder_time: {reminder_time}"));
        }
        (parts[0].parse::<u32>()?, parts[1].parse::<u32>()?)
    };

    let store_path = PathBuf::from(
        std::env::var("STORE_PATH").unwrap_or_else(|_| "store".to_owned()),
    );
    let (client, user_id) = mxbot_common::session::build_and_restore(
        &config.matrix,
        &store_path,
        config.security.encryption_strategy.into(),
    ).await?;

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

    if admin_users.is_empty() {
        warn!("No admin_users configured — !reset-trust command is disabled");
    } else {
        info!("Admin users: {admin_users:?}");
    }

    if allowed_inviters.is_empty() {
        warn!("No allowed_inviters configured — bot will accept invites from anyone");
    } else {
        info!("Allowed inviters: {allowed_inviters:?}");
    }

    let berlin_recycling_creds = config.berlin_recycling
        .map(|br| (br.username, br.password));
    if berlin_recycling_creds.is_some() {
        info!("Berlin Recycling credentials configured — paper pickups enabled");
    }

    let state = BotState {
        bot_user_id: user_id,
        admin_users,
        allowed_inviters,
        reset_allowed: Arc::new(Mutex::new(HashSet::new())),
        bsr_schedule_id: schedule_id,
        berlin_recycling_creds,
        waste_labels: config.waste_labels,
        waste_colors: config.waste_colors,
        reminder_hour,
        reminder_minute,
        http: reqwest::Client::new(),
        confirmed: Arc::new(Mutex::new(HashMap::new())),
        br_cache: Arc::new(Mutex::new(Vec::new())),
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

    // To-device verification requests
    client.add_event_handler({
        let state = state.clone();
        move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let state = state.clone();
            async move {
                let Some(request) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                else {
                    warn!("to-device verification request object not found");
                    return;
                };
                tokio::spawn(mxbot_common::verify::handle_verification_request(
                    client, Arc::clone(&state.reset_allowed), request,
                ));
            }
        }
    });

    // In-room messages: verification requests and admin commands
    client.add_event_handler({
        let state = state.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            let state = state.clone();
            async move {
                if let MessageType::VerificationRequest(_) = &ev.content.msgtype {
                    let Some(request) = client
                        .encryption()
                        .get_verification_request(&ev.sender, &ev.event_id)
                        .await
                    else {
                        warn!("in-room verification request object not found");
                        return;
                    };
                    tokio::spawn(mxbot_common::verify::handle_verification_request(
                    client, Arc::clone(&state.reset_allowed), request,
                ));
                    return;
                }

                if ev.sender == state.bot_user_id || room.state() != RoomState::Joined {
                    return;
                }

                let MessageType::Text(ref text) = ev.content.msgtype else { return };
                let raw = text.body.trim();

                if let Some(target) = raw.strip_prefix("!reset-trust ") {
                    if state.admin_users.contains(&ev.sender) {
                        match target.trim().parse::<OwnedUserId>() {
                            Ok(target_user) => {
                                state.reset_allowed.lock().await.insert(target_user.clone());
                                info!("Trust reset allowed for {} (by {})", target_user, ev.sender);
                            }
                            Err(_) => warn!("!reset-trust: invalid user ID '{}'", target.trim()),
                        }
                    } else {
                        warn!("!reset-trust from non-admin {} — ignored", ev.sender);
                    }
                } else if let Some(cmd) = parse_garbage_command(raw) {
                    tokio::spawn(handle_garbage_command(state.clone(), room, cmd));
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
    tokio::spawn(scheduler_loop(state.clone(), client.clone(), test_mode));

    info!("Starting sync...");
    let filter = FilterDefinition::with_lazy_loading();
    client
        .sync_once(SyncSettings::default().filter(filter.clone().into()))
        .await?;

    // Drain pending invites from prior sessions.
    let invited = client.invited_rooms();
    if !invited.is_empty() {
        info!("Pending invite(s) found after initial sync — joining {} room(s)", invited.len());
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
        match client.sync(SyncSettings::default().filter(filter.clone().into())).await {
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
        Pickup { date: today() + chrono::Duration::days(days_from_today), label: label.to_owned() }
    }

    // ── Command parsing ───────────────────────────────────────────────────────

    #[test]
    fn parses_bare_command_as_next_per_type() {
        assert_eq!(parse_garbage_command("!garbage"), Some(GarbageCommand::NextPerType));
        assert_eq!(parse_garbage_command("  !garbage  "), Some(GarbageCommand::NextPerType));
        assert_eq!(parse_garbage_command("!garbage "), Some(GarbageCommand::NextPerType));
    }

    #[test]
    fn parses_subcommands_case_insensitively() {
        assert_eq!(parse_garbage_command("!garbage next"), Some(GarbageCommand::NextEvent));
        assert_eq!(parse_garbage_command("!GARBAGE NEXT"), Some(GarbageCommand::NextEvent));
        assert_eq!(parse_garbage_command("!garbage week"), Some(GarbageCommand::Week));
        assert_eq!(parse_garbage_command("!garbage help"), Some(GarbageCommand::Help));
    }

    #[test]
    fn unknown_subcommand_falls_back_to_help() {
        assert_eq!(parse_garbage_command("!garbage bogus"), Some(GarbageCommand::Help));
    }

    #[test]
    fn ignores_unrelated_messages() {
        assert_eq!(parse_garbage_command("hello"), None);
        assert_eq!(parse_garbage_command("!garbagefoo"), None);
        assert_eq!(parse_garbage_command("!reset-trust @x:y"), None);
    }

    // ── Relative date formatting ──────────────────────────────────────────────

    #[test]
    fn relative_date_near_term() {
        assert_eq!(format_relative_date(today(), today()), "today");
        assert_eq!(format_relative_date(today() + chrono::Duration::days(1), today()), "tomorrow");
        assert_eq!(format_relative_date(today() + chrono::Duration::days(2), today()), "in 2 days");
        assert_eq!(format_relative_date(today() + chrono::Duration::days(6), today()), "in 6 days");
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
        let pickups = vec![pickup(2, "Organic"), pickup(2, "Paper"), pickup(5, "Residual")];
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
        let pickups = vec![pickup(0, "A"), pickup(6, "B"), pickup(7, "C"), pickup(-1, "D")];
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
        let pickups = vec![pickup(1, "Organic"), pickup(4, "Paper"), pickup(7, "Residual")];
        let (plain, html) = render_garbage_list("Next collections", &pickups, today(), &colors());

        assert!(plain.starts_with("🗑️ Next collections\n\n"));
        assert_eq!(plain.lines().count(), 5); // header + blank + 3 entries
        assert!(!plain.contains('|')); // no table-like formatting
        assert!(plain.contains("🟤 Organic · tomorrow"));
        assert!(plain.contains("🔵 Paper · in 4 days"));
        assert!(plain.contains("⚫ Residual · Mon, 27 Jul"));

        assert!(!html.contains("<table"));
        assert!(html.contains("<b>Next collections</b>"));
        assert!(html.matches("<br>").count() >= 4); // header break + between 3 entries
    }

    #[test]
    fn unknown_waste_color_falls_back_to_generic_trash_emoji() {
        let pickups = vec![pickup(1, "Mystery")];
        let (plain, _) = render_garbage_list("Next collections", &pickups, today(), &colors());
        assert!(plain.contains("🗑️ Mystery · tomorrow"));
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
        assert!(plain.len() < 200);
    }

    // ── Berlin Recycling cache ────────────────────────────────────────────────
    // Regression coverage for the portal dropping an imminent pickup from its
    // calendar response a day or two before it happens: a date we've already
    // seen once must keep showing up even if a later fetch no longer lists it.

    #[test]
    fn merge_with_empty_cache_returns_fetched_future_dates() {
        let fetched = vec![pickup(1, "Paper"), pickup(8, "Paper")];
        let merged = merge_berlin_recycling(&[], &fetched, today());
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_keeps_a_cached_date_the_source_stopped_listing() {
        // Yesterday's fetch saw tomorrow's pickup; today's fetch — because
        // the portal already dropped it — no longer includes it.
        let cache = vec![pickup(1, "Paper"), pickup(8, "Paper")];
        let fetched = vec![pickup(8, "Paper")]; // tomorrow's entry vanished from the source
        let merged = merge_berlin_recycling(&cache, &fetched, today());

        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|p| p.date == today() + chrono::Duration::days(1)));
    }

    #[test]
    fn merge_drops_past_cached_entries_instead_of_resurrecting_them() {
        let cache = vec![pickup(-7, "Paper"), pickup(1, "Paper")];
        let merged = merge_berlin_recycling(&cache, &[], today());

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].date, today() + chrono::Duration::days(1));
    }

    #[test]
    fn merge_deduplicates_a_date_present_in_both_cache_and_fetch() {
        let cache = vec![pickup(1, "Paper")];
        let fetched = vec![pickup(1, "Paper")];
        let merged = merge_berlin_recycling(&cache, &fetched, today());
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn merge_on_failed_fetch_falls_back_to_cache() {
        // fetch_all_pickups passes an empty slice on a failed Berlin Recycling
        // fetch; previously-cached upcoming dates must still come through.
        let cache = vec![pickup(1, "Paper"), pickup(8, "Paper")];
        let merged = merge_berlin_recycling(&cache, &[], today());
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_result_is_sorted_by_date_then_label() {
        let cache = vec![pickup(8, "Residual"), pickup(1, "Paper")];
        let fetched = vec![pickup(1, "Organic")];
        let merged = merge_berlin_recycling(&cache, &fetched, today());

        let dates: Vec<_> = merged.iter().map(|p| (p.date, p.label.as_str())).collect();
        assert_eq!(dates, vec![
            (today() + chrono::Duration::days(1), "Organic"),
            (today() + chrono::Duration::days(1), "Paper"),
            (today() + chrono::Duration::days(8), "Residual"),
        ]);
    }
}

