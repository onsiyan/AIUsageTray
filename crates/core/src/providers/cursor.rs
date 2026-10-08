//! Cursor plan usage through cursor.com's dashboard endpoints.
//!
//! Cursor has no public usage API, so this reads the endpoints
//! its own dashboard uses, authenticated with the `WorkosCursorSessionToken`
//! session cookie. That cookie is `<user id>::<access token>`, where the
//! access token is the JWT the Cursor editor keeps in its global state
//! database, so an account can come from the signed-in Cursor app or from a
//! cookie pasted from cursor.com.
//!
//! - `GET /api/usage-summary` (required): billing cycle, the included plan's
//!   total, Auto + Composer and API percentages, and on-demand spend.
//! - `GET /api/auth/me`: the account's email and user id.
//! - `GET /api/usage?user=ID`: request counts on legacy request-based plans.
//! - `POST /api/dashboard/get-sand-usage-status`: the weekly Grok Bot
//!   allowance. Best effort: its failure never hides the monthly bars.
//! - `POST /api/dashboard/teams` and `get-team-spend`: on team plans, the
//!   member's own spend against their per-user budget. Best effort too; the
//!   other members' spending is read only to find this member and is never
//!   kept.
//!
//! The access token is never refreshed here. While the Cursor app stays
//! signed in to the same account, each refresh prefers its newer token.

use crate::{
    accounts::{AccountRecord, CURSOR, VerifiedIdentity},
    auth::{AccountAuthMaterialProvider, AuthError},
    providers::shared::{invalid_payload, json_bool, json_string, map_http_error, missing_auth},
    transport::{TransportError, UsageHttpRequest, UsageHttpResponse, UsageHttpTransport},
    usage::{
        AdditionalRateLimitWindow, RateLimitWindow, UsageAdapter, UsageAdapterError,
        UsageAdapterErrorCode, UsageMetric, UsagePrimaryWindowKind, UsageProbeResult,
        UsageSnapshot, UsageWindowKind,
    },
};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use url::Url;

const BASE_URL: &str = "https://cursor.com";
const SESSION_COOKIE: &str = "WorkosCursorSessionToken";
const APP_TOKEN_KEY: &str = "cursorAuth/accessToken";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);
/// Optional lookups may not hold up the required usage summary for long.
const OPTIONAL_DEADLINE: Duration = Duration::from_secs(5);
/// Bounds the whole team budget lookup, pages included.
const TEAM_BUDGET_DEADLINE: Duration = Duration::from_secs(10);
const TEAM_SPEND_PAGE_SIZE: usize = 50;
const TEAM_SPEND_MAX_PAGES: i64 = 20;
/// The member's budget on a team plan; hosts may choose not to show it.
pub const TEAM_BUDGET_KEY: &str = "team.member_budget";
/// A token this close to expiry is treated as expired.
const EXPIRY_MARGIN_SECONDS: i64 = 60;
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) AIUsageTray";

pub const TOTAL_WINDOW_NAME: &str = "Total";
pub const AUTO_WINDOW_NAME: &str = "Auto + Composer";
pub const API_WINDOW_NAME: &str = "API";
pub const GROK_BOT_WINDOW_NAME: &str = "Grok Bot";
pub const REQUESTS_WINDOW_NAME: &str = "Requests";

/// A Cursor session: the access token and what its JWT says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSession {
    pub access_token: String,
    /// The WorkOS user id, the part of the JWT subject after `|`.
    pub user_id: String,
    pub email: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl CursorSession {
    /// Reads the user id and expiry from an access token.
    pub fn from_access_token(token: &str) -> Result<Self, String> {
        let token = token.trim();
        let payload = jwt_payload(token).ok_or("the Cursor token is not a valid session token")?;
        let user_id = json_string(&payload, &["sub"])
            .and_then(|subject| {
                subject
                    .rsplit('|')
                    .next()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
            })
            .ok_or("the Cursor token has no user id")?;
        // The id goes into a cookie; anything unexpected there is refused.
        if !user_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
        {
            return Err("the Cursor token has an invalid user id".to_owned());
        }
        let expires_at = payload
            .get("exp")
            .and_then(Value::as_f64)
            .and_then(|seconds| DateTime::from_timestamp(seconds as i64, 0));
        Ok(Self {
            access_token: token.to_owned(),
            user_id,
            email: json_string(&payload, &["email"]),
            expires_at,
        })
    }

    /// Whether the token is still good for more than a minute.
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        self.expires_at
            .is_some_and(|expires_at| (expires_at - now).num_seconds() > EXPIRY_MARGIN_SECONDS)
    }

    pub fn cookie_header(&self) -> String {
        format!(
            "{SESSION_COOKIE}={}%3A%3A{}",
            self.user_id, self.access_token
        )
    }
}

