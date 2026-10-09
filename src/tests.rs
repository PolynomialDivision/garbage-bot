//! Unit tests across the modules.

use super::*;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 7, 20).unwrap() // fixed: a Monday
}

#[test]
fn reminder_clock_is_explicitly_berlin_not_container_localtime() {
    assert_eq!(reminder_now().timezone(), chrono_tz::Europe::Berlin);
}

fn colors() -> HashMap<String, String> {
    HashMap::from([
        ("Organic".to_owned(), "#8B4513".to_owned()),
        ("Paper".to_owned(), "#4682B4".to_owned()),
        ("Residual".to_owned(), "#808080".to_owned()),
        ("Recycling".to_owned(), "#DAA520".to_owned()),
    ])
}

fn reminder_colors() -> HashMap<String, String> {
    HashMap::from([
        ("Residual Waste (grey)".to_owned(), "#808080".to_owned()),
        ("Organic Waste (brown)".to_owned(), "#8B4513".to_owned()),
        ("Paper (blue)".to_owned(), "#4682B4".to_owned()),
        ("Recyclables (yellow)".to_owned(), "#DAA520".to_owned()),
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
    assert_eq!(
        format_relative_date(today(), today()),
        "Mon, 20 Jul (today)"
    );
    assert_eq!(
        format_relative_date(today() + chrono::Duration::days(1), today()),
        "Tue, 21 Jul (tomorrow)"
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
        pickup(-1, "Residual"), // past: must be excluded
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
    assert_eq!(plain, "🤷 No upcoming pickups found.");
    assert_eq!(html, "🤷 No upcoming pickups found.");
}

#[test]
fn no_config_at_all_yields_empty_state_via_build_reply() {
    let (plain, _) = build_garbage_reply(GarbageCommand::NextPerType, &[], today(), &colors());
    assert_eq!(plain, "🤷 No upcoming pickups found.");
}

// ── Compact Matrix output ─────────────────────────────────────────────────

#[test]
fn single_bin_reminder_uses_singular_wording_and_only_that_days_pickup() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let pickups = vec![
        Pickup {
            date,
            label: "Residual Waste (grey)".to_owned(),
        },
        Pickup {
            date: date + chrono::Duration::days(1),
            label: "Paper (blue)".to_owned(),
        },
    ];

    let (plain, html) = build_reminder(&pickups, date, &reminder_colors(), None);

    assert_eq!(
        plain,
        "🚛 Garbage pickup tomorrow\n📅 Monday, 10 Aug\n\n⚫ Residual Waste (grey)\n\n@room 🌙 Put the bin out tonight.\n👉 Tap ✅ when done."
    );
    assert_eq!(
        html,
        "🚛 <strong>Garbage pickup tomorrow</strong><br>📅 <strong>Monday, 10 Aug</strong><br><br>⚫ Residual Waste (grey)<br><br>@room 🌙 Put the bin out tonight.<br>👉 Tap ✅ when done."
    );
    assert!(!plain.contains("Paper"));
}

#[test]
fn multi_bin_reminder_lists_each_due_type_with_plural_wording() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let pickups = vec![
        Pickup {
            date,
            label: "Residual Waste (grey)".to_owned(),
        },
        Pickup {
            date,
            label: "Organic Waste (brown)".to_owned(),
        },
        Pickup {
            date,
            label: "Paper (blue)".to_owned(),
        },
    ];

    let (plain, html) = build_reminder(&pickups, date, &reminder_colors(), None);

    assert_eq!(
        plain,
        "🚛 Garbage pickup tomorrow\n📅 Monday, 10 Aug\n\n⚫ Residual Waste (grey)\n🟤 Organic Waste (brown)\n🔵 Paper (blue)\n\n@room 🌙 Put the bins out tonight.\n👉 Tap ✅ when done."
    );
    assert!(
        html.contains("⚫ Residual Waste (grey)<br>🟤 Organic Waste (brown)<br>🔵 Paper (blue)")
    );
    assert!(html.ends_with("@room 🌙 Put the bins out tonight.<br>👉 Tap ✅ when done."));
    assert!(!plain.contains("🗑️"));
}

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
    assert!(plain.contains("🟤 Organic · Tue, 21 Jul (tomorrow)"));
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
    assert!(plain.contains("🗑️ Mystery · Tue, 21 Jul (tomorrow)"));
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
// even if a later fetch no longer lists it, for either source, since a
// failed BSR refresh must not wipe a known-good BSR schedule either.

#[test]
fn merge_with_empty_cache_returns_fetched_future_dates() {
    let fetched = vec![pickup(1, "Paper"), pickup(8, "Paper")];
    let merged = merge_pickup_cache(&[], &fetched, today());
    assert_eq!(merged.len(), 2);
}

#[test]
fn merge_keeps_a_cached_date_the_source_stopped_listing() {
    // Yesterday's fetch saw tomorrow's pickup; today's fetch (because
    // the portal already dropped it) no longer includes it.
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
fn restart_before_reminder_time_waits_without_creating_pending_reminder() {
    let target = NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    let before = NaiveTime::from_hms_opt(17, 30, 0).unwrap();
    assert_eq!(
        should_start_new_reminder(today(), before, target, None, false),
        None
    );
}

#[test]
fn restart_late_on_previous_day_creates_missing_tomorrow_reminder() {
    let target = NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    let late = NaiveTime::from_hms_opt(22, 0, 0).unwrap();
    assert_eq!(
        should_start_new_reminder(today(), late, target, None, false),
        Some(today() + chrono::Duration::days(1))
    );
}

#[test]
fn checked_but_unsent_reminder_is_reconciled_after_restart() {
    // `last_checked_date` deliberately is not an input: a transient empty
    // schedule result must never suppress a later successful reconciliation.
    let target = NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    let late = NaiveTime::from_hms_opt(22, 0, 0).unwrap();
    assert_eq!(
        should_start_new_reminder(today(), late, target, None, false),
        Some(today() + chrono::Duration::days(1))
    );
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
fn should_not_restart_a_reminder_already_delivered_today() {
    let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
    let tomorrow = today() + chrono::Duration::days(1);
    assert_eq!(
        should_start_new_reminder(today(), target, target, Some(tomorrow), false),
        None
    );
}

#[test]
fn should_start_again_once_the_date_rolls_over() {
    // Yesterday's delivery must not suppress today's different target date.
    let target = NaiveTime::from_hms_opt(20, 0, 0).unwrap();
    let stale_checked_date = today(); // was "tomorrow" relative to yesterday
    let result =
        should_start_new_reminder(today(), target, target, Some(stale_checked_date), false);
    assert_eq!(result, Some(today() + chrono::Duration::days(1)));
}

// ── Reminder delivery: duplicate prevention across rooms/restarts ────────

fn pending(sent: &[&str], attempt: u32) -> PendingReminder {
    pending_at(sent, attempt, reminder_now_utc())
}

fn pending_at(sent: &[&str], attempt: u32, created_at: DateTime<chrono::Utc>) -> PendingReminder {
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
fn reminder_transaction_id_is_stable_per_date_pickups_and_room() {
    let p = pending(&[], 0);
    assert_eq!(
        reminder_transaction_id(&p, "!room:example.org"),
        reminder_transaction_id(&p, "!room:example.org")
    );
    assert_ne!(
        reminder_transaction_id(&p, "!room:example.org"),
        reminder_transaction_id(&p, "!other:example.org")
    );
}

#[test]
fn is_fully_delivered_true_only_when_every_joined_room_has_it() {
    let joined = vec!["!a:x".to_owned(), "!b:x".to_owned()];
    assert!(!is_fully_delivered(&pending(&["!a:x"], 1), &joined));
    assert!(is_fully_delivered(&pending(&["!a:x", "!b:x"], 1), &joined));
}

#[test]
fn is_fully_delivered_false_with_no_joined_rooms() {
    // Not "vacuously delivered". There must be an actual room that got it.
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
    assert!(
        !path.exists(),
        "corrupt file must not be overwritten in place"
    );
    let mut entries = fs::read_dir(&dir).await.unwrap();
    let backup = entries.next_entry().await.unwrap().unwrap();
    assert!(backup
        .file_name()
        .to_string_lossy()
        .starts_with("reminder_state.corrupt-"));
}

#[tokio::test]
async fn persisted_state_round_trips_through_save_and_load() {
    let dir = std::env::temp_dir().join(format!("garbage-bot-test-{}", uuid_ish()));
    fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("reminder_state.json");

    let created_at = reminder_now_utc();
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
    // created_at is new: defaults to "now" rather than failing to parse
    // or silently expiring an in-flight reminder on the next tick.
    assert!(
        (reminder_now_utc() - pending.created_at)
            .num_seconds()
            .abs()
            < 5
    );
}

// ── Reminder lateness cap ─────────────────────────────────────────────────

#[test]
fn is_pending_expired_false_within_max_lateness() {
    let max_lateness = chrono::Duration::hours(12);
    let now = reminder_now();
    let mut p = pending_at(&[], 3, now.to_utc() - chrono::Duration::hours(11));
    p.date = now.date_naive() + chrono::Duration::days(1);
    assert!(!is_pending_expired(&p, now, max_lateness));
}

#[test]
fn is_pending_expired_true_beyond_max_lateness() {
    let max_lateness = chrono::Duration::hours(12);
    let now = reminder_now();
    let mut p = pending_at(&[], 20, now.to_utc() - chrono::Duration::hours(13));
    p.date = now.date_naive() + chrono::Duration::days(1);
    assert!(is_pending_expired(&p, now, max_lateness));
}

#[test]
fn is_pending_expired_fresh_reminder_never_expired() {
    let max_lateness = chrono::Duration::minutes(0); // pathological but valid config
    let now = reminder_now();
    let mut p = pending_at(&[], 0, now.to_utc());
    p.date = now.date_naive() + chrono::Duration::days(1);
    assert!(!is_pending_expired(&p, now, max_lateness)); // exactly at the boundary, not over it
}

#[test]
fn pending_reminder_expires_at_start_of_collection_day() {
    let now = reminder_now();
    let mut p = pending_at(&[], 0, now.to_utc());
    p.date = now.date_naive();
    assert!(is_pending_expired(&p, now, chrono::Duration::hours(24)));
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

/// Unique per call: tests run in parallel, and two of them reading the
/// clock in the same nanosecond used to share one state file.
fn uuid_ish() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{nanos}-{n}", std::process::id())
}

// ── Confirmations and follow-up ───────────────────────────────────────────

fn posted_at(sent_at: DateTime<chrono::Utc>, date: NaiveDate) -> PostedReminder {
    PostedReminder {
        date,
        pickups: vec![Pickup {
            date,
            label: "Paper (blue)".to_owned(),
        }],
        event_ids: HashMap::from([("!a:x".to_owned(), "$evt".to_owned())]),
        sent_at,
        confirmations: Vec::new(),
        followup_sent: false,
        followup_at: None,
        followup_event_ids: HashMap::new(),
    }
}

fn berlin(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<chrono::Utc> {
    REMINDER_TZ
        .with_ymd_and_hms(y, m, d, h, min, 0)
        .unwrap()
        .to_utc()
}

#[test]
fn confirmed_reminder_replaces_call_to_action_with_who_did_it() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let mut posted = posted_at(berlin(2026, 8, 9, 20, 0), date);
    assert!(add_confirmation(
        &mut posted,
        "@a:x",
        "Alice",
        berlin(2026, 8, 9, 21, 14),
        "$r1"
    ));
    assert!(!add_confirmation(
        &mut posted,
        "@a:x",
        "Alice",
        berlin(2026, 8, 9, 21, 20),
        "$r2"
    ));
    assert!(add_confirmation(
        &mut posted,
        "@b:x",
        "Bob <3",
        berlin(2026, 8, 9, 21, 30),
        "$r3"
    ));

    let names = posted.confirmed_by().unwrap();
    let (plain, html) = build_reminder(&posted.pickups, date, &reminder_colors(), Some(&names));

    assert!(plain.ends_with("✅ Done by Alice (21:14), Bob <3 (21:30)"));
    assert!(!plain.contains("@room"));
    assert!(html.ends_with("✅ <strong>Done</strong> by Alice (21:14), Bob &lt;3 (21:30)"));
}

#[test]
fn followup_is_due_once_when_nobody_confirmed() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let after = chrono::Duration::minutes(120);
    let mut posted = posted_at(berlin(2026, 8, 9, 20, 0), date);

    assert_eq!(
        followup_due_at(&posted, after),
        Some(berlin(2026, 8, 9, 22, 0))
    );

    posted.followup_sent = true;
    assert_eq!(followup_due_at(&posted, after), None);

    posted.followup_sent = false;
    add_confirmation(
        &mut posted,
        "@a:x",
        "Alice",
        berlin(2026, 8, 9, 21, 0),
        "$r4",
    );
    assert_eq!(followup_due_at(&posted, after), None);
}

#[test]
fn no_followup_once_collection_morning_has_started() {
    // A reminder recovered late at 05:00 on collection day would nudge
    // at 07:00, after the crews have come.
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let posted = posted_at(berlin(2026, 8, 10, 5, 0), date);

    assert_eq!(
        followup_due_at(&posted, chrono::Duration::minutes(120)),
        None
    );
}

#[test]
fn followup_says_this_morning_after_midnight() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let posted = posted_at(berlin(2026, 8, 9, 23, 0), date);
    let evening = berlin(2026, 8, 9, 23, 30).with_timezone(&REMINDER_TZ);
    let night = berlin(2026, 8, 10, 1, 0).with_timezone(&REMINDER_TZ);

    assert!(followup_text(&posted, evening)
        .0
        .contains("tomorrow morning"));
    assert!(followup_text(&posted, night).0.contains("this morning"));
}

#[test]
fn record_posted_extends_the_same_reminder_and_replaces_an_older_one() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let mut persisted = PersistedState::default();
    let pickups = vec![pickup(1, "Paper")];

    record_posted(
        &mut persisted,
        &pickups,
        date,
        vec![("!a:x".into(), "$1".into())],
    );
    add_confirmation(
        persisted.posted.as_mut().unwrap(),
        "@a:x",
        "Alice",
        reminder_now_utc(),
        "$r5",
    );
    record_posted(
        &mut persisted,
        &pickups,
        date,
        vec![("!b:x".into(), "$2".into())],
    );

    let posted = persisted.posted.as_ref().unwrap();
    assert_eq!(posted.event_ids.len(), 2);
    assert_eq!(posted.confirmations.len(), 1);
    assert!(posted.is_reminder_event("$2"));

    let next = date + chrono::Duration::days(7);
    record_posted(
        &mut persisted,
        &pickups,
        next,
        vec![("!a:x".into(), "$3".into())],
    );
    let posted = persisted.posted.as_ref().unwrap();
    assert_eq!(posted.date, next);
    assert!(posted.confirmations.is_empty());
    assert!(!posted.is_reminder_event("$1"));

    record_posted(&mut persisted, &pickups, next, Vec::new());
    assert_eq!(persisted.posted.as_ref().unwrap().event_ids.len(), 1);
}

#[test]
fn status_shows_confirmation_state_of_the_last_reminder() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let mut persisted = PersistedState {
        last_sent_date: Some(date),
        posted: Some(posted_at(berlin(2026, 8, 9, 20, 0), date)),
        ..Default::default()
    };
    assert_eq!(
        describe_reminder_state(&persisted),
        "last sent 2026-08-10 · no ✅ yet"
    );

    let posted = persisted.posted.as_mut().unwrap();
    add_confirmation(posted, "@a:x", "Alice", berlin(2026, 8, 9, 21, 14), "$r6");
    assert_eq!(
        describe_reminder_state(&persisted),
        "last sent 2026-08-10 · ✅ Alice (21:14)"
    );
}

