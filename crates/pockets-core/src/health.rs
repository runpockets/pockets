//! Bounded HTTP readiness probes for a Pocket's loopback health endpoint.
//!
//! Probes are deliberately narrow so a health check can never leave the
//! machine: the base URL must be `http://127.0.0.1:<port>` on the root path
//! with no credentials, query or fragment, and the health path must match the
//! manifest `web.healthcheck` rule — absolute, exactly one leading slash, no
//! whitespace, query or fragment ([`validate_health_path`]; the bundled value
//! is `/health`). The client disables proxies and redirects and applies
//! a 2-second request timeout, so a response can neither be steered through a
//! proxy nor pull a redirect target from outside loopback.

use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::Url;

/// Health path of the bundled Pocket, for use as the `path` argument in
/// tests; production commands read `web.healthcheck` from the manifest.
#[cfg(test)]
pub const HEALTH_PATH: &str = "/health";

/// The only host a health probe may contact.
const LOOPBACK_HOST: &str = "127.0.0.1";

/// Hard limit for a single health request, including connection setup.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Total time [`wait_ready`] spends probing before giving up.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Pause between probes while waiting for readiness.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Probe `base_url` + `path` once.
///
/// `Ok(true)` only for a 2xx response. Connection failures, timeouts and any
/// non-2xx response — including redirects, which are never followed — are
/// `Ok(false)`: the service is simply not ready yet. An `Err` means the probe
/// target was rejected before any request was attempted.
pub async fn is_ready(base_url: &str, path: &str) -> Result<bool, String> {
    let client = probe_client()?;
    let url = validated_url(base_url, path)?;
    Ok(probe(&client, &url).await)
}

/// Probe every [`POLL_INTERVAL`] until the endpoint reports 2xx, hard-bounded
/// by [`TOTAL_TIMEOUT`]: a probe still in flight at the deadline is cut off
/// and reported as the total timeout.
///
/// Returns `Err` with an actionable message if the endpoint never becomes
/// ready; invalid targets fail immediately, before any probing.
pub async fn wait_ready(base_url: &str, path: &str) -> Result<(), String> {
    let client = probe_client()?;
    let url = validated_url(base_url, path)?;

    let deadline = tokio::time::Instant::now() + TOTAL_TIMEOUT;
    loop {
        // `timeout_at` bounds each probe by the total deadline, so a probe
        // can never run past it; elapsed means the window expired.
        match tokio::time::timeout_at(deadline, probe(&client, &url)).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(_elapsed) => {
                return Err(format!(
                    "The Pocket was not reachable at {url} after {} seconds. \
                     Make sure the Pocket container is running and its web server \
                     has started, then try again.",
                    TOTAL_TIMEOUT.as_secs(),
                ));
            }
        }
        let now = tokio::time::Instant::now();
        if now < deadline {
            tokio::time::sleep((deadline - now).min(POLL_INTERVAL)).await;
        }
    }
}

/// One probe round-trip: only 2xx counts as ready, everything else is not.
async fn probe(client: &reqwest::Client, url: &Url) -> bool {
    match client.get(url.clone()).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

/// Build the locked-down probe client: no proxy, no redirects, 2-second
/// timeout. TLS is not compiled in; only loopback HTTP is reachable anyway.
fn probe_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| format!("Could not create the health probe client: {err}"))
}

/// Validate `base_url` + `path` and combine them into the probe URL.
fn validated_url(base_url: &str, path: &str) -> Result<Url, String> {
    validate_health_path(path)?;

    let mut url =
        Url::parse(base_url).map_err(|err| format!("{base_url:?} is not a valid URL: {err}"))?;

    if url.scheme() != "http" {
        return Err(format!(
            "Health probes only accept plain http loopback URLs, found {base_url:?}"
        ));
    }
    // Exact literal: `localhost`, IPv6 loopback and other spellings are not
    // resolved here; only the address the engine actually publishes matches.
    if url.host_str() != Some(LOOPBACK_HOST) {
        return Err(format!(
            "Health probes only target {LOOPBACK_HOST}, found {base_url:?}"
        ));
    }
    match url.port() {
        Some(port) if port != 0 => {}
        _ => {
            return Err(format!(
                "Health probe URLs must include an explicit non-zero port, found {base_url:?}"
            ));
        }
    }
    if url.path() != "/" {
        return Err(format!(
            "Health probe base URLs must use the root path '/', found {base_url:?}"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "Health probe URLs must not carry credentials, found {base_url:?}"
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "Health probe URLs must not carry a query or fragment, found {base_url:?}"
        ));
    }

    // `set_path` keeps the validated path byte-for-byte: '/' is not in the
    // percent-encode set and everything else was rejected above.
    url.set_path(path);
    Ok(url)
}

/// Manifest `web.healthcheck` rule (schema `^/(?!/)[^\s?#]*$`): exactly one
/// leading slash, no whitespace, no query or fragment. Interior double
/// slashes such as `/a//b` are allowed by the schema and passed through.
fn validate_health_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.get(1..2) == Some("/") {
        return Err(format!(
            "The health path must begin with exactly one slash, found {path:?}"
        ));
    }
    if path.chars().any(char::is_whitespace) {
        return Err(format!(
            "The health path must not contain whitespace, found {path:?}"
        ));
    }
    if path.contains('?') || path.contains('#') {
        return Err(format!(
            "The health path must not contain a query string or fragment, found {path:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// Answer exactly one request with `response` and return its request
    /// line, on a background task so the probe runs concurrently.
    async fn serve_once(listener: Arc<TcpListener>, response: Vec<u8>) -> JoinHandle<String> {
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept probe");
            let mut request = [0u8; 512];
            let read = socket.read(&mut request).await.expect("read request");
            socket.write_all(&response).await.expect("write response");
            String::from_utf8_lossy(&request[..read])
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
    }

    async fn bind_loopback() -> (Arc<TcpListener>, String) {
        let listener = TcpListener::bind((LOOPBACK_HOST, 0))
            .await
            .expect("bind ephemeral loopback port");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        (Arc::new(listener), base)
    }

    #[tokio::test]
    async fn healthy_200_is_ready_and_external_bases_are_rejected() {
        let (listener, base) = bind_loopback().await;
        let server = serve_once(
            listener,
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
        )
        .await;

        assert_eq!(is_ready(&base, HEALTH_PATH).await, Ok(true));
        // The probe requested the health path on the validated loopback URL.
        assert_eq!(server.await.expect("server task"), "GET /health HTTP/1.1");

        // Rejected up front, before any request leaves the process.
        for base_url in [
            "http://example.com:8080",
            "https://127.0.0.1:8080",
            "http://localhost:8080",
            "http://127.0.0.1",
        ] {
            assert!(
                is_ready(base_url, HEALTH_PATH).await.is_err(),
                "{base_url} must be rejected",
            );
        }
    }

    #[tokio::test]
    async fn redirect_302_is_not_ready_and_is_not_followed() {
        let (listener, base) = bind_loopback().await;
        // The redirect target stays on the same local listener, so following
        // it would show up as a second connection we can actually observe.
        let redirect =
            format!("HTTP/1.1 302 Found\r\nlocation: {base}/redirected\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let server = serve_once(listener.clone(), redirect.into_bytes()).await;

        assert_eq!(is_ready(&base, HEALTH_PATH).await, Ok(false));
        server.await.expect("server task");

        // A client that follows redirects does so before `send()` returns, so
        // any second connection would already be pending. None may arrive.
        let followed = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(
            followed.is_err(),
            "a 302 must not be followed, found a second connection: {followed:?}",
        );
    }
}
