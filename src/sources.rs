//! Collection schedules from BSR and Berlin Recycling, and their cache.

use super::*;

use anyhow::Context;
use serde::de::DeserializeOwned;

// ── BSR API ───────────────────────────────────────────────────────────────────

/// Upper bound for any single schedule request. Without it a hanging portal
/// would stall the scheduler (and every `!garbage` command) indefinitely.
pub(crate) const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("building the HTTP client")
}

pub(crate) const BSR_PICKUPS: &str = "https://umnewforms.bsr.de/p/de.bsr.adressen.app/abfuhrEvents";
pub(crate) const BSR_STREETS: &str = "https://umnewforms.bsr.de/p/de.bsr.adressen.app/streetNames";
pub(crate) const BSR_SCHEDID: &str =
    "https://umnewforms.bsr.de/p/de.bsr.adressen.app/plzSet/plzSet";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pickup {
    pub(crate) date: NaiveDate,
    pub(crate) label: String,
}

#[derive(Deserialize)]
pub(crate) struct BsrStreetEntry {
    pub(crate) value: String,
}

#[derive(Deserialize)]
pub(crate) struct BsrScheduleEntry {
    pub(crate) value: String,
    pub(crate) label: String,
}

#[derive(Deserialize)]
pub(crate) struct BsrPickupResponse {
    #[serde(default)]
    pub(crate) dates: HashMap<String, Vec<BsrPickupEntry>>,
}

#[derive(Deserialize)]
pub(crate) struct BsrPickupEntry {
    #[serde(rename = "serviceDate_actual")]
    pub(crate) service_date: String,
    pub(crate) category: String,
}