#[test]
fn retracting_the_last_check_withdraws_the_confirmation() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let mut posted = posted_at(berlin(2026, 8, 9, 20, 0), date);
    let at = berlin(2026, 8, 9, 21, 0);
    add_confirmation(&mut posted, "@a:x", "Alice", at, "$check");
    add_confirmation(&mut posted, "@a:x", "Alice", at, "$tick"); // ✅ and ✔
    add_confirmation(&mut posted, "@b:x", "Bob", at, "$bob");

    assert!(!remove_confirmation_reaction(&mut posted, "$check"));
    assert_eq!(posted.confirmations.len(), 2);
    assert!(remove_confirmation_reaction(&mut posted, "$tick"));
    assert_eq!(posted.confirmed_by().unwrap(), "Bob (21:00)");
    assert!(!remove_confirmation_reaction(&mut posted, "$unrelated"));
    assert!(remove_confirmation_reaction(&mut posted, "$bob"));
    assert!(posted.confirmed_by().is_none());

    let (plain, _) = build_reminder(&posted.pickups, date, &reminder_colors(), None);
    assert!(plain.ends_with("Tap ✅ when done."));
}

#[test]
fn followup_turns_into_an_all_clear_once_confirmed() {
    let date = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let mut posted = posted_at(berlin(2026, 8, 9, 20, 0), date);
    let at = berlin(2026, 8, 9, 22, 0).with_timezone(&REMINDER_TZ);
    assert!(followup_text(&posted, at).0.starts_with("⏰ @room Nobody"));

    add_confirmation(
        &mut posted,
        "@a:x",
        "Alice <3",
        berlin(2026, 8, 9, 22, 5),
        "$r",
    );
    let (plain, html) = followup_text(&posted, at);

    assert_eq!(plain, "✅ Bins are out. Thanks, Alice <3 (22:05)! 🙌");
    assert!(html.contains("Alice &lt;3"));
}

#[test]
fn state_written_before_followup_tracking_still_loads() {
    let json = r#"{"date":"2026-08-10","pickups":[],"event_ids":{"!a:x":"$1"},
        "sent_at":"2026-08-09T18:00:00Z",
        "confirmations":[{"user_id":"@a:x","name":"A","at":"2026-08-09T19:00:00Z"}],
        "followup_sent":true}"#;

    let posted: PostedReminder = serde_json::from_str(json).unwrap();

    assert!(posted.followup_at.is_none());
    assert!(posted.confirmations[0].reaction_ids.is_empty());
}
