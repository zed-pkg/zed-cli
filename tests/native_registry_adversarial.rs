//! Adversarial tests for the native-registry client.
//!
//! Every host in `native_host_client` is a third party. Some are enterprise
//! mirrors an operator configured, some are public registries, and a release
//! job hands several of them a publish token. So the client has to hold up
//! against a *hostile* registry, not just an unavailable one — and that is
//! exactly what a fixture in `zed-pkg-test/security-adversarial-e2e` is for.
//!
//! These run against a local one-shot HTTP server rather than a real registry:
//! the failure modes here (an unbounded body, a redirect chasing a credential)
//! are ones no public registry will reproduce on demand, and testing them
//! against `registry.npmjs.org` would be both unreliable and rude.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use zed_cli::native_host_client::{
    HeaderValue, Method, RegistryRequest, RequestBody, execute,
};
use zed_interfaces::native_host::{ChannelRoute, HostEndpoints, NativeHost, ReleaseChannel};

/// What one served request looked like from the server's side.
#[derive(Debug, Default, Clone)]
struct Seen {
    path: String,
    authorization: Option<String>,
}

/// Read one request, hand back `respond(path)`, and report what arrived.
///
/// Deliberately minimal: a real HTTP mock crate would be a new dependency in a
/// tool that handles publish tokens, and this needs to parse exactly one
/// request line and one header.
fn serve<F>(count: usize, respond: F) -> (String, mpsc::Receiver<Seen>)
where
    F: Fn(&str) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..count {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            if let Some(seen) = handle(stream, &respond) {
                let _ = tx.send(seen);
            }
        }
    });
    (format!("http://{addr}"), rx)
}

fn handle<F>(mut stream: TcpStream, respond: &F) -> Option<Seen>
where
    F: Fn(&str) -> Vec<u8>,
{
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let mut seen = Seen {
        path: path.clone(),
        authorization: None,
    };
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            seen.authorization = Some(value.trim().to_string());
        }
    }

    let _ = stream.write_all(&respond(&path));
    let _ = stream.flush();
    Some(seen)
}

fn route_at(base: &str) -> ChannelRoute {
    let mut route = NativeHost::Npm
        .channel_route("1.0.0", ReleaseChannel::Stable, 1)
        .expect("npm stable route");
    route.endpoints = HostEndpoints {
        publish: Some(base.to_string()),
        index: base.to_string(),
        download: None,
    };
    route
}

fn get(url: &str) -> RegistryRequest {
    RegistryRequest {
        method: Method::Get,
        url: url.to_string(),
        url_contains_secret: false,
        headers: Vec::new(),
        body: RequestBody::Empty,
    }
}

fn authed_get(url: &str, token: &str) -> RegistryRequest {
    let mut request = get(url);
    request.headers.push((
        "Authorization".to_string(),
        HeaderValue::Secret(format!("Bearer {token}")),
    ));
    request
}

#[test]
fn an_oversized_index_response_is_refused_rather_than_buffered() {
    // A registry reports its own sizes, and a hostile or compromised one can
    // report anything — so the cap has to be the client's, enforced while
    // reading. `zed-cli`'s Zed-registry client already does this
    // (`ZED_PKG_MAX_ARTIFACT_BYTES`); the native client reached parity here.
    //
    // Without it, one `zed release versions` against a bad mirror is an OOM.
    let body = vec![b'x'; 4 * 1024 * 1024];
    let (base, _rx) = serve(1, move |_| {
        let mut out =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(&body);
        out
    });

    // SAFETY: single-threaded test process; the value is read by `execute` on
    // this thread before any other test observes it.
    unsafe { std::env::set_var("ZED_PKG_MAX_REGISTRY_BYTES", "1048576") };
    let error = execute(&get(&format!("{base}/big"))).expect_err("4 MiB must exceed a 1 MiB cap");
    unsafe { std::env::remove_var("ZED_PKG_MAX_REGISTRY_BYTES") };

    let message = format!("{error:#}");
    assert!(
        message.contains("exceeds") || message.contains("too large"),
        "the error should say the response was too large, got: {message}"
    );
}

