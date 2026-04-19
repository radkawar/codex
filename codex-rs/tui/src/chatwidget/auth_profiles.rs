use crate::status::format_reset_timestamp;
use crate::status::plan_type_display_name;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::AuthProfileListResponse;
use codex_app_server_protocol::AuthProfileSummary;
use codex_app_server_protocol::RateLimitWindow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const PROFILE_COLUMN_MAX_WIDTH: usize = 18;
const ACCOUNT_COLUMN_MAX_WIDTH: usize = 32;
const PLAN_COLUMN_MAX_WIDTH: usize = 10;
const WINDOW_LEFT_COLUMN_WIDTH: usize = 11;
const WINDOW_RESET_COLUMN_WIDTH: usize = 16;

struct AuthProfileTableRow {
    current: String,
    profile: String,
    account: String,
    plan: String,
    five_hour_left: String,
    five_hour_reset: String,
    weekly_left: String,
    weekly_reset: String,
}

pub(super) fn format_auth_profiles_output(response: &AuthProfileListResponse) -> String {
    let captured_at = Local::now();
    let rows = response
        .profiles
        .iter()
        .map(|profile| auth_profile_table_row(profile, captured_at))
        .collect::<Vec<_>>();

    let current_width = column_width("CUR", rows.iter().map(|row| row.current.as_str()), 3);
    let profile_width = column_width(
        "PROFILE",
        rows.iter().map(|row| row.profile.as_str()),
        PROFILE_COLUMN_MAX_WIDTH,
    );
    let account_width = column_width(
        "ACCOUNT",
        rows.iter().map(|row| row.account.as_str()),
        ACCOUNT_COLUMN_MAX_WIDTH,
    );
    let plan_width = column_width(
        "PLAN",
        rows.iter().map(|row| row.plan.as_str()),
        PLAN_COLUMN_MAX_WIDTH,
    );
    let five_hour_left_width = column_width(
        "5H LEFT",
        rows.iter().map(|row| row.five_hour_left.as_str()),
        WINDOW_LEFT_COLUMN_WIDTH,
    );
    let five_hour_reset_width = column_width(
        "5H RESET",
        rows.iter().map(|row| row.five_hour_reset.as_str()),
        WINDOW_RESET_COLUMN_WIDTH,
    );
    let weekly_left_width = column_width(
        "WEEKLY LEFT",
        rows.iter().map(|row| row.weekly_left.as_str()),
        WINDOW_LEFT_COLUMN_WIDTH,
    );
    let weekly_reset_width = column_width(
        "WEEKLY RESET",
        rows.iter().map(|row| row.weekly_reset.as_str()),
        WINDOW_RESET_COLUMN_WIDTH,
    );

    let header = join_table_cells([
        pad_table_cell("CUR", current_width),
        pad_table_cell("PROFILE", profile_width),
        pad_table_cell("ACCOUNT", account_width),
        pad_table_cell("PLAN", plan_width),
        pad_table_cell("5H LEFT", five_hour_left_width),
        pad_table_cell("5H RESET", five_hour_reset_width),
        pad_table_cell("WEEKLY LEFT", weekly_left_width),
        pad_table_cell("WEEKLY RESET", weekly_reset_width),
    ]);
    let divider = join_table_cells([
        "-".repeat(current_width),
        "-".repeat(profile_width),
        "-".repeat(account_width),
        "-".repeat(plan_width),
        "-".repeat(five_hour_left_width),
        "-".repeat(five_hour_reset_width),
        "-".repeat(weekly_left_width),
        "-".repeat(weekly_reset_width),
    ]);
    let body = rows
        .iter()
        .map(|row| {
            join_table_cells([
                pad_table_cell(&row.current, current_width),
                pad_table_cell(&row.profile, profile_width),
                pad_table_cell(&row.account, account_width),
                pad_table_cell(&row.plan, plan_width),
                pad_table_cell(&row.five_hour_left, five_hour_left_width),
                pad_table_cell(&row.five_hour_reset, five_hour_reset_width),
                pad_table_cell(&row.weekly_left, weekly_left_width),
                pad_table_cell(&row.weekly_reset, weekly_reset_width),
            ])
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("Saved auth profiles (CUR = current auth match):\n{header}\n{divider}\n{body}")
}

fn auth_profile_table_row(
    profile: &AuthProfileSummary,
    captured_at: DateTime<Local>,
) -> AuthProfileTableRow {
    let (account, plan) = match &profile.account {
        Some(Account::ApiKey {}) => ("API key".to_string(), "-".to_string()),
        Some(Account::Chatgpt { email, plan_type }) => {
            (email.clone(), plan_type_display_name(*plan_type))
        }
        None => ("unknown".to_string(), "-".to_string()),
    };

    let five_hour = profile
        .rate_limits
        .as_ref()
        .and_then(|limits| limits.primary.as_ref());
    let weekly = profile
        .rate_limits
        .as_ref()
        .and_then(|limits| limits.secondary.as_ref());

    AuthProfileTableRow {
        current: if profile.active {
            "yes".to_string()
        } else {
            "no".to_string()
        },
        profile: profile.name.clone(),
        account,
        plan,
        five_hour_left: format_window_left(five_hour),
        five_hour_reset: format_window_reset(five_hour, captured_at),
        weekly_left: format_window_left(weekly),
        weekly_reset: format_window_reset(weekly, captured_at),
    }
}

fn format_window_left(window: Option<&RateLimitWindow>) -> String {
    match window {
        Some(window) => format!("{}%", (100 - window.used_percent).clamp(0, 100)),
        None => "-".to_string(),
    }
}

fn format_window_reset(window: Option<&RateLimitWindow>, captured_at: DateTime<Local>) -> String {
    window
        .and_then(|window| window.resets_at)
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|dt| format_reset_timestamp(dt.with_timezone(&Local), captured_at))
        .unwrap_or_else(|| "-".to_string())
}

fn column_width<'a>(
    header: &str,
    values: impl Iterator<Item = &'a str>,
    max_width: usize,
) -> usize {
    let content_width = values
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .max(UnicodeWidthStr::width(header));
    content_width.min(max_width)
}

fn join_table_cells(cells: impl IntoIterator<Item = String>) -> String {
    cells.into_iter().collect::<Vec<_>>().join("  ")
}

fn pad_table_cell(text: &str, width: usize) -> String {
    let truncated = truncate_display_width(text, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(truncated.as_str()));
    format!("{truncated}{}", " ".repeat(padding))
}

fn truncate_display_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > max_width.saturating_sub(1) {
            break;
        }
        out.push_str(grapheme);
        used += width;
    }
    out.push('…');
    out
}
