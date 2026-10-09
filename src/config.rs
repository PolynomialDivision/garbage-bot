//! Configuration file: `[bsr]`, `[berlin_recycling]`, `[reminder]`, labels.

use super::*;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct Config {
    pub(crate) matrix: MatrixConfig,
    pub(crate) bsr: BsrConfig,
    #[serde(default)]
    pub(crate) berlin_recycling: Option<BerlinRecyclingConfig>,
    #[serde(default)]
    pub(crate) reminder: ReminderConfig,
    #[serde(default)]
    pub(crate) waste_labels: HashMap<String, String>,
    #[serde(default)]
    pub(crate) waste_colors: HashMap<String, String>,
    #[serde(default)]
    pub(crate) security: SecurityConfig,
}

#[derive(Deserialize)]
pub(crate) struct BerlinRecyclingConfig {
    pub(crate) username: String,
    pub(crate) password: String,
}

#[derive(Deserialize)]
pub(crate) struct BsrConfig {
    pub(crate) schedule_id: Option<String>,
    pub(crate) street: Option<String>,
    pub(crate) number: Option<String>,
    pub(crate) plz: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ReminderConfig {
    #[serde(default = "default_reminder_time")]
    pub(crate) reminder_time: TimeOfDay,
    /// How long a reminder keeps retrying after becoming due before it's
    /// given up on as stale. A reminder says "put them out tonight". Once
    /// it's this late, that's no longer true, so delivering it would just be
    /// misleading rather than helpful.
    #[serde(default = "default_max_lateness_minutes")]
    pub(crate) max_lateness_minutes: u32,
    /// Minutes after the reminder at which one follow-up is posted if nobody
    /// has confirmed with ✅ yet. 0 disables it.
    #[serde(default = "default_followup_minutes")]
    pub(crate) followup_minutes: u32,
}

pub(crate) fn default_followup_minutes() -> u32 {
    120 // a 20:00 reminder follows up at 22:00
}

pub(crate) fn default_reminder_time() -> TimeOfDay {
    TimeOfDay::new(20, 0)
}

pub(crate) fn default_max_lateness_minutes() -> u32 {
    12 * 60 // 12h: e.g. a 20:00 reminder gives up at 08:00 the next morning.
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            reminder_time: default_reminder_time(),
            max_lateness_minutes: default_max_lateness_minutes(),
            followup_minutes: default_followup_minutes(),
        }
    }
}