fn jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let (_, payload) = (parts.next()?, parts.next()?);
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
}

/// The access token in whatever the user pasted: a whole `Cookie:` header,
/// the `WorkosCursorSessionToken` cookie or its value, or the bare token.
pub fn access_token_from_input(input: &str) -> Option<String> {
    let input = input.trim();
    let input = input
        .strip_prefix("Cookie:")
        .or_else(|| input.strip_prefix("cookie:"))
        .unwrap_or(input)
        .trim();
    let value = input
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == SESSION_COOKIE)
        .map_or(input, |(_, value)| value.trim());
    let value = value.replace("%3A", ":").replace("%3a", ":");
    let token = value.rsplit("::").next()?.trim().trim_matches('"');
    (token.split('.').count() == 3 && jwt_payload(token).is_some()).then(|| token.to_owned())
}

/// Where the Cursor editor keeps its global state, including its session.
pub fn default_app_database() -> Option<PathBuf> {
    let relative = Path::new("Cursor")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb");
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join(relative))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(relative)
        })
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|config| config.join(relative))
    }
}

/// The access token the Cursor app is signed in with, if any. The database
/// is only read, never written.
pub fn read_app_token(database: &Path) -> Result<Option<String>, String> {
    if !database.is_file() {
        return Ok(None);
    }
    match read_app_token_with(database, false) {
        Err(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::CannotOpen && !has_wal_sidecars(database) =>
        {
            // An idle WAL-mode database without its sidecar files cannot be
            // opened read-only; immutable mode reads it without creating
            // files in Cursor's folder. Never with a live WAL, whose newer
            // state immutable mode would skip.
            read_app_token_with(database, true)
        }
        result => result,
    }
    .map_err(|error| format!("could not read the Cursor app session: {error}"))
}

fn has_wal_sidecars(database: &Path) -> bool {
    let sidecar = |suffix: &str| {
        let mut path = database.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path).exists()
    };
    sidecar("-wal") || sidecar("-shm")
}

fn read_app_token_with(
    database: &Path,
    immutable: bool,
) -> Result<Option<String>, rusqlite::Error> {
    use rusqlite::{OpenFlags, OptionalExtension, types::ValueRef};
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = if immutable {
        let uri = Url::from_file_path(database)
            .map_err(|()| rusqlite::Error::InvalidPath(database.to_owned()))?;
        rusqlite::Connection::open_with_flags(
            format!("{uri}?immutable=1"),
            flags | OpenFlags::SQLITE_OPEN_URI,
        )?
    } else {
        rusqlite::Connection::open_with_flags(database, flags)?
    };
    connection.busy_timeout(Duration::from_millis(250))?;
    connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1",
            [APP_TOKEN_KEY],
            |row| {
                Ok(match row.get_ref(0)? {
                    ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                    ValueRef::Blob(bytes) => decode_blob(bytes),
                    _ => None,
                })
            },
        )
        .optional()
        .map(|value| {
            value
                .flatten()
                .map(|token| token.trim().trim_matches('"').to_owned())
                .filter(|token| !token.is_empty())
        })
}

