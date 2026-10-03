//! Eases usage percentages from their previous value when a refresh changes them.

use super::*;

pub(super) const USAGE_CHANGE_ANIMATION_DURATION: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) struct UsagePercentKey {
    pub(super) account_id: AccountId,
    pub(super) field: String,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct UsagePercentTransition {
    pub(super) from_remaining: f64,
    pub(super) to_remaining: f64,
    pub(super) started_at: Instant,
}

impl UsagePercentTransition {
    pub(super) fn value_at(self, now: Instant) -> f64 {
        let progress = (now.saturating_duration_since(self.started_at).as_secs_f64()
            / USAGE_CHANGE_ANIMATION_DURATION.as_secs_f64())
        .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        self.from_remaining + (self.to_remaining - self.from_remaining) * eased
    }

    pub(super) fn is_active(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < USAGE_CHANGE_ANIMATION_DURATION
    }
}

#[derive(Default)]
pub(super) struct UsageAnimationState {
    pub(super) transitions: HashMap<UsagePercentKey, UsagePercentTransition>,
}

impl UsageAnimationState {
    pub(super) fn update(
        &mut self,
        previous_entries: &[AccountUsageEntry],
        next_entries: &[AccountUsageEntry],
        now: Instant,
    ) {
        let previous_values = collect_usage_percent_values(previous_entries);
        let next_values = collect_usage_percent_values(next_entries);
        let previous_transitions = std::mem::take(&mut self.transitions);
        let mut next_transitions = HashMap::new();

        for (key, target) in next_values {
            let Some(previous_target) = previous_values.get(&key).copied() else {
                continue;
            };
            let active_transition = previous_transitions
                .get(&key)
                .copied()
                .filter(|transition| transition.is_active(now));

            if usage_percent_is_close(previous_target, target) {
                if let Some(transition) = active_transition
                    .filter(|transition| usage_percent_is_close(transition.to_remaining, target))
                {
                    next_transitions.insert(key, transition);
                }
                continue;
            }

            let from = active_transition
                .map(|transition| transition.value_at(now))
                .unwrap_or(previous_target);
            if !usage_percent_is_close(from, target) {
                next_transitions.insert(
                    key,
                    UsagePercentTransition {
                        from_remaining: from,
                        to_remaining: target,
                        started_at: now,
                    },
                );
            }
        }

        self.transitions = next_transitions;
    }

    /// Same as `update`, for a single account, leaving every other account's
    /// running transitions untouched.
    pub(super) fn update_account(
        &mut self,
        previous: &AccountUsageEntry,
        next: &AccountUsageEntry,
        now: Instant,
    ) {
        let account_id = next.account.id;
        let mut others = std::mem::take(&mut self.transitions);
        self.transitions = others
            .iter()
            .filter(|(key, _)| key.account_id == account_id)
            .map(|(key, transition)| (key.clone(), *transition))
            .collect();
        others.retain(|key, _| key.account_id != account_id);
        self.update(
            std::slice::from_ref(previous),
            std::slice::from_ref(next),
            now,
        );
        self.transitions.extend(others);
    }

    pub(super) fn is_active(&self) -> bool {
        !self.transitions.is_empty()
    }

    pub(super) fn advance(&mut self, now: Instant) {
        self.transitions
            .retain(|_, transition| transition.is_active(now));
    }

    pub(super) fn has_transitions_for(&self, account_id: AccountId) -> bool {
        self.transitions
            .keys()
            .any(|key| key.account_id == account_id)
    }

    pub(super) fn animated_snapshot(
        &self,
        account_id: AccountId,
        snapshot: &UsageSnapshot,
    ) -> Option<UsageSnapshot> {
        if snapshot.account_id != account_id || !self.has_transitions_for(account_id) {
            return None;
        }

        let now = Instant::now();
        let mut animated = snapshot.clone();
        if let Some(window) = &mut animated.primary {
            animate_used_percent(
                &self.transitions,
                account_id,
                "window:primary",
                &mut window.used_percent,
                now,
            );
        }
        if let Some(window) = &mut animated.secondary {
            animate_used_percent(
                &self.transitions,
                account_id,
                "window:secondary",
                &mut window.used_percent,
                now,
            );
        }
        for window in &mut animated.additional_windows {
            animate_used_percent(
                &self.transitions,
                account_id,
                &format!("window:additional:{}", window.key),
                &mut window.window.used_percent,
                now,
            );
        }
        for metric in &mut animated.metrics {
            if let Some(used_percent) = &mut metric.used_percent {
                animate_used_percent(
                    &self.transitions,
                    account_id,
                    &format!("metric:{}", metric.key),
                    used_percent,
                    now,
                );
            }
        }
        if let Some(spend) = &mut animated.spend
            && let Some(used_percent) = &mut spend.used_percent
        {
            animate_used_percent(&self.transitions, account_id, "spend", used_percent, now);
        }
        if let Some(limit) = animated
            .credits
            .as_mut()
            .and_then(|credits| credits.limit.as_mut())
            && let Some(used_percent) = &mut limit.used_percent
        {
            animate_used_percent(
                &self.transitions,
                account_id,
                "credit-limit",
                used_percent,
                now,
            );
        }

        Some(animated)
    }
}

pub(super) fn collect_usage_percent_values(
    entries: &[AccountUsageEntry],
) -> HashMap<UsagePercentKey, f64> {
    let mut values = HashMap::new();

    for entry in entries {
        let Some(snapshot) = entry.snapshot.as_ref().filter(|snapshot| {
            snapshot.account_id == entry.account.id
                && providers_match(&entry.account.provider_id, &snapshot.provider_id)
                && !snapshot.observed_email.as_deref().is_some_and(|email| {
                    !email
                        .trim()
                        .eq_ignore_ascii_case(entry.account.email.trim())
                })
        }) else {
            continue;
        };

        if let Some(window) = &snapshot.primary {
            insert_usage_percent(
                &mut values,
                entry.account.id,
                "window:primary",
                window.used_percent,
            );
        }
        if let Some(window) = &snapshot.secondary {
            insert_usage_percent(
                &mut values,
                entry.account.id,
                "window:secondary",
                window.used_percent,
            );
        }
        for window in &snapshot.additional_windows {
            insert_usage_percent(
                &mut values,
                entry.account.id,
                &format!("window:additional:{}", window.key),
                window.window.used_percent,
            );
        }
        for metric in &snapshot.metrics {
            if let Some(used_percent) = metric.used_percent {
                insert_usage_percent(
                    &mut values,
                    entry.account.id,
                    &format!("metric:{}", metric.key),
                    used_percent,
                );
            }
        }
        if let Some(used_percent) = snapshot.spend.as_ref().and_then(|spend| spend.used_percent) {
            insert_usage_percent(&mut values, entry.account.id, "spend", used_percent);
        }
        if let Some(used_percent) = snapshot
            .credits
            .as_ref()
            .and_then(|credits| credits.limit.as_ref())
            .and_then(|limit| limit.used_percent)
        {
            insert_usage_percent(&mut values, entry.account.id, "credit-limit", used_percent);
        }
    }

    values
}

pub(super) fn insert_usage_percent(
    values: &mut HashMap<UsagePercentKey, f64>,
    account_id: AccountId,
    field: &str,
    used_percent: f64,
) {
    if used_percent.is_finite() {
        values.insert(
            UsagePercentKey {
                account_id,
                field: field.to_owned(),
            },
            (100.0 - used_percent).clamp(0.0, 100.0),
        );
    }
}

pub(super) fn usage_percent_is_close(left: f64, right: f64) -> bool {
    (left - right).abs() < 0.02
}

pub(super) fn animate_used_percent(
    transitions: &HashMap<UsagePercentKey, UsagePercentTransition>,
    account_id: AccountId,
    field: &str,
    used_percent: &mut f64,
    now: Instant,
) {
    if !used_percent.is_finite() {
        return;
    }

    let key = UsagePercentKey {
        account_id,
        field: field.to_owned(),
    };
    if let Some(transition) = transitions.get(&key) {
        *used_percent = 100.0 - transition.value_at(now);
    }
}
