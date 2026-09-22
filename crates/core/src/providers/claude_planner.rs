//! Deterministic source planning for Claude usage probes.
//!
//! The planner deliberately knows nothing about HTTP, credentials, or the
//! CLI implementation. It answers one question only: given the host runtime,
//! the requested source mode, and the credentials that are plausibly present,
//! in which order should the adapter attempt sources?

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeRuntime {
    /// The tray/desktop host. OAuth is preferred, then local CLI, then web.
    App,
    /// A host that is itself running as a CLI-oriented integration. Web is
    /// preferred so the host does not recursively launch another CLI process.
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeSourceMode {
    /// Let the runtime-aware planner choose the authoritative source order.
    Automatic,
    Cli,
    OAuth,
    Web,
    AdminApi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeSource {
    AdminApi,
    OAuth,
    Web,
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudePlanReason {
    ExplicitSourceSelection,
    AutomaticAdminApi,
    AppAutoPreferredOAuth,
    AppAutoFallbackCli,
    AppAutoFallbackWeb,
    CliAutoPreferredWeb,
    CliAutoFallbackCli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeSourcePlanningInput {
    pub runtime: ClaudeRuntime,
    pub selected_source: ClaudeSourceMode,
    pub has_admin_api_key: bool,
    pub has_web_session: bool,
    pub has_cli: bool,
    pub has_oauth_credentials: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeSourcePlanStep {
    pub source: ClaudeSource,
    pub reason: ClaudePlanReason,
    pub is_plausibly_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSourcePlan {
    pub input: ClaudeSourcePlanningInput,
    pub ordered_steps: Vec<ClaudeSourcePlanStep>,
}

impl ClaudeSourcePlan {
    pub fn available_steps(&self) -> impl Iterator<Item = &ClaudeSourcePlanStep> {
        self.ordered_steps
            .iter()
            .filter(|step| step.is_plausibly_available)
    }

    pub fn is_no_source_available(&self) -> bool {
        self.available_steps().next().is_none()
    }

    pub fn preferred_step(&self) -> Option<&ClaudeSourcePlanStep> {
        match self.input.selected_source {
            ClaudeSourceMode::Automatic => self.available_steps().next(),
            ClaudeSourceMode::AdminApi
            | ClaudeSourceMode::OAuth
            | ClaudeSourceMode::Web
            | ClaudeSourceMode::Cli => self.ordered_steps.first(),
        }
    }

    pub fn order_label(&self) -> String {
        self.ordered_steps
            .iter()
            .map(|step| source_label(step.source))
            .collect::<Vec<_>>()
            .join("→")
    }

    /// Returns compact diagnostics suitable for a backend log or a future
    /// troubleshooting panel. It contains no credentials.
    pub fn debug_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("planner_order={}", self.order_label())];
        lines.push(format!(
            "planner_selected={}",
            self.preferred_step()
                .map(|step| source_label(step.source))
                .unwrap_or("none")
        ));
        lines.push(format!(
            "planner_no_source={}",
            self.is_no_source_available()
        ));
        for step in &self.ordered_steps {
            lines.push(format!(
                "planner_step.{}={} reason={}",
                source_label(step.source),
                if step.is_plausibly_available {
                    "available"
                } else {
                    "unavailable"
                },
                reason_label(step.reason)
            ));
        }
        lines
    }
}

pub fn plan(input: ClaudeSourcePlanningInput) -> ClaudeSourcePlan {
    let ordered_steps = match input.selected_source {
        ClaudeSourceMode::AdminApi => {
            vec![step(
                ClaudeSource::AdminApi,
                ClaudePlanReason::ExplicitSourceSelection,
                input,
            )]
        }
        ClaudeSourceMode::OAuth => vec![step(
            ClaudeSource::OAuth,
            ClaudePlanReason::ExplicitSourceSelection,
            input,
        )],
        ClaudeSourceMode::Web => vec![step(
            ClaudeSource::Web,
            ClaudePlanReason::ExplicitSourceSelection,
            input,
        )],
        ClaudeSourceMode::Cli => vec![step(
            ClaudeSource::Cli,
            ClaudePlanReason::ExplicitSourceSelection,
            input,
        )],
        ClaudeSourceMode::Automatic => match input.runtime {
            ClaudeRuntime::App => {
                if input.has_admin_api_key {
                    vec![step(
                        ClaudeSource::AdminApi,
                        ClaudePlanReason::AutomaticAdminApi,
                        input,
                    )]
                } else {
                    vec![
                        step(
                            ClaudeSource::OAuth,
                            ClaudePlanReason::AppAutoPreferredOAuth,
                            input,
                        ),
                        step(
                            ClaudeSource::Cli,
                            ClaudePlanReason::AppAutoFallbackCli,
                            input,
                        ),
                        step(
                            ClaudeSource::Web,
                            ClaudePlanReason::AppAutoFallbackWeb,
                            input,
                        ),
                    ]
                }
            }
            ClaudeRuntime::Cli => vec![
                step(
                    ClaudeSource::Web,
                    ClaudePlanReason::CliAutoPreferredWeb,
                    input,
                ),
                step(
                    ClaudeSource::Cli,
                    ClaudePlanReason::CliAutoFallbackCli,
                    input,
                ),
            ],
        },
    };

    ClaudeSourcePlan {
        input,
        ordered_steps,
    }
}

fn step(
    source: ClaudeSource,
    reason: ClaudePlanReason,
    input: ClaudeSourcePlanningInput,
) -> ClaudeSourcePlanStep {
    ClaudeSourcePlanStep {
        source,
        reason,
        is_plausibly_available: match source {
            ClaudeSource::AdminApi => input.has_admin_api_key,
            ClaudeSource::OAuth => input.has_oauth_credentials,
            ClaudeSource::Web => input.has_web_session,
            ClaudeSource::Cli => input.has_cli,
        },
    }
}

fn source_label(source: ClaudeSource) -> &'static str {
    match source {
        ClaudeSource::AdminApi => "api",
        ClaudeSource::OAuth => "oauth",
        ClaudeSource::Web => "web",
        ClaudeSource::Cli => "cli",
    }
}

fn reason_label(reason: ClaudePlanReason) -> &'static str {
    match reason {
        ClaudePlanReason::ExplicitSourceSelection => "explicit-source-selection",
        ClaudePlanReason::AutomaticAdminApi => "automatic-admin-api",
        ClaudePlanReason::AppAutoPreferredOAuth => "app-auto-preferred-oauth",
        ClaudePlanReason::AppAutoFallbackCli => "app-auto-fallback-cli",
        ClaudePlanReason::AppAutoFallbackWeb => "app-auto-fallback-web",
        ClaudePlanReason::CliAutoPreferredWeb => "cli-auto-preferred-web",
        ClaudePlanReason::CliAutoFallbackCli => "cli-auto-fallback-cli",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        runtime: ClaudeRuntime,
        selected_source: ClaudeSourceMode,
        has_admin_api_key: bool,
        has_web_session: bool,
        has_cli: bool,
        has_oauth_credentials: bool,
    ) -> ClaudeSourcePlanningInput {
        ClaudeSourcePlanningInput {
            runtime,
            selected_source,
            has_admin_api_key,
            has_web_session,
            has_cli,
            has_oauth_credentials,
        }
    }

    #[test]
    fn app_auto_matches_authority_order_and_skips_unavailable_sources() {
        let admin_plan = plan(input(
            ClaudeRuntime::App,
            ClaudeSourceMode::Automatic,
            true,
            true,
            true,
            true,
        ));
        assert_eq!(admin_plan.order_label(), "api");
        assert_eq!(admin_plan.available_steps().count(), 1);
        assert_eq!(
            admin_plan.ordered_steps[0].reason,
            ClaudePlanReason::AutomaticAdminApi
        );

        let no_oauth = plan(input(
            ClaudeRuntime::App,
            ClaudeSourceMode::Automatic,
            false,
            true,
            true,
            false,
        ));
        assert_eq!(no_oauth.order_label(), "oauth→cli→web");
        assert_eq!(
            no_oauth
                .available_steps()
                .map(|step| step.source)
                .collect::<Vec<_>>(),
            vec![ClaudeSource::Cli, ClaudeSource::Web]
        );
    }

    #[test]
    fn cli_runtime_prefers_web_and_does_not_recurse_into_oauth_or_admin() {
        let plan = plan(input(
            ClaudeRuntime::Cli,
            ClaudeSourceMode::Automatic,
            true,
            true,
            true,
            true,
        ));
        assert_eq!(plan.order_label(), "web→cli");
        assert_eq!(plan.available_steps().count(), 2);
    }

    #[test]
    fn explicit_oauth_selection_never_falls_back_to_another_account_source() {
        let app = plan(input(
            ClaudeRuntime::App,
            ClaudeSourceMode::OAuth,
            false,
            false,
            true,
            true,
        ));
        assert_eq!(app.order_label(), "oauth");
        assert_eq!(
            app.preferred_step().map(|step| step.source),
            Some(ClaudeSource::OAuth)
        );
        assert_eq!(app.available_steps().count(), 1);

        let cli = plan(input(
            ClaudeRuntime::Cli,
            ClaudeSourceMode::OAuth,
            false,
            false,
            true,
            true,
        ));
        assert_eq!(cli.order_label(), "oauth");
        assert_eq!(cli.available_steps().count(), 1);
    }

    #[test]
    fn debug_lines_do_not_include_credentials() {
        let plan = plan(input(
            ClaudeRuntime::App,
            ClaudeSourceMode::Automatic,
            false,
            true,
            false,
            false,
        ));
        let debug = plan.debug_lines().join("\n");
        assert!(debug.contains("planner_order=oauth→cli→web"));
        assert!(debug.contains("planner_step.web=available"));
        assert!(!debug.contains("token"));
    }
}
