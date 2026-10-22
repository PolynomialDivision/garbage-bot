//! ✅ confirmations, retractions and the follow-up when nobody confirmed.

use super::*;

// ── Reaction handler ──────────────────────────────────────────────────────────

pub(crate) async fn handle_reaction(state: BotState, room: Room, ev: OriginalSyncReactionEvent) {
    // Ignore the bot's own seed reaction. It only exists to show users the button.
    if ev.sender == state.bot.user_id {
        return;
    }
    let key = strip_variation_selectors(&ev.content.relates_to.key);
    if !DONE_REACTIONS.contains(&key.as_str()) {
        return;
    }
    let related_id = ev.content.relates_to.event_id.to_string();
    let is_reminder = {
        let persisted = state.persisted.lock().await;
        persisted
            .posted
            .as_ref()
            .is_some_and(|posted| posted.is_reminder_event(&related_id))
    };
    if !is_reminder {
        return; // not the current reminder
    }

    let name = room
        .get_member(ev.sender.as_ref())
        .await
        .ok()
        .flatten()
        .and_then(|m| m.display_name().map(str::to_owned))
        .unwrap_or_else(|| ev.sender.localpart().to_owned());

    let posted = {
        let mut persisted = state.persisted.lock().await;
        let Some(posted) = persisted
            .posted
            .as_mut()
            .filter(|posted| posted.is_reminder_event(&related_id))
        else {
            return;
        };
        let reaction_id = ev.event_id.to_string();
        if !add_confirmation(
            posted,
            ev.sender.as_str(),
            &name,
            reminder_now_utc(),
            &reaction_id,
        ) {
            // Already confirmed; the extra reaction is remembered for retraction.
            save_persisted_state(&state.reminder_state_path, &persisted).await;
            return;
        }
        let posted = posted.clone();
        save_persisted_state(&state.reminder_state_path, &persisted).await;
        posted
    };
    info!(
        "Confirmation from {name} for {} ({} total)",
        posted.date,
        posted.confirmations.len()
    );
    edit_reminder_messages(&state).await;
}

/// Record a ✅ from `user_id`; true if this made them a new confirmer
/// (a second ✅-type reaction from them is only remembered for retraction).
pub(crate) fn add_confirmation(
    posted: &mut PostedReminder,
    user_id: &str,
    name: &str,
    at: DateTime<chrono::Utc>,
    reaction_id: &str,
) -> bool {
    if let Some(existing) = posted
        .confirmations
        .iter_mut()
        .find(|c| c.user_id == user_id)
    {
        existing.reaction_ids.push(reaction_id.to_owned());
        return false;
    }
    posted.confirmations.push(Confirmation {
        user_id: user_id.to_owned(),
        name: name.to_owned(),
        at,
        reaction_ids: vec![reaction_id.to_owned()],
    });
    true
}

/// Forget a retracted reaction; true if its user thereby no longer confirms.
pub(crate) fn remove_confirmation_reaction(posted: &mut PostedReminder, reaction_id: &str) -> bool {
    let Some(index) = posted
        .confirmations
        .iter()
        .position(|c| c.reaction_ids.iter().any(|id| id == reaction_id))
    else {
        return false;
    };
    let confirmation = &mut posted.confirmations[index];
    confirmation.reaction_ids.retain(|id| id != reaction_id);
    if confirmation.reaction_ids.is_empty() {
        posted.confirmations.remove(index);
        return true;
    }
    false
}

/// A retracted ✅ on the current reminder takes the confirmation back.
pub(crate) async fn handle_redaction(state: BotState, ev: OriginalSyncRoomRedactionEvent) {
    let Some(redacted) = ev.content.redacts.clone().or_else(|| ev.redacts.clone()) else {
        return;
    };
    let withdrawn = {
        let mut persisted = state.persisted.lock().await;
        let Some(posted) = persisted.posted.as_mut() else {
            return;
        };
        if !remove_confirmation_reaction(posted, redacted.as_str()) {
            save_persisted_state(&state.reminder_state_path, &persisted).await;
            return;
        }
        let date = posted.date;
        save_persisted_state(&state.reminder_state_path, &persisted).await;
        date
    };
    info!("{} withdrew their confirmation for {withdrawn}", ev.sender);
    edit_reminder_messages(&state).await;
}