/// GET a BSR endpoint and decode its JSON. The status is checked first, so an
/// outage is logged as "500 Internal Server Error" instead of a JSON error
/// about the HTML error page.
async fn bsr_get_json<T: DeserializeOwned>(http: &reqwest::Client, url: Url) -> Result<T> {
    Ok(http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

pub(crate) async fn resolve_schedule_id(
    http: &reqwest::Client,
    street: &str,
    number: &str,
    plz: Option<&str>,
) -> Result<String> {
    let url = Url::parse_with_params(BSR_STREETS, &[("searchQuery", street)])?;
    let streets: Vec<BsrStreetEntry> = bsr_get_json(http, url).await?;

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
    let results: Vec<BsrScheduleEntry> = bsr_get_json(http, url).await?;

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

    // Multiple results: log them all so the user can identify the right one.
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
        warn!("PLZ {plz} not found in any result, falling back to first entry");
    }

    Ok(results[0].value.clone())
}

/// The BSR schedule ID: configured, remembered, or looked up now. A failed
/// lookup is retried on the next refresh, so a BSR outage does not keep the
/// bot from starting.
pub(crate) async fn bsr_schedule_id(state: &BotState) -> Result<String> {
    if let Some(id) = state.bsr_schedule_id.lock().await.clone() {
        return Ok(id);
    }
    let address = state
        .bsr_address
        .as_ref()
        .ok_or_else(|| anyhow!("No BSR schedule_id or address configured"))?;
    info!(
        "Resolving BSR schedule_id for {} {}{}...",
        address.street,
        address.number,
        address
            .plz
            .as_deref()
            .map(|p| format!(", PLZ {p}"))
            .unwrap_or_default()
    );
    let id = resolve_schedule_id(
        &state.http,
        &address.street,
        &address.number,
        address.plz.as_deref(),
    )
    .await
    .context("BSR address lookup failed")?;
    info!("Resolved schedule_id: {id}");

    *state.bsr_schedule_id.lock().await = Some(id.clone());
    let mut persisted = state.persisted.lock().await;
    persisted.bsr_lookup = Some(BsrLookup {
        address: address.clone(),
        schedule_id: id.clone(),
    });
    save_persisted_state(&state.reminder_state_path, &persisted).await;
    Ok(id)
}

pub(crate) async fn fetch_pickups(
    http: &reqwest::Client,
    schedule_id: &str,
    waste_labels: &HashMap<String, String>,
) -> Result<Vec<Pickup>> {
    let now = reminder_now();
    let filter = format!(
        "AddrKey eq '{}' and DateFrom eq datetime'{}-{:02}-01T00:00:00' and DateTo eq datetime'{}-{:02}-01T00:00:00'",
        schedule_id,
        now.year(), now.month(),
        now.year() + 1, now.month(),
    );

    let url = Url::parse_with_params(BSR_PICKUPS, &[("filter", &filter)])?;
    let resp: BsrPickupResponse = bsr_get_json(http, url).await?;

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

pub(crate) const BR_PORTAL: &str = "https://kundenportal.berlin-recycling.de/";

pub(crate) async fn fetch_berlin_recycling_pickups(
    username: &str,
    password: &str,
    waste_labels: &HashMap<String, String>,
) -> Result<Vec<Pickup>> {
    // Each call creates a fresh session. The portal is stateful via cookies.
    let http = reqwest::Client::builder()
        .cookie_store(true)
        .timeout(HTTP_TIMEOUT)
        .build()?;

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
            "Berlin Recycling login failed: redirected to {}",
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

// ── Core logic ────────────────────────────────────────────────────────────────

/// Merge freshly-fetched pickups from one source into the cache of
/// previously-seen upcoming ones for that source. Past entries (before
/// `today`) are dropped so the cache doesn't grow forever; everything else
/// survives even if `fetched` is empty (failed refresh) or no longer lists a
/// date the cache already had. A source is never allowed to make a known
/// pickup disappear, only for it to age out once its date has passed.
pub(crate) fn merge_pickup_cache(
    cache: &[Pickup],
    fetched: &[Pickup],
    today: NaiveDate,
) -> Vec<Pickup> {
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
/// fetch (a no-op except pruning past dates, see `merge_pickup_cache`) and
/// record the error. Shared by BSR and Berlin Recycling so the "never wipe a
/// known-good schedule" behavior lives in exactly one place.
pub(crate) fn apply_fetch_outcome(
    source_name: &str,
    cache: &[Pickup],
    outcome: Result<Vec<Pickup>>,
    health: &mut SourceHealth,
    today: NaiveDate,
) -> Vec<Pickup> {
    match outcome {
        Ok(fetched) => {
            info!("{source_name}: {} pickup(s) fetched", fetched.len());
            health.last_success = Some(reminder_now());
            health.last_error = None;
            health.consecutive_failures = 0;
            merge_pickup_cache(cache, &fetched, today)
        }
        Err(e) => {
            error!("{source_name}: schedule refresh failed, keeping previous schedule: {e:#}");
            health.last_error = Some(format!("{e:#}"));
            health.consecutive_failures += 1;
            merge_pickup_cache(cache, &[], today)
        }
    }
}

pub(crate) async fn fetch_all_pickups(state: &BotState) -> Vec<Pickup> {
    // Network first, without holding the state lock: a slow portal must not
    // block reactions, status replies or the scheduler meanwhile.
    let bsr_outcome = match bsr_schedule_id(state).await {
        Ok(id) => fetch_pickups(&state.http, &id, &state.waste_labels).await,
        Err(e) => Err(e),
    };
    let br_outcome = match state.berlin_recycling_creds {
        Some((ref user, ref pass)) => {
            Some(fetch_berlin_recycling_pickups(user, pass, &state.waste_labels).await)
        }
        None => None,
    };

    let today = reminder_now().date_naive();
    let mut persisted = state.persisted.lock().await;
    persisted.bsr_cache = apply_fetch_outcome(
        "BSR",
        &persisted.bsr_cache,
        bsr_outcome,
        &mut *state.bsr_health.lock().await,
        today,
    );
    if let Some(br_outcome) = br_outcome {
        persisted.br_cache = apply_fetch_outcome(
            "Berlin Recycling",
            &persisted.br_cache,
            br_outcome,
            &mut *state.br_health.lock().await,
            today,
        );
    }
    save_persisted_state(&state.reminder_state_path, &persisted).await;
    combined_cache(&persisted, state.berlin_recycling_creds.is_some())
}

pub(crate) fn combined_cache(persisted: &PersistedState, with_br: bool) -> Vec<Pickup> {
    let mut pickups = persisted.bsr_cache.clone();
    if with_br {
        pickups.extend(persisted.br_cache.iter().cloned());
    }
    pickups.sort_by_key(|p| p.date);
    pickups
}

/// How old the cached schedule may be before a `!garbage` query refetches.
/// Collection dates change rarely; logging into Berlin Recycling for every
/// query made replies slow for no benefit.
pub(crate) const SCHEDULE_MAX_AGE: chrono::Duration = chrono::Duration::hours(6);

/// The schedule for `!garbage` queries: the cache while it is fresh, a
/// refetch once any source is older than `SCHEDULE_MAX_AGE` or failing.
pub(crate) async fn current_pickups(state: &BotState) -> Vec<Pickup> {
    let now = reminder_now();
    let stale = |health: &SourceHealth| {
        health.last_error.is_some()
            || health
                .last_success
                .is_none_or(|t| now.signed_duration_since(t) > SCHEDULE_MAX_AGE)
    };
    let bsr_stale = stale(&*state.bsr_health.lock().await);
    let br_stale = state.berlin_recycling_creds.is_some() && stale(&*state.br_health.lock().await);
    if bsr_stale || br_stale {
        return fetch_all_pickups(state).await;
    }
    let today = now.date_naive();
    let persisted = state.persisted.lock().await;
    combined_cache(&persisted, state.berlin_recycling_creds.is_some())
        .into_iter()
        .filter(|p| p.date >= today)
        .collect()
}

/// All pickups scheduled on exactly `date`.
pub(crate) fn pickups_for_date(pickups: &[Pickup], date: NaiveDate) -> Vec<Pickup> {
    pickups.iter().filter(|p| p.date == date).cloned().collect()
}