/// Token blobs are UTF-8, or UTF-16LE: ASCII in UTF-16LE is also valid
/// UTF-8 with NULs between the letters, so it is recognised first.
fn decode_blob(bytes: &[u8]) -> Option<String> {
    let ascii_utf16 = bytes.len().is_multiple_of(2)
        && !bytes.is_empty()
        && bytes
            .chunks_exact(2)
            .all(|pair| (1..128).contains(&pair[0]) && pair[1] == 0);
    if ascii_utf16 {
        return Some(bytes.iter().step_by(2).map(|&byte| byte as char).collect());
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_owned());
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

/// The Cursor app's session, when it has a usable one.
pub fn local_app_session() -> Option<CursorSession> {
    let token = read_app_token(&default_app_database()?).ok()??;
    CursorSession::from_access_token(&token)
        .ok()
        .filter(|session| session.is_usable(Utc::now()))
}

pub struct CursorUsageAdapter {
    transport: Arc<dyn UsageHttpTransport>,
    auth: Arc<dyn AccountAuthMaterialProvider>,
    base_url: Url,
    app_database: Option<PathBuf>,
    deadline: Duration,
}

impl CursorUsageAdapter {
    pub fn new(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            transport,
            auth,
            base_url: parse_url(BASE_URL)?,
            app_database: default_app_database(),
            deadline: DEFAULT_DEADLINE,
        })
    }

    /// Reads the Cursor app's session from `database` instead of the
    /// default location, or never with `None`.
    pub fn with_app_database(mut self, database: Option<PathBuf>) -> Self {
        self.app_database = database;
        self
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The Cursor app's token when it is signed in to this account and
    /// fresh, otherwise the token saved with the account.
    async fn session(&self, account: &AccountRecord) -> Result<Option<CursorSession>, String> {
        let now = Utc::now();
        if let Some(database) = self.app_database.clone()
            && let Ok(Ok(Some(token))) =
                tokio::task::spawn_blocking(move || read_app_token(&database)).await
            && let Ok(session) = CursorSession::from_access_token(&token)
            && session.is_usable(now)
            && account.provider_account_id.as_deref() == Some(session.user_id.as_str())
        {
            return Ok(Some(session));
        }
        let token = match self.auth.get(account).await {
            Ok(Some(material)) => material
                .bearer_token
                .map(|token| token.trim().to_owned())
                .filter(|token| !token.is_empty()),
            Ok(None) | Err(AuthError::ReauthenticationRequired(_)) => None,
            Err(error) => return Err(error.to_string()),
        };
        token
            .map(|token| CursorSession::from_access_token(&token))
            .transpose()
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        query: Option<(&str, &str)>,
        session: &CursorSession,
    ) -> Result<UsageHttpRequest, TransportError> {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|error| TransportError::InvalidUrl(error.to_string()))?;
        if let Some((name, value)) = query {
            url.query_pairs_mut().append_pair(name, value);
        }
        let mut headers = BTreeMap::from([
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Cookie".to_owned(), session.cookie_header()),
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
        ]);
        let body = (method == Method::POST).then(|| {
            // The dashboard's POST endpoints check the origin against CSRF.
            headers.insert("Content-Type".to_owned(), "application/json".to_owned());
            headers.insert(
                "Origin".to_owned(),
                self.base_url.origin().ascii_serialization(),
            );
            "{}".to_owned()
        });
        Ok(UsageHttpRequest {
            method,
            url,
            headers,
            body,
        })
    }

    async fn send(
        &self,
        request: UsageHttpRequest,
        deadline: Duration,
    ) -> Result<UsageHttpResponse, TransportError> {
        tokio::time::timeout(deadline, self.transport.send(request))
            .await
            .map_err(|_| TransportError::Timeout("cursor usage".to_owned()))?
    }

    /// A dashboard POST endpoint with a JSON body.
    fn dashboard_request(
        &self,
        endpoint: &str,
        body: Value,
        session: &CursorSession,
    ) -> Result<UsageHttpRequest, TransportError> {
        let mut request = self.request(
            Method::POST,
            &format!("/api/dashboard/{endpoint}"),
            None,
            session,
        )?;
        request.body = Some(body.to_string());
        if let Ok(referer) = self.base_url.join("/dashboard") {
            request
                .headers
                .insert("Referer".to_owned(), referer.to_string());
        }
        Ok(request)
    }

    /// The member's spend and budget in dollars, found among the team's
    /// members by email. Any doubt (several teams, a page that does not add
    /// up, the email twice) gives no budget rather than someone else's.
    async fn team_budget(&self, session: &CursorSession, email: &str) -> Option<(f64, f64)> {
        let email = email.trim();
        if email.is_empty() {
            return None;
        }
        let teams = self
            .optional_json(self.dashboard_request("teams", serde_json::json!({}), session))
            .await?;
        let team_id = sole_team_id(&teams)?;
        let mut expected_pages = None;
        let mut candidate = None;
        for page in 1..=TEAM_SPEND_MAX_PAGES {
            let body = serde_json::json!({
                "teamId": team_id,
                "page": page,
                "pageSize": TEAM_SPEND_PAGE_SIZE,
                "sortBy": "name",
                "sortDirection": "asc",
            });
            let spend = self
                .optional_json(self.dashboard_request("get-team-spend", body, session))
                .await?;
            let members = spend.get("teamMemberSpend")?.as_array()?;
            let total_pages = spend.get("totalPages")?.as_i64()?;
            let complete = (1..=TEAM_SPEND_MAX_PAGES).contains(&total_pages)
                && expected_pages.is_none_or(|expected| expected == total_pages)
                && !members.is_empty()
                && members.len() <= TEAM_SPEND_PAGE_SIZE
                && (page == total_pages || members.len() == TEAM_SPEND_PAGE_SIZE);
            if !complete {
                return None;
            }
            expected_pages = Some(total_pages);
            for member in members.iter().filter(|member| {
                json_string(member, &["email"])
                    .is_some_and(|member_email| member_email.trim().eq_ignore_ascii_case(email))
            }) {
                if candidate.is_some() {
                    return None;
                }
                candidate = Some(member_budget(member)?);
            }
            // A later page could still name the email again.
            if page == total_pages {
                return candidate;
            }
        }
        None
    }

    /// An optional endpoint's JSON, or `None` when it fails in any way.
    async fn optional_json(
        &self,
        request: Result<UsageHttpRequest, TransportError>,
    ) -> Option<Value> {
        let response = self.send(request.ok()?, OPTIONAL_DEADLINE).await.ok()?;
        if !response.is_success() {
            return None;
        }
        serde_json::from_str::<Value>(&response.body)
            .ok()
            .filter(Value::is_object)
    }
}

