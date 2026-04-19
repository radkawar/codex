use crate::status::format_reset_timestamp;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::AccountPrimingProfileOutcome;
use codex_app_server_protocol::AccountPrimingProfileResult;
use codex_app_server_protocol::AccountPrimingRunSummary;
use codex_app_server_protocol::AccountPrimingStatus;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::RateLimitWindow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const PROFILE_COLUMN_MAX_WIDTH: usize = 18;
const ACCOUNT_COLUMN_MAX_WIDTH: usize = 30;
const RESULT_COLUMN_MAX_WIDTH: usize = 14;
const RESET_COLUMN_WIDTH: usize = 16;
const DETAIL_COLUMN_MAX_WIDTH: usize = 40;

struct AccountPrimingRow {
    profile: String,
    account: String,
    result: String,
    five_hour_reset: String,
    weekly_reset: String,
    detail: String,
}

pub(super) fn format_account_priming_status_output(status: &AccountPrimingStatus) -> String {
    let state = if status.running { "running" } else { "stopped" };
    let interval = status
        .interval_seconds
        .map(|seconds| format!("{seconds}s"))
        .unwrap_or_else(|| "-".to_string());
    let started = status
        .started_at
        .map(format_timestamp)
        .unwrap_or_else(|| "-".to_string());
    let current_run = status
        .current_run_started_at
        .map(format_timestamp)
        .unwrap_or_else(|| "-".to_string());
    let current_profile = status
        .current_profile_name
        .clone()
        .unwrap_or_else(|| "-".to_string());
    let last_run = status
        .last_run
        .as_ref()
        .map(format_run_brief)
        .unwrap_or_else(|| "none".to_string());

    let mut output = format!(
        "Account priming\nSTATE: {state}\nINTERVAL: {interval}\nSTARTED: {started}\nCURRENT RUN: {current_run}\nCURRENT PROFILE: {current_profile}\nLAST RUN: {last_run}"
    );
    if let Some(last_run) = status.last_run.as_ref() {
        output.push_str("\n\n");
        output.push_str(&format_account_priming_run_output(last_run));
    }
    output
}

pub(super) fn format_account_priming_run_output(summary: &AccountPrimingRunSummary) -> String {
    let captured_at = Local::now();
    let rows = summary
        .results
        .iter()
        .map(|result| account_priming_row(result, captured_at))
        .collect::<Vec<_>>();

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
    let result_width = column_width(
        "RESULT",
        rows.iter().map(|row| row.result.as_str()),
        RESULT_COLUMN_MAX_WIDTH,
    );
    let five_hour_reset_width = column_width(
        "5H RESET",
        rows.iter().map(|row| row.five_hour_reset.as_str()),
        RESET_COLUMN_WIDTH,
    );
    let weekly_reset_width = column_width(
        "WEEKLY RESET",
        rows.iter().map(|row| row.weekly_reset.as_str()),
        RESET_COLUMN_WIDTH,
    );
    let detail_width = column_width(
        "DETAIL",
        rows.iter().map(|row| row.detail.as_str()),
        DETAIL_COLUMN_MAX_WIDTH,
    );

    let header = join_table_cells([
        pad_table_cell("PROFILE", profile_width),
        pad_table_cell("ACCOUNT", account_width),
        pad_table_cell("RESULT", result_width),
        pad_table_cell("5H RESET", five_hour_reset_width),
        pad_table_cell("WEEKLY RESET", weekly_reset_width),
        pad_table_cell("DETAIL", detail_width),
    ]);
    let divider = join_table_cells([
        "-".repeat(profile_width),
        "-".repeat(account_width),
        "-".repeat(result_width),
        "-".repeat(five_hour_reset_width),
        "-".repeat(weekly_reset_width),
        "-".repeat(detail_width),
    ]);
    let body = rows
        .iter()
        .map(|row| {
            join_table_cells([
                pad_table_cell(&row.profile, profile_width),
                pad_table_cell(&row.account, account_width),
                pad_table_cell(&row.result, result_width),
                pad_table_cell(&row.five_hour_reset, five_hour_reset_width),
                pad_table_cell(&row.weekly_reset, weekly_reset_width),
                pad_table_cell(&row.detail, detail_width),
            ])
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Last priming run: {} | primed {} | active {} | unsupported {} | failed {}{}\n{header}\n{divider}\n{body}",
        format_timestamp(summary.completed_at),
        summary.primed_count,
        summary.already_active_count,
        summary.unsupported_count,
        summary.failed_count,
        if summary.cancelled {
            " | cancelled"
        } else {
            ""
        },
    )
}

fn format_run_brief(summary: &AccountPrimingRunSummary) -> String {
    format!(
        "{} | primed {} | active {} | unsupported {} | failed {}{}",
        format_timestamp(summary.completed_at),
        summary.primed_count,
        summary.already_active_count,
        summary.unsupported_count,
        summary.failed_count,
        if summary.cancelled {
            " | cancelled"
        } else {
            ""
        },
    )
}

fn account_priming_row(
    result: &AccountPrimingProfileResult,
    captured_at: DateTime<Local>,
) -> AccountPrimingRow {
    let account = match &result.account {
        Some(Account::ApiKey {}) => "API key".to_string(),
        Some(Account::Chatgpt { email, .. }) => email.clone(),
        None => "-".to_string(),
    };
    let snapshot = result
        .after_rate_limits
        .as_ref()
        .or(result.before_rate_limits.as_ref());

    AccountPrimingRow {
        profile: result.profile_name.clone(),
        account,
        result: outcome_label(result.outcome).to_string(),
        five_hour_reset: format_window_reset(
            snapshot.and_then(|snapshot| snapshot.primary.as_ref()),
            captured_at,
        ),
        weekly_reset: format_window_reset(
            snapshot.and_then(|snapshot| snapshot.secondary.as_ref()),
            captured_at,
        ),
        detail: result
            .error
            .clone()
            .unwrap_or_else(|| detail_from_snapshot(snapshot)),
    }
}

fn detail_from_snapshot(snapshot: Option<&RateLimitSnapshot>) -> String {
    match snapshot {
        Some(snapshot) if snapshot.primary.is_none() || snapshot.secondary.is_none() => {
            "window inactive".to_string()
        }
        Some(_) => "-".to_string(),
        None => "-".to_string(),
    }
}

fn outcome_label(outcome: AccountPrimingProfileOutcome) -> &'static str {
    match outcome {
        AccountPrimingProfileOutcome::Primed => "primed",
        AccountPrimingProfileOutcome::AlreadyActive => "active",
        AccountPrimingProfileOutcome::UnsupportedAuth => "unsupported",
        AccountPrimingProfileOutcome::Failed => "failed",
    }
}

fn format_window_reset(window: Option<&RateLimitWindow>, captured_at: DateTime<Local>) -> String {
    window
        .and_then(|window| window.resets_at)
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|dt| format_reset_timestamp(dt.with_timezone(&Local), captured_at))
        .unwrap_or_else(|| "-".to_string())
}

fn format_timestamp(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        })
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
