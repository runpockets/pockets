//! Tests for the preview proxy: the pure tier/injection tables, the asset
//! surface, and the loopback socket behavior against fake in-process
//! upstreams.

use super::*;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::Mutex;

/// Session-independent fixture: the exact 64-hex sha256
/// `spikes/morph/RESULTS.md` records for morphdom 2.7.8.
const MORPHDOM_SHA256: &str = "6b5bc205c08800dd59f1792032f723b7ee3aedae70ef64353c8233e0eed7c1cb";

/// The single injected tag, spelled once here so both injection and
/// end-to-end tests assert the same wire contract as `serve_asset` serves.
const INJECTED_TAG: &[u8] = b"<script src=\"/__pockets-live/client.js\" defer></script>";

/// A request fixture with the given method and headers.
fn fixture_request(method: &str, headers: &[(&str, &str)]) -> Request {
    Request {
        method: method.to_owned(),
        path: "/".to_owned(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

/// A response-head fixture; `status_line` only matters for byte-level relay,
/// which the socket tests assert, so the fixture carries an empty one.
fn fixture_response(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
    ResponseHead {
        status,
        reason: "Test".to_owned(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        status_line: Vec::new(),
        upgraded: status == 101,
    }
}

#[test]
fn tier_of_follows_the_documented_extension_table() {
    // docs/live-preview.md "Hot reload contract": CSS swaps, everything else
    // morphs, static JS and binary assets hard-reload (the documented
    // conservative choices, see `Tier::Reload`).
    for path in ["style.css", "public/app.CSS", "theme.min.Css"] {
        assert_eq!(tier_of(path), Tier::Style, "{path} is a stylesheet");
    }
    // The root server entry is server code: restart, then morph (contract).
    assert_eq!(
        tier_of("server.js"),
        Tier::Morph,
        "the server entry morphs after restart"
    );
    assert_eq!(
        tier_of("public/server.js"),
        Tier::Reload,
        "a served script of the same name is static"
    );
    for path in [
        "public/app.mjs",
        "Legacy.CJS",
        "public/logo.png",
        "a/b/c.JPEG",
        "fonts/body.woff2",
        "favicon.ico",
    ] {
        assert_eq!(tier_of(path), Tier::Reload, "{path} must hard-reload");
    }
    for path in [
        "server.js.bak",
        "pages/index.html",
        "app.tsx",
        "pocket.yaml",
        "README",
        "public/data.json",
        "images",    // extension-less directory-style path
        "v1.2/app2", // dot in a directory, no file extension
        ".env",      // dotfile: no extension
        "weird",
    ] {
        assert_eq!(tier_of(path), Tier::Morph, "{path} falls through to morph");
    }
}

#[test]
fn messages_for_aggregates_tiers_in_order() {
    // CSS only: one style frame carrying exactly the CSS paths.
    let frames = messages_for(&["public/a.css".into(), "b.css".into()]);
    assert_eq!(
        frames,
        vec![ClientMessage::Style {
            paths: vec!["public/a.css".to_owned(), "b.css".to_owned()],
        }]
    );

    // Markup only: one morph frame.
    assert_eq!(
        messages_for(&["pages/index.html".into()]),
        vec![ClientMessage::Morph]
    );

    // Mixed markup + CSS: morph FIRST (a morph re-applies the server's
    // original stylesheet hrefs), then the style cache-buster.
    let frames = messages_for(&["style.css".into(), "server.js".into()]);
    assert_eq!(
        frames,
        vec![
            ClientMessage::Morph,
            ClientMessage::Style {
                paths: vec!["style.css".to_owned()],
            },
        ]
    );

    // Any reload-tier path preempts the rest: a reload subsumes them.
    for paths in [
        vec!["app.js".to_owned(), "style.css".to_owned()],
        vec!["style.css".to_owned(), "logo.png".to_owned()],
        vec!["server.js".to_owned(), "app.js".to_owned()],
    ] {
        assert_eq!(
            messages_for(&paths),
            vec![ClientMessage::Reload],
            "{paths:?}"
        );
    }

    // Empty change list ("restarted, nothing specific reported"): re-sync.
    assert_eq!(messages_for(&[]), vec![ClientMessage::Morph]);
}

#[test]
fn client_message_json_matches_the_documented_shapes() {
    // The wire shapes the injected client switches on (docs "Hot reload
    // contract" + the proxy status relay).
    assert_eq!(
        serde_json::to_string(&ClientMessage::Style {
            paths: vec!["a.css".to_owned()],
        })
        .expect("json"),
        r#"{"type":"style","paths":["a.css"]}"#
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::Morph).expect("json"),
        r#"{"type":"morph"}"#
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::Reload).expect("json"),
        r#"{"type":"reload"}"#
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::State {
            state: PreviewState::Ready,
        })
        .expect("json"),
        r#"{"type":"state","state":"ready"}"#
    );
}

#[test]
fn injection_happens_only_for_2xx_text_html() {
    let get = fixture_request("GET", &[]);
    let html = fixture_response(200, &[("content-type", "text/html")]);
    assert!(
        should_inject(&get, &html),
        "2xx text/html is the inject tier"
    );

    // Charset and casing still identify the media type.
    assert!(
        should_inject(
            &get,
            &fixture_response(201, &[("content-type", "TEXT/HTML; charset=utf-8")])
        ),
        "2xx with a charset or uppercase type still injects"
    );

    // Identity encoding is uncompressed and safe; anything else passes through.
    assert!(should_inject(
        &get,
        &fixture_response(
            200,
            &[
                ("content-type", "text/html"),
                ("content-encoding", "identity")
            ]
        )
    ));
    assert!(
        !should_inject(
            &get,
            &fixture_response(
                200,
                &[("content-type", "text/html"), ("content-encoding", "gzip")]
            )
        ),
        "injecting into compressed bytes would corrupt them"
    );

    for (label, request, response) in [
        (
            "non-html content type",
            get.clone(),
            fixture_response(200, &[("content-type", "application/json")]),
        ),
        (
            "css is an asset, not a document",
            get.clone(),
            fixture_response(200, &[("content-type", "text/css")]),
        ),
        (
            "redirect",
            get.clone(),
            fixture_response(302, &[("content-type", "text/html")]),
        ),
        (
            "not modified",
            get.clone(),
            fixture_response(304, &[("content-type", "text/html")]),
        ),
        (
            "no body",
            get.clone(),
            fixture_response(204, &[("content-type", "text/html")]),
        ),
        (
            "partial content must not be re-framed",
            get.clone(),
            fixture_response(206, &[("content-type", "text/html")]),
        ),
        (
            "HEAD has no body",
            fixture_request("HEAD", &[]),
            fixture_response(200, &[("content-type", "text/html")]),
        ),
        (
            "POST is not a navigation",
            fixture_request("POST", &[]),
            fixture_response(200, &[("content-type", "text/html")]),
        ),
    ] {
        assert!(
            !should_inject(&request, &response),
            "must not inject: {label}"
        );
    }

    // The actual upgrade response shape (101) is excluded by `upgraded`.
    assert!(
        !should_inject(
            &get,
            &fixture_response(101, &[("content-type", "text/html")])
        ),
        "101 must never inject"
    );
}

#[test]
fn inject_client_inserts_before_the_last_body_close() {
    let mut expected = b"<html><body><p>hi</p>".to_vec();
    expected.extend_from_slice(INJECTED_TAG);
    expected.extend_from_slice(b"</body></html>");
    assert_eq!(
        inject_client(b"<html><body><p>hi</p></body></html>"),
        expected
    );

    // Case-insensitive scan, LAST closing tag wins: an inline script's
    // string literal `"</body>"` earlier in the document must not win.
    let injected = inject_client(b"<body><script>var x = '</BODY >';</script>late</body></html>");
    let text = String::from_utf8(injected).expect("utf-8 fixture");
    assert!(text.ends_with("</body></html>"), "{text}");
    let tag_at = text.find(INJECTED_TAG_STR).expect("tag present");
    let last_close = text.rfind("</body>").expect("close tag");
    assert!(tag_at < last_close, "tag goes before the LAST close");
    assert!(
        text.matches(INJECTED_TAG_STR).count() == 1,
        "exactly one tag"
    );

    // Whitespace-then-`>` close and attribute-carrying close still match.
    let text = String::from_utf8(inject_client(b"<BODY CLASS=x></BODY >")).expect("utf-8 fixture");
    assert_eq!(text, format!("<BODY CLASS=x>{INJECTED_TAG_STR}</BODY >"));

    // No closing tag at all: appended.
    let text = String::from_utf8(inject_client(b"<p>open document")).expect("utf-8 fixture");
    assert_eq!(text, format!("<p>open document{INJECTED_TAG_STR}"));

    // Empty and binary-ish bodies survive untouched-plus-tag.
    assert_eq!(
        String::from_utf8_lossy(&inject_client(b"")),
        INJECTED_TAG_STR
    );
}

/// The injected tag as text, for the string-oriented assertions above.
const INJECTED_TAG_STR: &str = "<script src=\"/__pockets-live/client.js\" defer></script>";

#[test]
fn morphdom_asset_bytes_match_the_recorded_sha256() {
    // The embedded asset must stay byte-identical to the spike-verified copy
    // (docs/live-preview.md: "a Pockets-owned vendored asset — morphdom
    // 2.7.8 (MIT ...) spike-proven"). Any edit to the asset breaks this.
    let digest = Sha256::digest(MORPHDOM_JS.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(hex, MORPHDOM_SHA256);
    assert_eq!(
        MORPHDOM_JS.len(),
        31952,
        "31,952 bytes per spikes/morph/RESULTS.md"
    );

    // The client is Pockets-owned and dependency-free; it must reference the
    // reserved asset paths it actually fetches (built from its own PREFIX).
    assert!(CLIENT_JS.contains("const PREFIX = \"/__pockets-live\""));
    assert!(CLIENT_JS.contains("PREFIX + \"/morphdom.js\""));
    assert!(CLIENT_JS.contains("PREFIX + \"/stream\""));
    assert!(
        CLIENT_JS.contains("onBeforeElUpdated"),
        "the form-preservation guard ships"
    );
    assert!(CLIENT_JS.contains("cache") && CLIENT_JS.contains("no-store"));
}

#[test]
fn parse_upstream_accepts_only_the_literal_loopback_mapping() {
    assert_eq!(
        parse_upstream("http://127.0.0.1:8080").expect("the exact shape preview.rs builds"),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
    );
    for rejected in [
        "http://localhost:8080",
        "http://0.0.0.0:8080",
        "http://10.0.0.2:80",
        "https://127.0.0.1:8080",
        "http://127.0.0.1:0",
        "http://127.0.0.1",
        "http://127.0.0.1:8080/path",
        "http://user@127.0.0.1:8080",
        "http://127.0.0.1:8080?x=1",
        "http://127.0.0.1:notaport",
    ] {
        assert!(
            parse_upstream(rejected).is_err(),
            "{rejected} must be refused before any socket opens"
        );
    }
}
/// An in-process fake upstream answering queued raw responses, one per
/// accepted connection (the proxy speaks one request per connection to the
/// container), recording every request line it received.
async fn fake_upstream(responses: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake upstream");
    let port = listener.local_addr().expect("upstream addr").port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_task = Arc::clone(&seen);
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (mut socket_read, mut socket_write) = socket.into_split();
            let mut reader = BufReader::new(&mut socket_read);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).await.is_err() {
                continue;
            }
            // Drain the header block; fixture bodies are empty.
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            seen_task
                .lock()
                .expect("seen lock")
                .push(request_line.trim_end().to_owned());
            let Some(response) = queue.lock().expect("queue lock").pop_front() else {
                continue;
            };
            let _ = socket_write.write_all(&response).await;
            let _ = socket_write.flush().await;
            // The write-half drops at the end of the iteration: the
            // connection closes, which is the framing the fixtures declare.
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

/// Raw HTTP/1.1 over the proxy's loopback port, reading to EOF (the proxy
/// closes after every response, so EOF is the framing signal).
async fn proxy_raw(port: u16, request: &[u8]) -> Vec<u8> {
    let mut socket = TcpStream::connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
        .await
        .expect("connect proxy");
    socket.write_all(request).await.expect("send request");
    let mut response = Vec::new();
    socket
        .read_to_end(&mut response)
        .await
        .expect("read response");
    response
}

async fn proxy_get(port: u16, path: &str) -> Vec<u8> {
    proxy_raw(
        port,
        format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n").as_bytes(),
    )
    .await
}

/// Split a raw response into its head (lowercased header block without the
/// status line's case assumptions) and body.
fn split_response(response: &[u8]) -> (String, Vec<u8>) {
    let at = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("head terminator");
    (
        String::from_utf8_lossy(&response[..at]).to_ascii_lowercase(),
        response[at + 4..].to_vec(),
    )
}

fn status_line(response: &[u8]) -> String {
    String::from_utf8_lossy(response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// A raw, framed HTML answer for the fake upstream.
fn html_response(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[tokio::test]
async fn proxy_binds_loopback_only() {
    let (upstream_url, _seen) = fake_upstream(Vec::new()).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender).await.expect("spawn proxy");
    let local = handle.local_addr();
    assert_eq!(
        local.ip(),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        "the listener must never bind a wildcard address"
    );
    assert_eq!(handle.url(), format!("http://{local}"));
    handle.stop();
}

#[tokio::test]
async fn reserved_paths_are_served_locally_and_never_reach_upstream() {
    let (upstream_url, seen) = fake_upstream(Vec::new()).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender).await.expect("spawn proxy");
    let port = handle.local_addr().port();

    let client = proxy_get(port, "/__pockets-live/client.js").await;
    let (head, body) = split_response(&client);
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(body, CLIENT_JS.as_bytes());

    let morphdom = proxy_get(port, "/__pockets-live/morphdom.js").await;
    let (head, body) = split_response(&morphdom);
    assert!(head.contains("content-type: text/javascript"), "{head}");
    assert_eq!(body, MORPHDOM_JS.as_bytes());

    let missing = proxy_get(port, "/__pockets-live/nope").await;
    assert!(
        status_line(&missing).starts_with("HTTP/1.1 404"),
        "{missing:?}"
    );

    // Even the open-then-abandon SSE connection must not touch upstream.
    let mut stream_socket =
        TcpStream::connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .expect("connect sse");
    stream_socket
        .write_all(
            format!("GET /__pockets-live/stream HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send sse request");
    drop(stream_socket);

    let recorded = seen.lock().expect("seen lock").clone();
    assert!(
        recorded.is_empty(),
        "the reserved prefix never proxies upstream: {recorded:?}"
    );
    handle.stop();
}

#[tokio::test]
async fn client_script_is_served_with_a_javascript_content_type() {
    let (upstream_url, _seen) = fake_upstream(Vec::new()).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender).await.expect("spawn proxy");
    let response = proxy_get(handle.local_addr().port(), "/__pockets-live/client.js").await;
    let (head, body) = split_response(&response);
    assert!(
        head.contains("content-type: text/javascript"),
        "the injected client must be served as JavaScript: {head}"
    );
    assert_eq!(body, CLIENT_JS.as_bytes());
    handle.stop();
}

#[tokio::test]
async fn html_is_injected_and_assets_pass_through_byte_identical() {
    let page = b"<html><body><h1>Preview</h1></body></html>";
    let css = b"body{color:red}\n";
    let upstream_css = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/css\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        css.len()
    )
    .into_bytes();
    let mut css_response = upstream_css.clone();
    css_response.extend_from_slice(css);

    let (upstream_url, seen) = fake_upstream(vec![html_response(page), css_response]).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender).await.expect("spawn proxy");
    let port = handle.local_addr().port();

    let proxied = proxy_get(port, "/").await;
    let (head, body) = split_response(&proxied);
    assert!(head.contains("content-type: text/html"), "{head}");
    let mut expected = b"<html><body><h1>Preview</h1>".to_vec();
    expected.extend_from_slice(INJECTED_TAG);
    expected.extend_from_slice(b"</body></html>");
    assert_eq!(body, expected);
    // The length was recomputed for the longer body, and chunking is off.
    assert!(
        head.contains(&format!("content-length: {}", expected.len())),
        "{head}"
    );

    let proxied_css = proxy_get(port, "/style.css").await;
    let (head, body) = split_response(&proxied_css);
    assert!(head.contains("content-type: text/css"), "{head}");
    assert_eq!(body, css.to_vec(), "non-HTML bytes survive exactly");

    // Both requests reached upstream in origin-form, exactly once.
    assert_eq!(
        seen.lock().expect("seen lock").clone(),
        vec!["GET / HTTP/1.1", "GET /style.css HTTP/1.1"]
    );
    handle.stop();
}

#[tokio::test]
async fn chunked_assets_pass_through_relayed_frame() {
    let asset = b"chunked-asset-payload";
    let mut upstream = b"HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
        .to_vec();
    upstream.extend_from_slice(format!("{:x}\r\n", asset.len()).as_bytes());
    upstream.extend_from_slice(asset);
    upstream.extend_from_slice(b"\r\n0\r\n\r\n");

    let (upstream_url, _seen) = fake_upstream(vec![upstream]).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender).await.expect("spawn proxy");
    let proxied = proxy_get(handle.local_addr().port(), "/logo.png").await;
    let (head, body) = split_response(&proxied);
    assert!(
        head.contains("transfer-encoding: chunked"),
        "chunk framing is re-asserted on relay: {head}"
    );
    assert!(
        body.starts_with(format!("{:x}\r\n", asset.len()).as_bytes()),
        "chunk size line is re-emitted: {body:?}"
    );
    assert!(
        body.windows(asset.len()).any(|window| window == asset),
        "payload survives"
    );
    assert!(body.ends_with(b"0\r\n\r\n"), "terminator survives");
    handle.stop();
}