#[async_trait]
impl UsageAdapter for CursorUsageAdapter {
    fn adapter_id(&self) -> &str {
        CURSOR
    }

    async fn probe(&self, account: &AccountRecord) -> Result<UsageProbeResult, TransportError> {
        let session = match self.session(account).await {
            Ok(Some(session)) => session,
            Ok(None) => return Ok(missing_auth("Cursor")),
            Err(reason) => return Ok(invalid_payload("Cursor", reason)),
        };
        if !session.is_usable(Utc::now()) {
            return Ok(UsageProbeResult::failure(UsageAdapterError {
                code: UsageAdapterErrorCode::Unauthorized,
                message: "The Cursor session expired; sign in to Cursor again".to_owned(),
                http_status_code: None,
                retry_after_seconds: None,
            }));
        }

        let summary_request = self.request(Method::GET, "/api/usage-summary", None, &session)?;
        let (summary, user, sand) = tokio::join!(
            self.send(summary_request, self.deadline),
            self.optional_json(self.request(Method::GET, "/api/auth/me", None, &session)),
            self.optional_json(self.request(
                Method::POST,
                "/api/dashboard/get-sand-usage-status",
                None,
                &session,
            )),
        );
        let summary = summary?;
        if !summary.is_success() {
            return Ok(map_http_error(&summary, "Cursor"));
        }
        let summary = match parse_summary(&summary.body) {
            Ok(summary) => summary,
            Err(reason) => return Ok(invalid_payload("Cursor", reason)),
        };

        let user_subject = user
            .as_ref()
            .and_then(|user| json_string(user, &["sub"]))
            .unwrap_or_else(|| session.user_id.clone());
        // Only request-based (legacy) plans answer with request counts.
        let requests = self
            .optional_json(self.request(
                Method::GET,
                "/api/usage",
                Some(("user", &user_subject)),
                &session,
            ))
            .await
            .and_then(|usage| parse_request_usage(&usage));

        let email = user
            .as_ref()
            .and_then(|user| json_string(user, &["email"]))
            .or_else(|| session.email.clone());
        let team_budget = match (&email, summary.is_team_plan) {
            (Some(email), true) => {
                tokio::time::timeout(TEAM_BUDGET_DEADLINE, self.team_budget(&session, email))
                    .await
                    .ok()
                    .flatten()
            }
            _ => None,
        };
        let reset_at = summary.billing_cycle_end;
        let usage = CursorUsage {
            summary,
            requests,
            grok_bot: sand.as_ref().map(GrokBotUsage::parse),
        };
        let mut snapshot = usage.snapshot(account, Utc::now());
        snapshot.observed_email = email.clone();
        if let Some((used, limit)) = team_budget {
            snapshot.metrics.push(UsageMetric {
                key: TEAM_BUDGET_KEY.to_owned(),
                name: "Team budget".to_owned(),
                used_percent: None,
                used_amount: Some(used),
                limit_amount: Some(limit),
                remaining_amount: None,
                unit: Some("USD".to_owned()),
                reset_at_utc: reset_at,
                reset_label: None,
                metadata: HashMap::new(),
            });
        }
        let identity = VerifiedIdentity {
            email,
            provider_account_id: Some(session.user_id),
            plan_type: snapshot.plan_type.clone(),
        };
        Ok(UsageProbeResult::success(snapshot, Some(identity)))
    }
}

