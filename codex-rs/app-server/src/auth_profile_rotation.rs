use codex_app_server_protocol::Account;
use codex_app_server_protocol::AuthProfileSummary;
use codex_app_server_protocol::RateLimitWindow;

const TOP_SCORE_BAND_PERCENT: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AvailableCandidate {
    index: usize,
    score: u8,
}

pub(crate) fn select_next_auth_profile(
    profiles: &[AuthProfileSummary],
) -> Option<&AuthProfileSummary> {
    let anchor = profiles
        .iter()
        .enumerate()
        .filter_map(|(index, profile)| profile.active.then_some(index))
        .next_back();

    let available_candidates = profiles
        .iter()
        .enumerate()
        .filter_map(|(index, profile)| available_candidate(index, profile))
        .collect::<Vec<_>>();
    if !available_candidates.is_empty() {
        let top_score = available_candidates
            .iter()
            .map(|candidate| candidate.score)
            .max()
            .unwrap_or(0);
        let threshold = top_score.saturating_sub(TOP_SCORE_BAND_PERCENT);
        let eligible_indices = available_candidates
            .iter()
            .filter_map(|candidate| (candidate.score >= threshold).then_some(candidate.index))
            .collect::<Vec<_>>();
        if let Some(index) = select_next_index(&eligible_indices, anchor) {
            return profiles.get(index);
        }
    }

    let unknown_indices = profiles
        .iter()
        .enumerate()
        .filter_map(|(index, profile)| unknown_candidate(index, profile))
        .collect::<Vec<_>>();
    select_next_index(&unknown_indices, anchor).and_then(|index| profiles.get(index))
}

fn available_candidate(index: usize, profile: &AuthProfileSummary) -> Option<AvailableCandidate> {
    if profile.active || matches!(profile.account, Some(Account::ApiKey {})) {
        return None;
    }

    let rate_limits = profile.rate_limits.as_ref()?;
    let primary_left = window_left_percent(rate_limits.primary.as_ref())?;
    let secondary_left = window_left_percent(rate_limits.secondary.as_ref())?;
    if primary_left == 0 || secondary_left == 0 {
        return None;
    }

    Some(AvailableCandidate {
        index,
        score: primary_left.min(secondary_left),
    })
}

fn unknown_candidate(index: usize, profile: &AuthProfileSummary) -> Option<usize> {
    if profile.active || matches!(profile.account, Some(Account::ApiKey {})) {
        return None;
    }

    if profile
        .rate_limits
        .as_ref()
        .is_some_and(|rate_limits| rate_limits.primary.is_some() && rate_limits.secondary.is_some())
    {
        return None;
    }

    Some(index)
}

fn window_left_percent(window: Option<&RateLimitWindow>) -> Option<u8> {
    window.map(|window| {
        let used_percent = window.used_percent.clamp(0, 100);
        (100 - used_percent) as u8
    })
}

fn select_next_index(indices: &[usize], anchor: Option<usize>) -> Option<usize> {
    if indices.is_empty() {
        return None;
    }

    if let Some(anchor) = anchor
        && let Some(index) = indices.iter().copied().find(|index| *index > anchor)
    {
        return Some(index);
    }

    indices.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::RateLimitSnapshot;
    use codex_protocol::account::PlanType;

    fn chatgpt_profile(
        name: &str,
        active: bool,
        primary_used_percent: Option<u8>,
        secondary_used_percent: Option<u8>,
    ) -> AuthProfileSummary {
        AuthProfileSummary {
            name: name.to_string(),
            account: Some(Account::Chatgpt {
                email: format!("{name}@example.com"),
                plan_type: PlanType::Pro,
            }),
            rate_limits: Some(RateLimitSnapshot {
                limit_id: Some("codex".to_string()),
                limit_name: None,
                plan_type: Some(PlanType::Pro),
                primary: primary_used_percent.map(|used_percent| {
                    codex_app_server_protocol::RateLimitWindow {
                        window_duration_mins: Some(300),
                        used_percent: used_percent.into(),
                        resets_at: Some(1_700_000_000),
                    }
                }),
                secondary: secondary_used_percent.map(|used_percent| {
                    codex_app_server_protocol::RateLimitWindow {
                        window_duration_mins: Some(10_080),
                        used_percent: used_percent.into(),
                        resets_at: Some(1_700_000_000),
                    }
                }),
                credits: None,
                rate_limit_reached_type: None,
            }),
            active,
        }
    }

    #[test]
    fn selects_next_profile_within_top_capacity_band() {
        let profiles = vec![
            chatgpt_profile("active", true, Some(20), Some(20)),
            chatgpt_profile("best", false, Some(2), Some(3)),
            chatgpt_profile("also-best", false, Some(5), Some(4)),
            chatgpt_profile("low", false, Some(55), Some(80)),
        ];

        let selected =
            select_next_auth_profile(&profiles).expect("a next auth profile should be selected");

        assert_eq!(selected.name, "best");
    }

    #[test]
    fn skips_exhausted_profiles() {
        let profiles = vec![
            chatgpt_profile("active", true, Some(100), Some(100)),
            chatgpt_profile("exhausted", false, Some(100), Some(20)),
            chatgpt_profile("available", false, Some(10), Some(10)),
        ];

        let selected =
            select_next_auth_profile(&profiles).expect("a next auth profile should be selected");

        assert_eq!(selected.name, "available");
    }

    #[test]
    fn falls_back_to_unknown_profiles_when_no_scored_profile_exists() {
        let mut unknown = chatgpt_profile("unknown", false, None, None);
        unknown.rate_limits = Some(codex_app_server_protocol::RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: Some(PlanType::Pro),
            primary: None,
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
        });

        let profiles = vec![
            chatgpt_profile("active", true, Some(100), Some(100)),
            unknown,
        ];

        let selected = select_next_auth_profile(&profiles)
            .expect("a fallback auth profile should be selected");

        assert_eq!(selected.name, "unknown");
    }

    #[test]
    fn returns_none_when_no_alternative_profile_is_available() {
        let profiles = vec![chatgpt_profile("active", true, Some(10), Some(10))];

        assert_eq!(select_next_auth_profile(&profiles), None);
    }
}