#[tokio::test]
async fn redirects_304s_and_head_responses_pass_through_uninjected() {
    let redirect = b"HTTP/1.1 302 Found\r\nlocation: /elsewhere\r\ncontent-type: text/html\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec();
    let not_modified =
        b"HTTP/1.1 304 Not Modified\r\netag: \"abc\"\r\nconnection: close\r\n\r\n".to_vec();
    // HEAD to an HTML page: length header present, body absent. The proxy
    // must not wait for (or inject into) a body.
    let head_response = b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 999\r\nconnection: close\r\n\r\n".to_vec();

    let (upstream_url, _seen) = fake_upstream(vec![redirect, not_modified, head_response]).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender).await.expect("spawn proxy");
    let port = handle.local_addr().port();

    let proxied = proxy_get(port, "/gone").await;
    let (head, body) = split_response(&proxied);
    assert!(head.starts_with("http/1.1 302"), "{head}");
    assert!(head.contains("location: /elsewhere"), "{head}");
    assert!(body.is_empty(), "a redirect body is not injected");

    let proxied = proxy_get(port, "/cached").await;
    let (head, body) = split_response(&proxied);
    assert!(head.starts_with("http/1.1 304"), "{head}");
    assert!(head.contains("etag: \"abc\""), "{head}");
    assert!(body.is_empty());

    let proxied = proxy_raw(
        port,
        format!("HEAD / HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n").as_bytes(),
    )
    .await;
    let (head, body) = split_response(&proxied);
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/html"), "{head}");
    assert!(body.is_empty(), "HEAD never grows a body: {body:?}");
    handle.stop();
}