#[test]
fn a_credential_never_follows_a_redirect_to_another_host() {
    // The classic registry-client credential leak: a compromised or merely
    // sloppy registry answers 302 and the client replays `Authorization` at
    // whatever it points to. A publish token is in scope on every one of these
    // requests, so this must hold regardless of the HTTP library's defaults.
    let (victim, victim_rx) = serve(1, |_| b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec());
    let victim_for_redirect = victim.clone();

    // `localhost` and `127.0.0.1` resolve to the same machine but are
    // different hosts, which is exactly the boundary that matters.
    let (attacker, _attacker_rx) = serve(1, move |_| {
        let target = victim_for_redirect.replace("127.0.0.1", "localhost");
        format!("HTTP/1.1 302 Found\r\nLocation: {target}/stolen\r\nContent-Length: 0\r\n\r\n")
            .into_bytes()
    });

    let _ = execute(&authed_get(
        &format!("{attacker}/index"),
        "super-secret-publish-token",
    ));

    let seen = victim_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the redirect target should have been reached");
    assert_eq!(seen.path, "/stolen");
    assert!(
        seen.authorization.is_none(),
        "the credential followed a cross-host redirect: {:?}",
        seen.authorization
    );
}

#[test]
fn a_failing_request_reports_status_without_echoing_the_credential() {
    // Error text reaches logs, CI output, and crash reports. A 401 body is
    // attacker-influenced, so the check is that *our* token is absent, not
    // that the body is clean.
    let (base, _rx) = serve(1, |_| {
        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 21\r\n\r\n{\"error\":\"forbidden\"}".to_vec()
    });

    let error = execute(&authed_get(&format!("{base}/index"), "super-secret-publish-token"))
        .expect_err("403 must be an error");
    let message = format!("{error:#}");
    assert!(
        !message.contains("super-secret-publish-token"),
        "the credential leaked into an error: {message}"
    );
    assert!(message.contains("403"), "the status should be reported: {message}");
}

#[test]
fn a_url_embedded_credential_is_absent_from_the_error_it_causes() {
    // LuaRocks and Packagist put the token in the request line, so the
    // ordinary "redact headers" reflex does not cover them.
    let (base, _rx) = serve(1, |_| b"HTTP/1.1 500 Server Error\r\nContent-Length: 0\r\n\r\n".to_vec());

    let mut request = get(&format!("{base}/api/1/super-secret-api-key/upload"));
    request.url_contains_secret = true;
    let error = execute(&request).expect_err("500 must be an error");

    let message = format!("{error:#}");
    assert!(
        !message.contains("super-secret-api-key"),
        "a URL-embedded credential leaked into an error: {message}"
    );
    assert!(message.contains("<redacted>"), "expected redaction marker: {message}");
}

#[test]
fn a_slow_registry_does_not_hang_a_release_forever() {
    // A release job that blocks indefinitely on one unresponsive mirror is a
    // stuck pipeline, not a failed one — and the second is far easier to
    // diagnose than the first.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        // Accept, then never answer.
        if let Ok((stream, _)) = listener.accept() {
            thread::sleep(std::time::Duration::from_secs(120));
            drop(stream);
        }
    });

    let started = std::time::Instant::now();
    let result = execute(&get(&format!("http://{addr}/index")));
    let waited = started.elapsed();

    assert!(result.is_err(), "an unanswered request must fail");
    assert!(
        waited < std::time::Duration::from_secs(90),
        "waited {waited:?} before giving up"
    );
}

#[test]
fn the_default_cap_is_generous_enough_for_a_real_index() {
    // The largest packuments in the wild are a few MiB; the cap exists to stop
    // an OOM, not to reject npm.
    let body = vec![b'y'; 3 * 1024 * 1024];
    let (base, _rx) = serve(1, move |_| {
        let mut out =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(&body);
        out
    });
    let served = execute(&get(&format!("{base}/index"))).expect("3 MiB is an ordinary index");
    assert_eq!(served.len(), 3 * 1024 * 1024);
}

#[test]
fn a_truncated_connection_is_an_error_not_a_short_read() {
    // Silently accepting a half-delivered index would read as "this package
    // has fewer versions than it does", which is a wrong answer rather than a
    // failure.
    let (base, _rx) = serve(1, |_| {
        // Promises 4096 bytes, sends 4.
        b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nshor".to_vec()
    });
    let result = execute(&get(&format!("{base}/index")));
    assert!(
        result.is_err(),
        "a body shorter than Content-Length must not be accepted as complete"
    );
}

fn _assert_read_is_used(_: &dyn Read) {}
