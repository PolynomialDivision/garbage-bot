//! The reminder scheduler: timing, delivery with retries, test reminders.

use super::*;

/// Resolve a naive local wall-clock time against a timezone, handling the two
/// DST edge cases `TimeZone::from_local_datetime` can return:
/// - "spring forward" gap (e.g. 02:30 doesn't exist), nudge forward until it
///   resolves, so a reminder time that falls in the gap fires slightly late
///   instead of never firing.
/// - "fall back" overlap (e.g. 02:30 happens twice), pick the earlier
///   occurrence, so the reminder fires at the first opportunity.
pub(crate) fn resolve_local<Tz: TimeZone>(tz: &Tz, naive: chrono::NaiveDateTime) -> DateTime<Tz> {
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
/// timezone: today if `target_time` hasn't passed yet, otherwise tomorrow.
/// DST-aware: computed via `resolve_local` rather than naive datetime
/// subtraction, which would be off by an hour on the two DST-transition days.
pub(crate) fn next_reminder_datetime<Tz: TimeZone>(
    now: DateTime<Tz>,
    target_time: NaiveTime,
) -> DateTime<Tz> {
    let today = now.date_naive();
    let candidate_date = if now.time() < target_time {
        today
    } else {
        today + chrono::Duration::days(1)
    };
    resolve_local(&now.timezone(), candidate_date.and_time(target_time))
}

/// Bounded exponential backoff for reminder-delivery retries: 5s, 10s, 20s...
/// capped at 5 minutes. Deliberately has no max-attempt cutoff. A reminder
/// must keep retrying until delivered, not give up and disappear.
pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    let secs = 5u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_secs(secs.min(300))
}

/// Whether a pending reminder has been retrying for longer than
/// `max_lateness`: the bounded counterpart to `backoff_delay`'s unbounded
/// attempt count. Retries are unlimited *in number* but not in *time*: past
/// this point the reminder's "put them out tonight" content is actively
/// wrong, so delivering it would mislead rather than help.
pub(crate) fn is_pending_expired(
    pending: &PendingReminder,
    now: DateTime<Tz>,
    max_lateness: chrono::Duration,
) -> bool {
    now.date_naive() >= pending.date
        || now.to_utc().signed_duration_since(pending.created_at) > max_lateness
}

pub(crate) fn format_duration_minutes(d: chrono::Duration) -> String {
    let total_minutes = d.num_minutes().max(0);
    format!("{}h{:02}m", total_minutes / 60, total_minutes % 60)
}

/// Room IDs (as strings) that still need this reminder sent.
pub(crate) fn rooms_pending_delivery(
    pending: &PendingReminder,
    joined_room_ids: &[String],
) -> Vec<String> {
    joined_room_ids
        .iter()
        .filter(|id| !pending.sent_room_ids.contains(*id))
        .cloned()
        .collect()
}

/// True once every currently-joined room has received the reminder. False
/// (not true) if there are no joined rooms at all. Nothing has actually been
/// delivered yet, so it must not be mistaken for "done".
pub(crate) fn is_fully_delivered(pending: &PendingReminder, joined_room_ids: &[String]) -> bool {
    !joined_room_ids.is_empty()
        && joined_room_ids
            .iter()
            .all(|id| pending.sent_room_ids.contains(id))
}

/// Pure: whether a new reminder should be created right now, and for which
/// date. Returns `None` if it's not due yet, one is already pending, or
/// today's slot was already evaluated (sent or found empty).
pub(crate) fn should_start_new_reminder(
    now_date: NaiveDate,
    now_time: NaiveTime,
    target_time: NaiveTime,
    last_sent_date: Option<NaiveDate>,
    has_pending: bool,
) -> Option<NaiveDate> {
    if has_pending || now_time < target_time {
        return None;
    }
    let target_date = now_date + chrono::Duration::days(1);
    if last_sent_date == Some(target_date) {
        return None;
    }
    Some(target_date)
}

pub(crate) fn reminder_transaction_id(
    pending: &PendingReminder,
    room_id: &str,
) -> OwnedTransactionId {
    let mut labels: Vec<&str> = pending.pickups.iter().map(|p| p.label.as_str()).collect();
    labels.sort_unstable();
    format!(
        "garbage-reminder-{}-{}-{}",
        pending.date,
        room_id,
        labels.join("-")
    )
    .into()
}