#[tokio::test]
async fn sse_fanout_delivers_broadcast_messages_as_client_frames() {
    let (upstream_url, _seen) = fake_upstream(Vec::new()).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender.clone())
        .await
        .expect("spawn proxy");
    let port = handle.local_addr().port();

    let mut socket = TcpStream::connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
        .await
        .expect("connect sse");
    socket
        .write_all(
            format!("GET /__pockets-live/stream HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send sse request");
    let (read_half, _write_half) = socket.into_split();
    let mut reader = BufReader::new(read_half);

    let head = read_http_head(&mut reader).await;
    assert!(head.contains("content-type: text/event-stream"), "{head}");

    // Announced reconnect cadence arrives before any signal.
    let first = read_data_frame(&mut reader).await;
    assert!(
        first.is_empty(),
        "the retry frame is a comment line: {first:?}"
    );

    // Style tier: changed CSS becomes exactly the documented style message.
    sender
        .send(LiveSignal::Changed(vec!["public/style.css".to_owned()]))
        .expect("subscribed");
    assert_eq!(
        read_data_frame(&mut reader).await,
        r#"{"type":"style","paths":["public/style.css"]}"#
    );

    // Morph tier and the plain status relay.
    sender
        .send(LiveSignal::Changed(vec!["server.js".to_owned()]))
        .expect("subscribed");
    assert_eq!(read_data_frame(&mut reader).await, r#"{"type":"morph"}"#);
    sender
        .send(LiveSignal::State(PreviewState::Ready))
        .expect("subscribed");
    assert_eq!(
        read_data_frame(&mut reader).await,
        r#"{"type":"state","state":"ready"}"#
    );
    handle.stop();
}

#[tokio::test]
async fn stop_shuts_the_listener_and_ends_open_streams() {
    let (upstream_url, _seen) = fake_upstream(Vec::new()).await;
    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&upstream_url, sender.clone())
        .await
        .expect("spawn proxy");
    let port = handle.local_addr().port();

    let mut socket = TcpStream::connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
        .await
        .expect("connect sse");
    socket
        .write_all(
            format!("GET /__pockets-live/stream HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send sse request");
    let (mut read_half, _write_half) = socket.into_split();

    // Wait until the handler has actually answered before stopping, or the
    // test would race the accept and observe a stop with nothing written.
    let mut first = [0u8; 1];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_half.read(&mut first),
    )
    .await
    .expect("the sse stream answers")
    .expect("first byte");

    handle.stop();
    // With the listener down and the last sender dropped (the registry's
    // clone, simulated here), the open stream must terminate on its own.
    drop(sender);
    let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut all = Vec::new();
        // Termination is the contract, not the TCP flavor: dropping the
        // listener may FIN the open stream cleanly or RST it mid-read;
        // EventSource reconnects on either. Frames must still have arrived.
        match read_half.read_to_end(&mut all).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(err) => panic!("unexpected sse termination: {err}"),
        }
        all
    })
    .await
    .expect("the stream ends once every sender is gone");
    assert!(
        !drained.is_empty(),
        "head + retry frame arrived before the close"
    );

    let refused = TcpStream::connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)).await;
    assert!(refused.is_err(), "the listener is shut down: {refused:?}");
}