/// The account a session belongs to, from `/api/auth/me`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorIdentity {
    pub user_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// Confirms a session with cursor.com and names its account.
pub async fn fetch_identity(
    transport: &dyn UsageHttpTransport,
    session: &CursorSession,
) -> Result<CursorIdentity, String> {
    let adapter_url = parse_url(BASE_URL).map_err(|error| error.to_string())?;
    let url = adapter_url
        .join("/api/auth/me")
        .map_err(|error| error.to_string())?;
    let response = transport
        .send(UsageHttpRequest {
            method: Method::GET,
            url,
            headers: BTreeMap::from([
                ("Accept".to_owned(), "application/json".to_owned()),
                ("Cookie".to_owned(), session.cookie_header()),
                ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ]),
            body: None,
        })
        .await
        .map_err(|error| format!("Could not reach Cursor: {error}"))?;
    if matches!(response.status_code, 401 | 403) {
        return Err("Cursor did not accept this session; sign in to cursor.com again".to_owned());
    }
    if !response.is_success() {
        return Err(format!(
            "Cursor account lookup returned HTTP {}",
            response.status_code
        ));
    }
    let user: Value = serde_json::from_str(&response.body)
        .map_err(|_| "Cursor returned an unreadable account".to_owned())?;
    Ok(CursorIdentity {
        user_id: session.user_id.clone(),
        email: json_string(&user, &["email"]).or_else(|| session.email.clone()),
        name: json_string(&user, &["name"]),
    })
}

fn parse_url(url: &str) -> Result<Url, TransportError> {
    Url::parse(url).map_err(|error| TransportError::InvalidUrl(error.to_string()))
}

/// The parts of `/api/usage-summary` the card shows. Amounts are in cents.
#[derive(Debug, Clone, PartialEq, Default)]
struct UsageSummary {
    billing_cycle_start: Option<DateTime<Utc>>,
    billing_cycle_end: Option<DateTime<Utc>>,
    membership_type: Option<String>,
    is_team_plan: bool,
    plan_used: Option<f64>,
    plan_limit: Option<f64>,
    total_percent_used: Option<f64>,
    auto_percent_used: Option<f64>,
    api_percent_used: Option<f64>,
    overall_used: Option<f64>,
    overall_limit: Option<f64>,
    pooled_used: Option<f64>,
    pooled_limit: Option<f64>,
    on_demand_used: Option<f64>,
    on_demand_limit: Option<f64>,
    team_on_demand_used: Option<f64>,
    team_on_demand_limit: Option<f64>,
}

