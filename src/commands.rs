//! The `!garbage` command.

use super::*;

// ── !garbage command ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GarbageCommand {
    /// `!garbage`: next upcoming pickup for each waste type.
    NextPerType,
    /// `!garbage next`: only the single nearest pickup event.
    NextEvent,
    /// `!garbage week`: every pickup in the next 7 days.
    Week,
    /// `!garbage status`: source health, reminder/scheduler state.
    Status,
    /// `!garbage refresh` (admin-only): force a schedule refresh now.
    Refresh,
    /// `!garbage version`: deployed build version.
    Version,
    /// `!garbage help` (or an unrecognized subcommand): usage text.
    Help,
}

pub(crate) fn parse_garbage_command(body: &str) -> Option<GarbageCommand> {
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

/// Remove duplicate (date, label) pairs. The BSR and Berlin Recycling feeds
/// can list the same pickup twice if their configured labels collide.
pub(crate) fn dedup_pickups(pickups: Vec<Pickup>) -> Vec<Pickup> {
    let mut seen: HashSet<(NaiveDate, String)> = HashSet::new();
    pickups
        .into_iter()
        .filter(|p| seen.insert((p.date, p.label.clone())))
        .collect()
}

/// Earliest upcoming date per waste type, sorted chronologically
/// (ties broken by label for a stable order).
pub(crate) fn next_pickup_per_type(pickups: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
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
pub(crate) fn next_event(pickups: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
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
pub(crate) fn pickups_this_week(pickups: &[Pickup], today: NaiveDate) -> Vec<Pickup> {
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
pub(crate) fn format_relative_date(date: NaiveDate, today: NaiveDate) -> String {
    let absolute = if date.year() == today.year() {
        date.format("%a, %-d %b").to_string()
    } else {
        date.format("%a, %-d %b %Y").to_string()
    };
    match (date - today).num_days() {
        0 => format!("{absolute} (today)"),
        1 => format!("{absolute} (tomorrow)"),
        _ => absolute,
    }
}

pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn garbage_empty_message() -> (String, String) {
    let text = "🤷 No upcoming pickups found.".to_owned();
    (text.clone(), text)
}

pub(crate) fn garbage_help_message() -> (String, String) {
    let plain = "🗑️ Garbage commands\n\
                 📋 !garbage: next per type\n\
                 ⏭️ !garbage next: next pickup\n\
                 📆 !garbage week: next 7 days\n\
                 🩺 !garbage status: source health\n\
                 🔄 !garbage refresh: reload (admin)\n\
                 🏷️ !garbage version: build version"
        .to_owned();
    let html = plain.replace('\n', "<br>");
    (plain, html)
}

pub(crate) fn garbage_version_message() -> (String, String) {
    let text = format!("🗑️ garbage-bot v{}", env!("CARGO_PKG_VERSION"));
    (text.clone(), text)
}

/// One-line human summary of a source's health for `!garbage status`.
pub(crate) fn describe_source_health(health: &SourceHealth, cached_count: usize) -> String {
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
pub(crate) fn describe_reminder_state(persisted: &PersistedState) -> String {
    let base = match &persisted.pending_reminder {
        Some(p) => format!(
            "pending for {} · attempt {} · delivered to {} room(s)",
            p.date,
            p.attempt,
            p.sent_room_ids.len()
        ),
        None => match persisted.last_sent_date {
            Some(d) => format!("last sent {d}"),
            None => "none sent yet".to_owned(),
        },
    };
    let base = match &persisted.posted {
        Some(posted) => match posted.confirmed_by() {
            Some(names) => format!("{base} · ✅ {names}"),
            None if posted.followup_sent => format!("{base} · no ✅ yet, follow-up sent"),
            None => format!("{base} · no ✅ yet"),
        },
        None => base,
    };
    match persisted.last_expired_date {
        Some(d) => format!("{base} · ⚠️ {d} expired unsent"),
        None => base,
    }
}

pub(crate) async fn build_status_message(state: &BotState) -> (String, String) {
    let persisted = state.persisted.lock().await;
    let bsr_health = state.bsr_health.lock().await.clone();

    let mut lines = vec!["🩺 Garbage status".to_owned()];
    lines.push(format!(
        "♻️ BSR: {}",
        describe_source_health(&bsr_health, persisted.bsr_cache.len())
    ));
    if state.berlin_recycling_creds.is_some() {
        let br_health = state.br_health.lock().await.clone();
        lines.push(format!(
            "📄 Paper: {}",
            describe_source_health(&br_health, persisted.br_cache.len())
        ));
    }
    lines.push(format!(
        "⏰ Reminder: {}",
        describe_reminder_state(&persisted)
    ));

    let plain = lines.join("\n");
    let html = escape_html(&plain).replace('\n', "<br>");
    (plain, html)
}

/// Render a compact one-line-per-pickup list, optimized for small phone
/// screens: no table, no long heading, `·` as separator, waste-type emoji
/// derived from `waste_colors` (same mapping the daily reminder uses).
pub(crate) fn render_garbage_list(
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

pub(crate) fn build_garbage_reply(
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

pub(crate) async fn handle_garbage_command(
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
            if !state.bot.is_admin(&sender) {
                let msg = "⛔ Only admins can do that.".to_owned();
                (msg.clone(), msg)
            } else {
                info!("!garbage refresh requested by {sender}");
                fetch_all_pickups(&state).await;
                build_status_message(&state).await
            }
        }
        GarbageCommand::NextPerType | GarbageCommand::NextEvent | GarbageCommand::Week => {
            let pickups = dedup_pickups(current_pickups(&state).await);
            let today = reminder_now().date_naive();
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
