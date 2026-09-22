use crate::auth::{
    AuthError, OAuthCallbackListener, OAuthCallbackListenerFactory, OAuthCallbackResult,
};
use async_trait::async_trait;
use chrono::Duration;
use std::net::Ipv4Addr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration as TokioDuration, timeout};
use url::Url;

pub struct LoopbackOAuthCallbackListener {
    configured_uri: Url,
    actual_uri: Url,
    listener: Option<TcpListener>,
}

impl LoopbackOAuthCallbackListener {
    pub fn new(redirect_uri: Url) -> Result<Self, AuthError> {
        if redirect_uri.scheme() != "http"
            || !matches!(redirect_uri.host_str(), Some("localhost" | "127.0.0.1"))
            || redirect_uri.path().trim_matches('/').is_empty()
        {
            return Err(AuthError::Config(
                "OAuth callback must be an HTTP localhost URL".to_owned(),
            ));
        }

        Ok(Self {
            actual_uri: redirect_uri.clone(),
            configured_uri: redirect_uri,
            listener: None,
        })
    }

    fn callback_url(&self, target: &str) -> Result<Url, AuthError> {
        let mut base = self.actual_uri.clone();
        base.set_query(None);
        let target = target.strip_prefix('/').unwrap_or(target);
        let path_and_query = format!("/{}", target);
        let parsed = Url::parse(&format!("http://localhost{path_and_query}"))
            .map_err(|error| AuthError::Callback(format!("invalid callback target: {error}")))?;
        base.set_path(parsed.path());
        base.set_query(parsed.query());
        Ok(base)
    }

    async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

#[async_trait]
impl OAuthCallbackListener for LoopbackOAuthCallbackListener {
    fn redirect_uri(&self) -> &Url {
        &self.actual_uri
    }

    async fn start(&mut self) -> Result<(), AuthError> {
        if self.listener.is_some() {
            return Ok(());
        }

        let port = self
            .configured_uri
            .port_or_known_default()
            .ok_or_else(|| AuthError::Config("OAuth callback port is missing".to_owned()))?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|error| {
                AuthError::Callback(format!("could not bind OAuth callback: {error}"))
            })?;
        let actual_port = listener
            .local_addr()
            .map_err(|error| AuthError::Callback(format!("could not inspect callback: {error}")))?
            .port();
        self.actual_uri
            .set_port(Some(actual_port))
            .map_err(|_| AuthError::Config("could not set callback port".to_owned()))?;
        self.listener = Some(listener);
        Ok(())
    }

    async fn wait(
        &mut self,
        expected_state: &str,
        timeout_duration: Duration,
    ) -> Result<OAuthCallbackResult, AuthError> {
        let listener = self.listener.take().ok_or_else(|| {
            AuthError::Callback("OAuth callback listener was not started".to_owned())
        })?;
        let timeout_duration =
            TokioDuration::from_secs(timeout_duration.num_seconds().max(1) as u64);
        let (mut stream, _) = timeout(timeout_duration, listener.accept())
            .await
            .map_err(|_| AuthError::Callback("OAuth callback timed out".to_owned()))?
            .map_err(|error| {
                AuthError::Callback(format!("could not accept OAuth callback: {error}"))
            })?;

        let mut buffer = vec![0_u8; 16 * 1024];
        let mut used = 0_usize;
        loop {
            if used == buffer.len() {
                Self::respond(
                    &mut stream,
                    "400 Bad Request",
                    "Authentication request was too large.",
                )
                .await;
                return Err(AuthError::Callback(
                    "OAuth callback request was too large".to_owned(),
                ));
            }
            let read = timeout(
                TokioDuration::from_secs(10),
                stream.read(&mut buffer[used..]),
            )
            .await
            .map_err(|_| AuthError::Callback("OAuth callback request timed out".to_owned()))?
            .map_err(|error| {
                AuthError::Callback(format!("could not read OAuth callback: {error}"))
            })?;
            if read == 0 {
                break;
            }
            used += read;
            if buffer[..used]
                .windows(4)
                .any(|window| window == b"\r\n\r\n")
            {
                break;
            }
        }

        let request = std::str::from_utf8(&buffer[..used])
            .map_err(|_| AuthError::Callback("OAuth callback was not valid UTF-8".to_owned()))?;
        let request_line = request
            .lines()
            .next()
            .ok_or_else(|| AuthError::Callback("OAuth callback request was empty".to_owned()))?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        if method != "GET" || target.is_empty() {
            Self::respond(
                &mut stream,
                "400 Bad Request",
                "Invalid authentication callback.",
            )
            .await;
            return Err(AuthError::Callback(
                "OAuth callback must be a GET request".to_owned(),
            ));
        }

        let callback_url = self.callback_url(target)?;
        if callback_url.path() != self.actual_uri.path() {
            Self::respond(
                &mut stream,
                "404 Not Found",
                "Invalid authentication callback.",
            )
            .await;
            return Err(AuthError::Callback(
                "OAuth callback path did not match".to_owned(),
            ));
        }

        let mut result = OAuthCallbackResult {
            code: None,
            state: None,
            error: None,
            error_description: None,
        };
        for (key, value) in callback_url.query_pairs() {
            match key.as_ref() {
                "code" => result.code = Some(value.into_owned()),
                "state" => result.state = Some(value.into_owned()),
                "error" => result.error = Some(value.into_owned()),
                "error_description" => result.error_description = Some(value.into_owned()),
                _ => {}
            }
        }

        let valid_state = result
            .state
            .as_deref()
            .is_some_and(|state| constant_time_equal(state.as_bytes(), expected_state.as_bytes()));
        if !valid_state {
            Self::respond(
                &mut stream,
                "400 Bad Request",
                "Invalid authentication state.",
            )
            .await;
            return Err(AuthError::Callback(
                "OAuth callback state mismatch".to_owned(),
            ));
        }

        Self::respond(
            &mut stream,
            "200 OK",
            "Authentication completed. You may close this window.",
        )
        .await;
        Ok(result)
    }
}

pub struct LoopbackOAuthCallbackListenerFactory;

#[async_trait]
impl OAuthCallbackListenerFactory for LoopbackOAuthCallbackListenerFactory {
    async fn create(
        &self,
        redirect_uri: &Url,
    ) -> Result<Box<dyn OAuthCallbackListener>, AuthError> {
        Ok(Box::new(LoopbackOAuthCallbackListener::new(
            redirect_uri.clone(),
        )?))
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut difference = (left.len() ^ right.len()) as u8;
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        difference |= left_byte ^ right_byte;
    }
    difference == 0
}