/// Rewrite the reminder in every room to show who confirmed. An edit instead
/// of a new message: the room sees the reminder turn into "✅ Done by Alice"
/// without another notification.
pub(crate) async fn edit_reminder_messages(state: &BotState) {
    let _serialized = state.edit_lock.lock().await;
    // Render from the latest state, not the caller's snapshot: a later
    // confirmation must never be overwritten by an older edit.
    let Some(posted) = state.persisted.lock().await.posted.clone() else {
        return;
    };
    let confirmed_by = posted.confirmed_by();
    let (plain, html) = build_reminder(
        &posted.pickups,
        posted.date,
        &state.waste_colors,
        confirmed_by.as_deref(),
    );
    for room in state.bot.broadcast_rooms() {
        let Some(event_id) = posted
            .event_ids
            .get(room.room_id().as_str())
            .and_then(|id| id.parse::<OwnedEventId>().ok())
        else {
            continue;
        };
        let content = RoomMessageEventContent::text_html(plain.clone(), html.clone())
            .add_mentions(Mentions::with_room_mention())
            // The original already mentioned @room: passing its mentions
            // keeps the edit from notifying everyone again.
            .make_replacement(ReplacementMetadata::new(
                event_id,
                Some(Mentions::with_room_mention()),
            ));
        if let Err(e) = send_with_retry(&room, content).await {
            error!("Failed to update reminder in {}: {e}", room.room_id());
        }
    }

    // The follow-up should not keep saying "nobody has confirmed".
    let Some(followup_at) = posted.followup_at else {
        return;
    };
    let (plain, html) = followup_text(&posted, followup_at.with_timezone(&reminder_tz()));
    for room in state.bot.broadcast_rooms() {
        let Some(event_id) = posted
            .followup_event_ids
            .get(room.room_id().as_str())
            .and_then(|id| id.parse::<OwnedEventId>().ok())
        else {
            continue;
        };
        let content = RoomMessageEventContent::text_html(plain.clone(), html.clone())
            .add_mentions(Mentions::with_room_mention())
            .make_replacement(ReplacementMetadata::new(
                event_id,
                Some(Mentions::with_room_mention()),
            ));
        if let Err(e) = send_with_retry(&room, content).await {
            error!("Failed to update follow-up in {}: {e}", room.room_id());
        }
    }
}

// ── Follow-up when nobody confirmed ──────────────────────────────────────────

/// Collection crews start early; a nudge after this hour on collection day
/// would only arrive once the bins are gone.
pub(crate) const LAST_FOLLOWUP_HOUR: u32 = 6;

/// When the one follow-up for `posted` is due, if it still can be: not yet
/// sent, nobody confirmed, and before the early morning of collection day.
pub(crate) fn followup_due_at(
    posted: &PostedReminder,
    followup_after: chrono::Duration,
) -> Option<DateTime<chrono::Utc>> {
    if posted.followup_sent || !posted.confirmations.is_empty() {
        return None;
    }
    let due = posted.sent_at + followup_after;
    let deadline = resolve_local(
        &reminder_tz(),
        posted
            .date
            .and_hms_opt(LAST_FOLLOWUP_HOUR, 0, 0)
            .expect("valid time"),
    );
    (due < deadline.to_utc()).then_some(due)
}

/// The follow-up as posted at `now`, or (once someone confirmed) turned
/// into a short all-clear so it no longer claims nobody did.
pub(crate) fn followup_text(posted: &PostedReminder, now: DateTime<Tz>) -> (String, String) {
    if let Some(names) = posted.confirmed_by() {
        let plain = format!("✅ Bins are out. Thanks, {names}! 🙌");
        let html = format!("✅ Bins are out. Thanks, {}! 🙌", escape_html(&names));
        return (plain, html);
    }
    let when = if now.date_naive() >= posted.date {
        "this morning"
    } else {
        "tomorrow morning"
    };
    let plain = format!(
        "⏰ @room Nobody has confirmed the bins yet.\n\
         🚛 Pickup is {when}.\n\
         👉 Tap ✅ on the reminder once they're out."
    );
    let html = plain.replace('\n', "<br>");
    (plain, html)
}

/// Post the follow-up once, as a reply to the reminder in each room.
pub(crate) async fn maybe_send_followup(state: &BotState) {
    let Some(followup_after) = state.followup_after() else {
        return;
    };
    let now = reminder_now();
    let posted = {
        let mut persisted = state.persisted.lock().await;
        let Some(posted) = persisted.posted.as_mut() else {
            return;
        };
        match followup_due_at(posted, followup_after) {
            Some(due) if now.to_utc() >= due => {}
            Some(_) => return,
            None => {
                return;
            }
        }
        // Marked before sending: one follow-up at most, even if a send fails.
        posted.followup_sent = true;
        posted.followup_at = Some(now.to_utc());
        let posted = posted.clone();
        save_persisted_state(&state.reminder_state_path, &persisted).await;
        posted
    };

    let (plain, html) = followup_text(&posted, now);
    for room in state.bot.broadcast_rooms() {
        let Some(event_id) = posted
            .event_ids
            .get(room.room_id().as_str())
            .and_then(|id| id.parse::<OwnedEventId>().ok())
        else {
            continue;
        };
        let mut content = RoomMessageEventContent::text_html(plain.clone(), html.clone())
            .add_mentions(Mentions::with_room_mention());
        content.relates_to = Some(Relation::Reply(Reply::with_event_id(event_id)));
        let transaction_id: OwnedTransactionId =
            format!("garbage-followup-{}-{}", posted.date, room.room_id()).into();
        match send_with_retry_txn(&room, content, &transaction_id).await {
            Ok(followup_id) => {
                info!("Follow-up sent to {} for {}", room.room_id(), posted.date);
                let mut persisted = state.persisted.lock().await;
                if let Some(current) = persisted.posted.as_mut().filter(|p| p.date == posted.date) {
                    current
                        .followup_event_ids
                        .insert(room.room_id().to_string(), followup_id.to_string());
                    save_persisted_state(&state.reminder_state_path, &persisted).await;
                }
            }
            Err(e) => error!("Failed to send follow-up to {}: {e}", room.room_id()),
        }
    }
}