#[tokio::test]
async fn end_to_end_std_upstream_proxy_injected_page_and_untouched_asset_bytes() {
    // The requirement's "two in-process std listeners" shape: a plain
    // std::net fake upstream serving one HTML page and one asset, and the
    // real proxy module in front of it.
    let page =
        b"<!doctype html><html><head><title>P</title></head><body><main>hi</main></body></html>";
    let asset: Vec<u8> = (0u8..=255).cycle().take(512).collect(); // arbitrary bytes

    let upstream = std::net::TcpListener::bind("127.0.0.1:0").expect("bind std upstream");
    let upstream_port = upstream.local_addr().expect("upstream addr").port();
    // The socket is created and bound with std; tokio registration happens
    // inside the runtime (from_std requires it), the accept loop stays a
    // plain loop answering each connection once and closing.
    upstream
        .set_nonblocking(true)
        .expect("nonblocking std listener");
    let upstream = tokio::net::TcpListener::from_std(upstream).expect("adopt std listener");
    let page_body = page.to_vec();
    let asset_body = asset.clone();
    tokio::spawn(async move {
        // Accept a bounded number of connections; each answers and closes.
        for _ in 0..8 {
            let Ok((stream, _)) = upstream.accept().await else {
                return;
            };
            let page_body = page_body.clone();
            let asset_body = asset_body.clone();
            tokio::spawn(async move {
                let (mut read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(&mut read_half);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.is_err() {
                    return;
                }
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                let route = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let body: &[u8] = match route.as_str() {
                    "/" => &page_body,
                    "/bytes.bin" => &asset_body,
                    _ => b"missing",
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    if route == "/" { "text/html" } else { "application/octet-stream" },
                    body.len(),
                );
                let _ = write_half.write_all(head.as_bytes()).await;
                let _ = write_half.write_all(body).await;
                let _ = write_half.flush().await;
            });
        }
    });

    let (sender, _receiver) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&format!("http://127.0.0.1:{upstream_port}"), sender)
        .await
        .expect("spawn proxy");
    let port = handle.local_addr().port();

    let proxied = proxy_get(port, "/").await;
    let (_head, body) = split_response(&proxied);
    let mut expected = Vec::new();
    expected.extend_from_slice(
        b"<!doctype html><html><head><title>P</title></head><body><main>hi</main>",
    );
    expected.extend_from_slice(INJECTED_TAG);
    expected.extend_from_slice(b"</body></html>");
    assert_eq!(body, expected, "the injected page, exactly once");

    let proxied_asset = proxy_get(port, "/bytes.bin").await;
    let (_head, body) = split_response(&proxied_asset);
    assert_eq!(body, asset, "asset bytes survive byte-identical");

    handle.stop();
}

