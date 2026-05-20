use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Result};
use chrono::{Datelike, Local, NaiveDate, NaiveTime};
use futures_util::StreamExt;
use matrix_sdk::{
    Client, Room, RoomState, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::verification::{
        SasState, Verification, VerificationRequest, VerificationRequestState,
    },
    ruma::{
        OwnedDeviceId, OwnedEventId, OwnedUserId,
        api::client::filter::FilterDefinition,
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            reaction::OriginalSyncReactionEvent,
            room::{
                member::StrippedRoomMemberEvent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
    },
};
use matrix_sdk_base::crypto::CollectStrategy;
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
struct MatrixConfig {
    homeserver: String,
    user_id: String,
    access_token: String,
    device_id: String,
    recovery_key: Option<String>,
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

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum EncryptionStrategy {
    AllDevices,
    #[default]
    IdentityBased,
    OnlyTrusted,
}

impl From<EncryptionStrategy> for CollectStrategy {
    fn from(s: EncryptionStrategy) -> Self {
        match s {
            EncryptionStrategy::AllDevices => CollectStrategy::AllDevices,
            EncryptionStrategy::IdentityBased => CollectStrategy::IdentityBasedStrategy,
            EncryptionStrategy::OnlyTrusted => CollectStrategy::OnlyTrustedDevices,
        }
    }
}

#[derive(Deserialize)]
struct SecurityConfig {
    #[serde(default)]
    admin_users: Vec<String>,
    #[serde(default)]
    allowed_inviters: Vec<String>,
    #[serde(default)]
    encryption_strategy: EncryptionStrategy,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self { admin_users: vec![], allowed_inviters: vec![], encryption_strategy: EncryptionStrategy::default() }
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
}

// ── Message formatting ────────────────────────────────────────────────────────

fn build_reminder(pickups: &[Pickup], pickup_date: NaiveDate, colors: &HashMap<String, String>) -> (String, String) {
    let day = pickup_date.format("%A, %d.%m.%Y").to_string();

    let plain_bins: String =
        pickups.iter().map(|p| format!("🗑️ {}\n", p.label)).collect();
    let html_bins: String = pickups.iter().map(|p| {
        match colors.get(&p.label) {
            Some(color) => format!("<li>🗑️ <font data-mx-color=\"{color}\"><strong>{}</strong></font></li>", p.label),
            None        => format!("<li>🗑️ <strong>{}</strong></li>", p.label),
        }
    }).collect();

    let plain = format!(
        "📢 Garbage Collection Reminder\nTomorrow ({day}):\n{plain_bins}Please put them out tonight!\nReact with ✅ once it's done."
    );
    let html = format!(
        "<strong>📢 Garbage Collection Reminder</strong>\
         <p>Tomorrow <b>{day}</b>:</p>\
         <ul>{html_bins}</ul>\
         <p>Please put them out <b>tonight</b>!<br>\
         React with ✅ once it's done.</p>"
    );
    (plain, html)
}

fn strip_variation_selectors(s: &str) -> String {
    s.chars().filter(|&c| !('\u{FE00}'..='\u{FE0F}').contains(&c)).collect()
}

const DONE_REACTIONS: &[&str] = &["✅", "✔", "☑"];

// ── Core logic ────────────────────────────────────────────────────────────────

async fn check_and_remind(state: &BotState, client: &Client, test_mode: bool) {
    let mut pickups = match fetch_pickups(&state.http, &state.bsr_schedule_id, &state.waste_labels).await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to fetch BSR data: {e}");
            return;
        }
    };

    if let Some((ref user, ref pass)) = state.berlin_recycling_creds {
        match fetch_berlin_recycling_pickups(user, pass, &state.waste_labels).await {
            Ok(br_pickups) => {
                info!("Berlin Recycling: {} pickup(s) fetched", br_pickups.len());
                pickups.extend(br_pickups);
                pickups.sort_by_key(|p| p.date);
            }
            Err(e) => error!("Failed to fetch Berlin Recycling data: {e}"),
        }
    }

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
        match room.send(RoomMessageEventContent::text_html(plain.clone(), html.clone())).await {
            Ok(resp) => {
                info!("Sent reminder to {}, event_id={}", room.room_id(), resp.response.event_id);
                state.confirmed.lock().await.insert(resp.response.event_id, HashSet::new());
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
    fs::create_dir_all(&store_path).await?;

    let strategy: CollectStrategy = config.security.encryption_strategy.into();
    info!("Encryption strategy: {strategy:?}");

    let client = Client::builder()
        .homeserver_url(&config.matrix.homeserver)
        .sqlite_store(&store_path, None)
        .with_room_key_recipient_strategy(strategy)
        .build()
        .await?;

    let user_id: OwnedUserId = config.matrix.user_id.parse()?;
    let device_id: OwnedDeviceId = OwnedDeviceId::from(config.matrix.device_id);

    client
        .restore_session(MatrixSession {
            meta: SessionMeta { user_id: user_id.clone(), device_id },
            tokens: SessionTokens {
                access_token: config.matrix.access_token,
                refresh_token: None,
            },
        })
        .await?;

    info!("Session restored as {user_id}");

    if let Some(ref key) = config.matrix.recovery_key {
        info!("Recovering cross-signing keys from secure backup...");
        match client.encryption().recovery().recover(key).await {
            Ok(()) => info!("Cross-signing keys recovered"),
            Err(e) => warn!("Recovery failed: {e}"),
        }
    }
    bootstrap_cross_signing(&client, &user_id).await;

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
    };

    // Auto-join invited rooms (only from allowed_inviters)
    client.add_event_handler({
        let state = state.clone();
        move |ev: StrippedRoomMemberEvent, room: Room| {
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
                tokio::spawn(async move {
                    let mut delay = 2u64;
                    loop {
                        match room.join().await {
                            Ok(_) => {
                                info!("Joined {}", room.room_id());
                                break;
                            }
                            Err(err) => {
                                warn!("Join failed for {}: {err}; retry in {delay}s", room.room_id());
                                sleep(Duration::from_secs(delay)).await;
                                delay = (delay * 2).min(3600);
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
                tokio::spawn(handle_verification_request(client, state, request));
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
                    tokio::spawn(handle_verification_request(client, state, request));
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
    client.sync(SyncSettings::default().filter(filter.into())).await?;

    Ok(())
}

// ── Reaction handler ──────────────────────────────────────────────────────────

async fn handle_reaction(state: BotState, room: Room, ev: OriginalSyncReactionEvent) {
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

// ── Verification (same as translate-bot) ─────────────────────────────────────

async fn handle_verification_request(
    client: Client,
    state: BotState,
    request: VerificationRequest,
) {
    let user_id = request.other_user_id();

    let already_verified = client
        .encryption()
        .get_user_devices(user_id)
        .await
        .map(|devices| devices.devices().any(|d| d.is_verified()))
        .unwrap_or(false);

    if already_verified {
        let allowed = state.reset_allowed.lock().await.remove(user_id);
        if !allowed {
            warn!("Rejecting verification from {} — already has a verified device", user_id);
            request.cancel().await.ok();
            return;
        }
        info!("Allowing re-verification for {} (trust was reset by admin)", user_id);
    }

    info!("Accepting verification from {user_id}");
    if let Err(e) = request.accept().await {
        error!("Failed to accept verification request: {e}");
        return;
    }

    let mut stream = request.changes();
    while let Some(state) = stream.next().await {
        match state {
            VerificationRequestState::Transitioned { verification } => {
                if let Verification::SasV1(sas) = verification {
                    tokio::spawn(handle_sas(sas));
                    break;
                }
            }
            VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => break,
            _ => {}
        }
    }
}

async fn handle_sas(sas: matrix_sdk::encryption::verification::SasVerification) {
    info!("SAS with {} {}", sas.other_device().user_id(), sas.other_device().device_id());

    if let Err(e) = sas.accept().await {
        error!("Failed to accept SAS: {e}");
        return;
    }

    let mut stream = sas.changes();
    while let Some(state) = stream.next().await {
        match state {
            SasState::KeysExchanged { .. } => {
                info!("Auto-confirming emojis");
                if let Err(e) = sas.confirm().await {
                    error!("SAS confirm failed: {e}");
                    break;
                }
            }
            SasState::Done { .. } => {
                info!(
                    "Verification done: {} {}",
                    sas.other_device().user_id(),
                    sas.other_device().device_id()
                );
                break;
            }
            SasState::Cancelled(info) => {
                warn!("Verification cancelled: {}", info.reason());
                break;
            }
            _ => {}
        }
    }
}

async fn bootstrap_cross_signing(client: &Client, user_id: &OwnedUserId) {
    match client.encryption().bootstrap_cross_signing(None).await {
        Ok(()) => info!("Cross-signing bootstrapped — bot device is now self-signed"),
        Err(e) => warn!("Cross-signing bootstrap failed for {user_id}: {e}"),
    }
}