fn parse_summary(body: &str) -> Result<UsageSummary, String> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| format!("usage summary JSON could not be parsed: {error}"))?;
    if !root.is_object() {
        return Err("usage summary is not an object".to_owned());
    }
    let at = |path: &[&str]| path.iter().try_fold(&root, |value, key| value.get(key));
    let amount = |path: &[&str]| number(at(path));
    let membership_type = json_string(&root, &["membershipType"]);
    let limit_type = json_string(&root, &["limitType"]);
    let is_team_plan = [&membership_type, &limit_type]
        .into_iter()
        .flatten()
        .any(|kind| {
            matches!(
                kind.to_ascii_lowercase().as_str(),
                "team" | "teams" | "enterprise" | "business"
            )
        });
    Ok(UsageSummary {
        billing_cycle_start: json_string(&root, &["billingCycleStart"])
            .as_deref()
            .and_then(parse_timestamp),
        billing_cycle_end: json_string(&root, &["billingCycleEnd"])
            .as_deref()
            .and_then(parse_timestamp),
        membership_type,
        is_team_plan,
        plan_used: amount(&["individualUsage", "plan", "used"]),
        plan_limit: amount(&["individualUsage", "plan", "limit"]),
        total_percent_used: amount(&["individualUsage", "plan", "totalPercentUsed"]),
        auto_percent_used: amount(&["individualUsage", "plan", "autoPercentUsed"]),
        api_percent_used: amount(&["individualUsage", "plan", "apiPercentUsed"]),
        overall_used: amount(&["individualUsage", "overall", "used"]),
        overall_limit: amount(&["individualUsage", "overall", "limit"]),
        pooled_used: amount(&["teamUsage", "pooled", "used"]),
        pooled_limit: amount(&["teamUsage", "pooled", "limit"]),
        on_demand_used: amount(&["individualUsage", "onDemand", "used"]),
        on_demand_limit: amount(&["individualUsage", "onDemand", "limit"]),
        team_on_demand_used: amount(&["teamUsage", "onDemand", "used"]),
        team_on_demand_limit: amount(&["teamUsage", "onDemand", "limit"]),
    })
}

/// Request counts of a legacy request-based plan, from `/api/usage`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RequestUsage {
    used: f64,
    limit: f64,
}

fn parse_request_usage(usage: &Value) -> Option<RequestUsage> {
    let model = usage.get("gpt-4")?;
    let limit = number(model.get("maxRequestUsage")).filter(|limit| *limit > 0.0)?;
    let used =
        number(model.get("numRequestsTotal")).or_else(|| number(model.get("numRequests")))?;
    Some(RequestUsage { used, limit })
}

/// The weekly Grok Bot ("Sand") allowance.
#[derive(Debug, Clone, PartialEq, Default)]
struct GrokBotUsage {
    usage_percent: Option<f64>,
    current_period_start: Option<DateTime<Utc>>,
    next_reset: Option<DateTime<Utc>>,
    has_included_limit: Option<bool>,
    trial_expires_at: Option<DateTime<Utc>>,
}

impl GrokBotUsage {
    fn parse(value: &Value) -> Self {
        let date = |name: &str| {
            json_string(value, &[name])
                .as_deref()
                .and_then(parse_timestamp)
        };
        // `includedLimitZero` is current; `hasNonZeroIncludedLimit` older.
        let has_included_limit = json_bool(value, &["includedLimitZero"])
            .map(|zero| !zero)
            .or_else(|| json_bool(value, &["hasNonZeroIncludedLimit"]));
        Self {
            usage_percent: number(value.get("usagePercent")),
            current_period_start: date("currentPeriodStart"),
            next_reset: date("nextResetTimestampUtc"),
            has_included_limit,
            trial_expires_at: date("sandTrialExpiresAt"),
        }
    }