/// Read the raw HTTP head (up to the blank line) from an SSE stream.
async fn read_http_head<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> String {
    let mut head = Vec::new();
    loop {
        let byte = {
            let mut one = [0u8; 1];
            match reader.read(&mut one).await.expect("read head") {
                0 => panic!("stream closed before head completed"),
                _ => one[0],
            }
        };
        head.push(byte);
        if head.ends_with(b"\r\n\r\n") {
            return String::from_utf8_lossy(&head).to_ascii_lowercase();
        }
    }
}

/// Read SSE frames until the next `data:` frame, skipping comment frames,
/// and return its JSON text (empty for comment-only frames such as retry).
async fn read_data_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> String {
    let timeout = std::time::Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        let mut frame = Vec::new();
        loop {
            let mut line = Vec::new();
            let read = reader
                .read_until(b'\n', &mut line)
                .await
                .expect("read frame line");
            if read == 0 {
                panic!("stream closed mid-frame");
            }
            frame.extend_from_slice(&line);
            if frame.ends_with(b"\n\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&frame).into_owned();
        let line = text
            .lines()
            .find(|line| line.starts_with("data: "))
            .unwrap_or_default();
        line.strip_prefix("data: ").unwrap_or_default().to_owned()
    })
    .await
    .expect("a frame arrives well inside the heartbeat window")
}