/// Send `plain`/`html` to one room, track it for ✅ confirmations, and seed a
/// ✅ reaction so users have an immediate button to tap. Shared by the
/// persisted/retried production path and the ephemeral `--test` path so
/// there's exactly one place that knows how to deliver a reminder message.
pub(crate) async fn deliver_reminder_to_room(
    room: &Room,
    plain: &str,
    html: &str,
    transaction_id: Option<OwnedTransactionId>,
) -> matrix_sdk::Result<OwnedEventId> {
    let mut content = RoomMessageEventContent::text_html(plain.to_owned(), html.to_owned());
    let mut mentions = Mentions::new();
    mentions.room = true;
    content = content.add_mentions(mentions);
    let resp = match transaction_id {
        Some(transaction_id) => {
            room.send(content)
                .with_transaction_id(transaction_id)
                .await?
        }
        None => room.send(content).await?,
    };
    let event_id = resp.response.event_id;

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
pub(crate) async fn resolve_pending_reminder(state: &BotState) {
    let pending = { state.persisted.lock().await.pending_reminder.clone() };
    let Some(mut pending) = pending else {
        return;
    };

    let now = reminder_now();
    if is_pending_expired(&pending, now, state.max_lateness()) {
        error!(
            "Reminder for {} expired unsent: still pending after {}, {}/{} room(s) confirmed, {} attempt(s): giving up rather than deliver stale \"put them out tonight\" info",
            pending.date,
            format_duration_minutes(now.to_utc() - pending.created_at),
            pending.sent_room_ids.len(),
            state.bot.broadcast_rooms().len(),
            pending.attempt
        );
        let mut persisted = state.persisted.lock().await;
        persisted.pending_reminder = None;
        persisted.last_expired_date = Some(pending.date);
        save_persisted_state(&state.reminder_state_path, &persisted).await;
        return;
    }

    let rooms = state.bot.broadcast_rooms();
    let joined_ids: Vec<String> = rooms.iter().map(|r| r.room_id().to_string()).collect();
    let targets = rooms_pending_delivery(&pending, &joined_ids);

    if joined_ids.is_empty() {
        warn!(
            "Reminder for {} still pending, no joined rooms yet (attempt {})",
            pending.date,
            pending.attempt + 1
        );
    }

    let (plain, html) = build_reminder(&pending.pickups, pending.date, &state.waste_colors, None);
    for room in &rooms {
        let room_id = room.room_id().to_string();
        if !targets.contains(&room_id) {
            continue;
        }
        let transaction_id = reminder_transaction_id(&pending, &room_id);
        match deliver_reminder_to_room(room, &plain, &html, Some(transaction_id)).await {
            Ok(event_id) => {
                info!(
                    "Reminder delivered: room={room_id} date={} event_id={event_id}",
                    pending.date
                );
                // Recorded right away so an immediate ✅ is already matched.
                record_posted(
                    &mut *state.persisted.lock().await,
                    &pending.pickups,
                    pending.date,
                    vec![(room_id.clone(), event_id.to_string())],
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

/// Remember where the reminder for `date` was posted, so ✅ reactions on it
/// can be matched (also after a restart) and the message edited.
pub(crate) fn record_posted(
    persisted: &mut PersistedState,
    pickups: &[Pickup],
    date: NaiveDate,
    delivered: Vec<(String, String)>,
) {
    if delivered.is_empty() {
        return;
    }
    let posted = match &mut persisted.posted {
        Some(posted) if posted.date == date => posted,
        slot => slot.insert(PostedReminder {
            date,
            pickups: pickups.to_vec(),
            event_ids: HashMap::new(),
            sent_at: reminder_now_utc(),
            confirmations: Vec::new(),
            followup_sent: false,
            followup_at: None,
            followup_event_ids: HashMap::new(),
        }),
    };
    posted.event_ids.extend(delivered);
}

/// If a new reminder is due, fetch the schedule and persist it as pending
/// *before* attempting any send, so a crash right after this still leaves a
/// recoverable record on restart. `resolve_pending_reminder` does the actual
/// sending, whether called right after this or on a later retry/restart.
pub(crate) async fn maybe_start_reminder(state: &BotState, target_time: NaiveTime) {
    let now = reminder_now();
    let (last_checked_date, last_sent_date, has_pending) = {
        let persisted = state.persisted.lock().await;
        (
            persisted.last_checked_date,
            persisted.last_sent_date,
            persisted.pending_reminder.is_some(),
        )
    };
    let decision = should_start_new_reminder(
        now.date_naive(),
        now.time(),
        target_time,
        last_sent_date,
        has_pending,
    );
    info!(
        now = %now.to_rfc3339(),
        timezone = %REMINDER_TZ,
        reminder_time = %target_time.format("%H:%M"),
        last_checked = ?last_checked_date,
        last_sent = ?last_sent_date,
        pending = has_pending,
        target_date = ?decision,
        "Reminder scheduler tick"
    );
    let Some(target_date) = decision else {
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
            created_at: now.to_utc(),
        });
    }
    save_persisted_state(&state.reminder_state_path, &persisted).await;
}

pub(crate) async fn run_scheduler(state: BotState) {
    let target_time = state.reminder_time();
    let now = reminder_now();
    let next = next_reminder_datetime(now, target_time);
    info!(
        timezone = %REMINDER_TZ,
        reminder_time = %target_time.format("%H:%M"),
        now = %now.to_rfc3339(),
        next = %next.to_rfc3339(),
        "Reminder scheduler started"
    );
    loop {
        // Order matters: create today's pending reminder first (if due), then
        // attempt delivery in the same tick. A freshly-created reminder
        // doesn't wait for the next tick just to make its first send attempt.
        // Re-read each tick: admins can change the time at runtime.
        let target_time = state.reminder_time();
        maybe_start_reminder(&state, target_time).await;
        resolve_pending_reminder(&state).await;
        maybe_send_followup(&state).await;

        let pending_attempt = {
            let persisted = state.persisted.lock().await;
            persisted.pending_reminder.as_ref().map(|p| p.attempt)
        };
        let sleep_dur = match pending_attempt {
            Some(attempt) => backoff_delay(attempt),
            None => {
                let now = reminder_now();
                let target_dt = next_reminder_datetime(now, target_time).to_utc();
                let followup_dt = {
                    let persisted = state.persisted.lock().await;
                    persisted
                        .posted
                        .as_ref()
                        .zip(state.followup_after())
                        .and_then(|(posted, after)| followup_due_at(posted, after))
                };
                let wake = followup_dt.map_or(target_dt, |f| f.min(target_dt));
                Duration::from_secs((wake - now.to_utc()).num_seconds().max(1) as u64)
                    .min(Duration::from_secs(15 * 60))
            }
        };
        let next_wake = reminder_now()
            + chrono::Duration::seconds(sleep_dur.as_secs().min(i64::MAX as u64) as i64);
        info!(
            next = %next_wake.to_rfc3339(),
            "Scheduler sleeping {}s (pending={})",
            sleep_dur.as_secs(),
            pending_attempt.is_some()
        );
        sleep(sleep_dur).await;
    }
}

/// One-shot `--test` path: send the next upcoming reminder immediately and
/// don't loop. Deliberately does not touch `state.persisted`'s pending/sent
/// tracking: it's a manual dev trigger, not part of the daily production
/// cadence, and shouldn't affect what the real scheduler considers "already
/// sent".
pub(crate) async fn send_test_reminder(state: &BotState) {
    let pickups = fetch_all_pickups(state).await;
    let today = reminder_now().date_naive();
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
        let joined = state.bot.broadcast_rooms();
        if !joined.is_empty() {
            break joined;
        }
        if tries >= 60 {
            warn!("No joined rooms after 30s, bot not invited yet?");
            return;
        }
        sleep(Duration::from_millis(500)).await;
        tries += 1;
    };

    let (plain, html) = build_reminder(&day_pickups, next_date, &state.waste_colors, None);
    let mut delivered = Vec::new();
    for room in rooms {
        match deliver_reminder_to_room(&room, &plain, &html, None).await {
            Ok(event_id) => {
                info!(
                    "Sent test reminder to {}, event_id={}",
                    room.room_id(),
                    event_id
                );
                delivered.push((room.room_id().to_string(), event_id.to_string()));
            }
            Err(e) => error!("Failed to send test reminder to {}: {e}", room.room_id()),
        }
    }
    // Tracked like a real reminder so ✅ on the test message can be tried
    // out; the next real reminder replaces it.
    record_posted(
        &mut *state.persisted.lock().await,
        &day_pickups,
        next_date,
        delivered,
    );
}