    /// A bar for a paid allowance or an unexpired trial. A trial's expiry
    /// does not replenish anything, so it has no reset time.
    fn window(&self, now: DateTime<Utc>) -> Option<RateLimitWindow> {
        let has_limit = self.has_included_limit == Some(true);
        let has_trial = !has_limit && self.trial_expires_at.is_some_and(|expires| expires > now);
        if !has_limit && !has_trial {
            return None;
        }
        let reset_at = (!has_trial).then_some(self.next_reset).flatten();
        Some(RateLimitWindow {
            kind: UsageWindowKind::Additional,
            name: GROK_BOT_WINDOW_NAME.to_owned(),
            used_percent: self.usage_percent?.clamp(0.0, 100.0),
            reset_at_utc: reset_at,
            limit_window_seconds: window_seconds(self.current_period_start, reset_at),
        })
    }
}

struct CursorUsage {
    summary: UsageSummary,
    requests: Option<RequestUsage>,
    grok_bot: Option<GrokBotUsage>,
}

impl CursorUsage {
    /// The included plan's used percentage, from the most precise field
    /// available. Cursor's percent fields are already in
    /// percent, even below 1 (0.36 means 0.36%).
    fn total_percent(&self) -> f64 {
        let summary = &self.summary;
        let ratio = |used: Option<f64>, limit: Option<f64>| match (used, limit) {
            (Some(used), Some(limit)) if limit > 0.0 => Some(used / limit * 100.0),
            _ => None,
        };
        summary
            .total_percent_used
            .or_else(
                || match (summary.auto_percent_used, summary.api_percent_used) {
                    (Some(auto), Some(api)) => Some((auto + api) / 2.0),
                    (auto, api) => api.or(auto),
                },
            )
            .or_else(|| ratio(Some(summary.plan_used.unwrap_or(0.0)), summary.plan_limit))
            .or_else(|| ratio(summary.overall_used, summary.overall_limit))
            .or_else(|| ratio(summary.pooled_used, summary.pooled_limit))
            .unwrap_or(0.0)
            .clamp(0.0, 100.0)
    }

    fn snapshot(&self, account: &AccountRecord, now: DateTime<Utc>) -> UsageSnapshot {
        let summary = &self.summary;
        let reset_at = summary.billing_cycle_end;
        let cycle_seconds = window_seconds(summary.billing_cycle_start, reset_at);
        let window = |kind, name: &str, used_percent: f64| RateLimitWindow {
            kind,
            name: name.to_owned(),
            used_percent: used_percent.clamp(0.0, 100.0),
            reset_at_utc: reset_at,
            limit_window_seconds: cycle_seconds,
        };

        let mut additional_windows = Vec::new();
        let (primary, secondary) = match self.requests {
            // A request quota stands alone: the token-based Auto and API
            // split means nothing next to it.
            Some(requests) => (
                window(
                    UsageWindowKind::Primary,
                    REQUESTS_WINDOW_NAME,
                    requests.used / requests.limit * 100.0,
                ),
                None,
            ),
            None => {
                if let Some(api) = summary.api_percent_used {
                    additional_windows.push(AdditionalRateLimitWindow {
                        key: "api".to_owned(),
                        name: API_WINDOW_NAME.to_owned(),
                        window: window(UsageWindowKind::Additional, API_WINDOW_NAME, api),
                    });
                }
                if let Some(grok_bot) = self.grok_bot.as_ref().and_then(|usage| usage.window(now)) {
                    additional_windows.push(AdditionalRateLimitWindow {
                        key: "grok_bot".to_owned(),
                        name: GROK_BOT_WINDOW_NAME.to_owned(),
                        window: grok_bot,
                    });
                }
                (
                    window(
                        UsageWindowKind::Primary,
                        TOTAL_WINDOW_NAME,
                        self.total_percent(),
                    ),
                    summary
                        .auto_percent_used
                        .map(|auto| window(UsageWindowKind::Secondary, AUTO_WINDOW_NAME, auto)),
                )
            }
        };

        UsageSnapshot {
            account_id: account.id,
            observed_at_utc: now,
            response_account_id: None,
            plan_type: summary.membership_type.as_deref().map(plan_name),
            primary_window_kind: Some(UsagePrimaryWindowKind::Other),
            primary: Some(primary),
            primary_window_is_synthetic: false,
            secondary,
            additional_windows,
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email: None,
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics: self.on_demand_metrics(reset_at),
            source_diagnostics: Vec::new(),
            provider_id: CURSOR.to_owned(),
            source: Some("web".to_owned()),
            data_confidence: "authoritative".to_owned(),
        }
    }