#[tokio::test]
async fn forwarded_head_is_well_formed_for_a_strict_upstream() {
    // The regression this exists for: the asserted `Host` line was once
    // pre-terminated, so `write_head` emitted a stray blank line, ending the
    // forwarded head early. Node read the following `Connection` line as
    // pipelined garbage and answered a bare 400 — every injectable HTML page
    // through the proxy failed while lenient fake upstreams stayed green.
    // A strict upstream that parses the head exactly is what catches it.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind strict upstream");
    let port = listener.local_addr().expect("addr").port();
    let (sender, _rx) = broadcast::channel::<LiveSignal>(LIVE_CHANNEL_CAPACITY);
    let handle = spawn(&format!("http://127.0.0.1:{port}"), sender)
        .await
        .expect("spawn proxy");
    let local = handle.local_addr();

    // The client request is issued concurrently: the proxy blocks writing
    // the forwarded head until this test (as the strict upstream) reads it.
    // The head carries one header that must survive, three hop-by-hop ones
    // that must be stripped, and a lowercase `host` the rewrite replaces.
    let client = tokio::spawn(async move {
        proxy_raw(
            local.port(),
            b"GET / HTTP/1.1\r\nhost: 127.0.0.1:9\r\naccept: */*\r\nkeep-alive: 5\r\nproxy-authorization: Basic x\r\nconnection: keep-alive\r\n\r\n",
        )
        .await
    });

    let (mut up, _peer) = listener.accept().await.expect("upstream accept");
    let mut raw: Vec<u8> = Vec::new();
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        if up.read(&mut byte).await.expect("read upstream") == 0 {
            break;
        }
        raw.push(byte[0]);
    }
    let head = String::from_utf8(raw).expect("ascii head");
    let mut lines = head.split("\r\n").collect::<Vec<_>>();
    let terminator = lines.iter().position(|line| line.is_empty()).unwrap();
    lines.truncate(terminator); // everything the parser treats as the head

    assert_eq!(lines[0], "GET / HTTP/1.1", "origin-form request line");
    let forwarded = &lines[1..];
    assert!(
        forwarded.iter().any(|line| *line == "accept: */*"),
        "client headers survive: {forwarded:?}"
    );
    let expected_host = format!("Host: 127.0.0.1:{port}");
    assert!(
        forwarded
            .iter()
            .any(|line| line.eq_ignore_ascii_case(expected_host.as_str())),
        "Host rewritten to the upstream, inside the head: {forwarded:?}"
    );
    assert!(
        forwarded
            .iter()
            .any(|line| line.eq_ignore_ascii_case("Connection: close")),
        "Connection is asserted inside the head, never after its terminator: {forwarded:?}"
    );
    for banned in [
        "keep-alive:",
        "proxy-authorization:",
        "connection: keep-alive",
        "host: 127.0.0.1:9",
    ] {
        assert!(
            !forwarded
                .iter()
                .any(|line| line.to_ascii_lowercase().starts_with(banned)),
            "{banned} must not reach the upstream: {forwarded:?}"
        );
    }
    // Exactly one Host line: the rewrite must not stack on a surviving one.
    assert_eq!(
        forwarded
            .iter()
            .filter(|line| line.to_ascii_lowercase().starts_with("host:"))
            .count(),
        1,
        "{forwarded:?}"
    );

    // Answer like the starter (chunked HTML, keep-alive) so the injection
    // path runs end to end over the verified head.
    let body = b"<!doctype html><html><body><p>ok</p></body></html>";
    let mut reply = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n",
        body.len()
    )
    .into_bytes();
    reply.extend_from_slice(body);
    reply.extend_from_slice(b"\r\n0\r\n\r\n");
    up.write_all(&reply).await.expect("reply");
    up.flush().await.expect("flush");

    let response = client.await.expect("client task");
    assert!(
        response.starts_with(b"HTTP/1.1 200 OK"),
        "a strict parser's answer must be the injected 200: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        response
            .windows(INJECTED_TAG.len())
            .any(|window| window == INJECTED_TAG),
        "the client tag is injected"
    );
    handle.stop();
}
