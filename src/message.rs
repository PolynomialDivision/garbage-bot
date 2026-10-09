//! Formatting of the reminder message.

use super::*;

// ── Message formatting ────────────────────────────────────────────────────────

/// Map a `#RRGGBB` hex color to the closest colored-circle emoji.
pub(crate) fn hex_to_dot(hex: &str) -> &'static str {
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

/// The reminder message. Once someone confirmed, `confirmed_by` replaces the
/// call to action, so the message itself shows that it is taken care of.
pub(crate) fn build_reminder(
    pickups: &[Pickup],
    pickup_date: NaiveDate,
    colors: &HashMap<String, String>,
    confirmed_by: Option<&str>,
) -> (String, String) {
    let day = pickup_date.format("%A, %-d %b").to_string();
    let day_pickups: Vec<&Pickup> = pickups
        .iter()
        .filter(|pickup| pickup.date == pickup_date)
        .collect();

    let plain_bins = day_pickups
        .iter()
        .map(|pickup| {
            let dot = colors
                .get(&pickup.label)
                .map(|color| hex_to_dot(color))
                .unwrap_or("🗑️");
            format!("{dot} {}", pickup.label)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let html_bins = day_pickups
        .iter()
        .map(|pickup| {
            let dot = colors
                .get(&pickup.label)
                .map(|color| hex_to_dot(color))
                .unwrap_or("🗑️");
            format!("{dot} {}", escape_html(&pickup.label))
        })
        .collect::<Vec<_>>()
        .join("<br>");
    let bin_word = if day_pickups.len() == 1 {
        "bin"
    } else {
        "bins"
    };

    let (footer_plain, footer_html) = match confirmed_by {
        Some(names) => (
            format!("✅ Done by {names}"),
            format!("✅ <strong>Done</strong> by {}", escape_html(names)),
        ),
        None => (
            format!("@room 🌙 Put the {bin_word} out tonight.\n👉 Tap ✅ when done."),
            format!("@room 🌙 Put the {bin_word} out tonight.<br>👉 Tap ✅ when done."),
        ),
    };
    let plain = format!("🚛 Garbage pickup tomorrow\n📅 {day}\n\n{plain_bins}\n\n{footer_plain}");
    let html = format!(
        "🚛 <strong>Garbage pickup tomorrow</strong><br>📅 <strong>{day}</strong><br><br>{html_bins}<br><br>{footer_html}"
    );
    (plain, html)
}

pub(crate) fn strip_variation_selectors(s: &str) -> String {
    s.chars()
        .filter(|&c| !('\u{FE00}'..='\u{FE0F}').contains(&c))
        .collect()
}

pub(crate) const DONE_REACTIONS: &[&str] = &["✅", "✔", "☑"];