    /// Spend beyond the included plan. A personal cap wins; a team member
    /// without one sees the shared team budget, and their own share of it.
    fn on_demand_metrics(&self, reset_at: Option<DateTime<Utc>>) -> Vec<UsageMetric> {
        let summary = &self.summary;
        let dollars = |cents: Option<f64>| cents.map(|cents| cents / 100.0);
        let personal_used = dollars(summary.on_demand_used).unwrap_or(0.0);
        let personal_limit = dollars(summary.on_demand_limit).filter(|limit| *limit > 0.0);
        let team_limit = dollars(summary.team_on_demand_limit).filter(|limit| *limit > 0.0);
        let metric = |key: &str, name: &str, used: f64, limit: Option<f64>| UsageMetric {
            key: key.to_owned(),
            name: name.to_owned(),
            used_percent: None,
            used_amount: Some(used),
            limit_amount: limit,
            remaining_amount: None,
            unit: Some("USD".to_owned()),
            reset_at_utc: reset_at,
            reset_label: None,
            metadata: HashMap::new(),
        };
        match (personal_limit, team_limit) {
            (None, Some(team_limit)) => {
                let team_used = dollars(summary.team_on_demand_used).unwrap_or(0.0);
                let mut metrics = vec![metric(
                    "on_demand.team",
                    "Team on-demand",
                    team_used,
                    Some(team_limit),
                )];
                if personal_used > 0.0 {
                    metrics.push(metric(
                        "on_demand.yours",
                        "Your on-demand",
                        personal_used,
                        None,
                    ));
                }
                metrics
            }
            (limit, _) if personal_used > 0.0 || limit.is_some() => {
                vec![metric("on_demand", "On-demand", personal_used, limit)]
            }
            _ => Vec::new(),
        }
    }
}

/// Cursor's plan names as its dashboard shows them.
fn plan_name(membership: &str) -> String {
    match membership.to_ascii_lowercase().as_str() {
        "enterprise" => "Enterprise",
        "express" => "Start",
        "free" => "Free",
        "free_trial" => "Pro Trial",
        "hobby" => "Hobby",
        "pro" | "pro_student" => "Pro",
        "pro_plus" => "Pro+",
        "team" => "Team",
        "ultra" => "Ultra",
        _ => membership,
    }
    .to_owned()
}

fn window_seconds(start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> i64 {
    match (start, end) {
        (Some(start), Some(end)) if end > start => (end - start).num_seconds(),
        _ => 0,
    }
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.with_timezone(&Utc));
    }
    // Epoch milliseconds, as some dashboard fields are sent.
    value
        .parse::<i64>()
        .ok()
        .and_then(DateTime::from_timestamp_millis)
}

/// The one team the session belongs to. Without the dashboard's team
/// selection cookie, several teams are ambiguous.
fn sole_team_id(teams: &Value) -> Option<i64> {
    let teams = teams.get("teams")?.as_array()?;
    let ids = teams
        .iter()
        .map(|team| team.get("id")?.as_i64().filter(|id| *id > 0))
        .collect::<Option<Vec<_>>>()?;
    match ids.as_slice() {
        [id] => Some(*id),
        _ => None,
    }
}

/// A member's spend and limit in dollars. An explicit zero effective limit
/// means no per-user budget; it never falls back to the monthly limit.
fn member_budget(member: &Value) -> Option<(f64, f64)> {
    let used = number(member.get("overallSpendCents")).filter(|used| *used >= 0.0)? / 100.0;
    let limit = match member.get("effectivePerUserLimitDollars") {
        Some(value) if !value.is_null() => number(Some(value)),
        _ => number(member.get("monthlyLimitDollars")),
    }
    .filter(|limit| *limit > 0.0)?;
    Some((used, limit))
}

/// A number given as a JSON number or a numeric string.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|value: &f64| value.is_finite())
}

#[cfg(test)]
mod tests;
