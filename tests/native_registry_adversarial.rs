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

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use zed_cli::native_host_client::{
    HeaderValue, Method, RegistryLimits, RegistryRequest, RequestBody, execute, execute_with_limits,
};

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

    let result = execute_with_limits(
        &get(&format!("{base}/big")),
        RegistryLimits {
            max_bytes: 1024 * 1024,
            ..RegistryLimits::default()
        },
    );

    // Deliberately not `expect_err`: on failure it would dump the entire
    // oversized body into the test output.
    let message = match result {
        Ok(body) => panic!("4 MiB was buffered whole ({} bytes read)", body.len()),
        Err(error) => format!("{error:#}"),
    };
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
    let (victim, victim_rx) = serve(1, |_| {
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()
    });
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

    let error = execute(&authed_get(
        &format!("{base}/index"),
        "super-secret-publish-token",
    ))
    .expect_err("403 must be an error");
    let message = format!("{error:#}");
    assert!(
        !message.contains("super-secret-publish-token"),
        "the credential leaked into an error: {message}"
    );
    assert!(
        message.contains("403"),
        "the status should be reported: {message}"
    );
}

#[test]
fn a_url_embedded_credential_is_absent_from_the_error_it_causes() {
    // LuaRocks and Packagist put the token in the request line, so the
    // ordinary "redact headers" reflex does not cover them.
    let (base, _rx) = serve(1, |_| {
        b"HTTP/1.1 500 Server Error\r\nContent-Length: 0\r\n\r\n".to_vec()
    });

    let mut request = get(&format!("{base}/api/1/super-secret-api-key/upload"));
    request.url_contains_secret = true;
    let error = execute(&request).expect_err("500 must be an error");

    let message = format!("{error:#}");
    assert!(
        !message.contains("super-secret-api-key"),
        "a URL-embedded credential leaked into an error: {message}"
    );
    assert!(
        message.contains("<redacted>"),
        "expected redaction marker: {message}"
    );
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
    let result = execute_with_limits(
        &get(&format!("http://{addr}/index")),
        RegistryLimits {
            timeout: std::time::Duration::from_secs(2),
            ..RegistryLimits::default()
        },
    );
    let waited = started.elapsed();

    assert!(result.is_err(), "an unanswered request must fail");
    assert!(
        waited < std::time::Duration::from_secs(30),
        "the timeout was not honoured; waited {waited:?}"
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

// ---------------------------------------------------------------------------
// Multi-step publish sequences.
//
// Driven through a recording closure rather than a live registry. What is worth
// verifying about a three-step publish is the ordering and the plumbing —
// which URL each step goes to, and which of them may see the credential — and a
// live-credential test would obscure exactly that.
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use zed_cli::native_host_client::{
    PublishPacing, RegistryResponse, publish_sequence, publish_sequence_paced,
};
use zed_interfaces::native_host::{NativeHost, ReleaseChannel};

fn route(host: NativeHost) -> zed_interfaces::native_host::ChannelRoute {
    host.channel_route("2.0.0", ReleaseChannel::Stable, 1)
        .expect("stable route")
}

/// One recorded request: where it went, and whether it carried a credential.
#[derive(Debug, Clone)]
struct Sent {
    url: String,
    authorization: Option<String>,
}

/// Requests recorded by [`recorder`], shared with the caller.
type SentLog = std::rc::Rc<RefCell<Vec<Sent>>>;
/// The scripted `send` closure. Boxed so it has a nameable type; the sequence
/// driver takes `&mut dyn FnMut`, so the indirection costs nothing.
type Sender = Box<dyn FnMut(&RegistryRequest) -> anyhow::Result<RegistryResponse>>;

fn recorder(replies: Vec<RegistryResponse>) -> (Sender, SentLog) {
    let log = std::rc::Rc::new(RefCell::new(Vec::new()));
    let sink = log.clone();
    let mut queue = replies.into_iter();
    let send = move |request: &RegistryRequest| {
        sink.borrow_mut().push(Sent {
            url: request.url.clone(),
            authorization: request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.expose().to_string()),
        });
        queue
            .next()
            .ok_or_else(|| anyhow::anyhow!("no scripted reply left"))
    };
    (Box::new(send), log)
}

fn reply(status: u16, body: &str, location: Option<&str>) -> RegistryResponse {
    RegistryResponse {
        status,
        location: location.map(str::to_string),
        body: body.to_string(),
    }
}

#[test]
fn pub_dev_takes_three_steps_and_never_shows_storage_the_token() {
    // Step 1 hands back a signed form for object storage. That form *is* the
    // authorization for step 2, so replaying the pub.dev token there would
    // hand it to whatever origin the grant names — a third party.
    let (mut send, log) = recorder(vec![
        reply(
            200,
            r#"{"url":"https://storage.example/upload","fields":{"key":"pkg/2.0.0.tar.gz","policy":"abc"}}"#,
            None,
        ),
        reply(
            204,
            "",
            Some("https://pub.dev/api/packages/versions/newUploadFinish"),
        ),
        reply(200, r#"{"success":{"message":"published"}}"#, None),
    ]);

    let steps = publish_sequence(
        &route(NativeHost::PubDev),
        "acme_client",
        std::path::Path::new("acme.tar.gz"),
        Some("pub-secret"),
        &mut send,
    )
    .expect("a scripted happy path publishes");

    assert_eq!(steps.len(), 3, "{steps:?}");
    assert_eq!(steps[0].description, "request a signed upload form");
    assert_eq!(steps[2].description, "finalize the version");

    let sent = log.borrow();
    assert_eq!(sent.len(), 3);
    assert!(sent[0].url.ends_with("/packages/versions/new"));
    assert_eq!(sent[1].url, "https://storage.example/upload");
    assert_eq!(
        sent[2].url,
        "https://pub.dev/api/packages/versions/newUploadFinish"
    );

    assert!(
        sent[0].authorization.is_some(),
        "pub.dev itself is authenticated"
    );
    assert!(
        sent[1].authorization.is_none(),
        "the token must not be replayed to storage: {:?}",
        sent[1].authorization
    );
    assert!(sent[2].authorization.is_some(), "finalize is authenticated");
}

#[test]
fn pub_dev_stops_with_a_named_reason_when_the_grant_is_malformed() {
    // A registry that answers 200 with the wrong shape must not be treated as
    // a successful publish.
    let (mut send, log) = recorder(vec![reply(200, r#"{"unexpected":true}"#, None)]);
    let error = publish_sequence(
        &route(NativeHost::PubDev),
        "acme_client",
        std::path::Path::new("acme.tar.gz"),
        Some("pub-secret"),
        &mut send,
    )
    .expect_err("a grant with no url cannot be uploaded to");
    assert!(format!("{error:#}").contains("`url`"), "{error:#}");
    assert_eq!(log.borrow().len(), 1, "it must not proceed to upload");
}

#[test]
fn pub_dev_without_a_finalize_location_does_not_claim_success() {
    // A 204 with no `Location` means the upload landed but the version was
    // never finalized. Returning Ok there would report a publish that did not
    // happen.
    let (mut send, _log) = recorder(vec![
        reply(
            200,
            r#"{"url":"https://storage.example/u","fields":{}}"#,
            None,
        ),
        reply(204, "", None),
    ]);
    let error = publish_sequence(
        &route(NativeHost::PubDev),
        "acme_client",
        std::path::Path::new("acme.tar.gz"),
        Some("pub-secret"),
        &mut send,
    )
    .expect_err("no Location means nothing was finalized");
    assert!(format!("{error:#}").contains("Location"), "{error:#}");
}

#[test]
fn maven_portal_polls_until_the_deployment_settles() {
    // A 201 from the upload means "accepted for validation", not "published".
    // Reporting success there would tell a release job a version exists that
    // Central may reject minutes later.
    let (mut send, log) = recorder(vec![
        reply(201, "\"deploy-123\"", None),
        reply(200, r#"{"deploymentState":"VALIDATING"}"#, None),
        reply(200, r#"{"deploymentState":"PUBLISHED"}"#, None),
    ]);

    let steps = publish_sequence_paced(
        &route(NativeHost::MavenCentral),
        "com.acme:client",
        std::path::Path::new("bundle.zip"),
        Some("portal-secret"),
        // Drive the state machine, not the clock.
        PublishPacing {
            poll_interval: std::time::Duration::ZERO,
            ..PublishPacing::default()
        },
        &mut send,
    )
    .expect("a settled deployment publishes");

    assert_eq!(steps.len(), 3);
    assert!(steps[1].description.contains("VALIDATING"), "{steps:?}");
    assert!(steps[2].description.contains("PUBLISHED"), "{steps:?}");

    let sent = log.borrow();
    assert!(sent[0].url.ends_with("/upload"));
    assert!(
        sent[1].url.contains("status?id=deploy-123"),
        "{}",
        sent[1].url
    );
    // The Portal wants `UserToken`, not `Bearer` — it rejects the latter.
    assert!(
        sent[0]
            .authorization
            .as_deref()
            .unwrap()
            .starts_with("UserToken "),
        "{:?}",
        sent[0].authorization
    );
}

#[test]
fn maven_portal_reports_a_rejected_deployment_as_a_failure() {
    let (mut send, _log) = recorder(vec![
        reply(201, "deploy-456", None),
        reply(200, r#"{"deploymentState":"FAILED"}"#, None),
    ]);
    let error = publish_sequence(
        &route(NativeHost::MavenCentral),
        "com.acme:client",
        std::path::Path::new("bundle.zip"),
        Some("portal-secret"),
        &mut send,
    )
    .expect_err("a FAILED deployment is not a publish");
    assert!(format!("{error:#}").contains("FAILED"), "{error:#}");
}

#[test]
fn a_missing_credential_stops_a_multi_step_publish_before_the_first_request() {
    // Discovering this after step 1 would leave a half-run release.
    for host in [NativeHost::PubDev, NativeHost::MavenCentral] {
        let (mut send, log) = recorder(Vec::new());
        let error = publish_sequence(
            &route(host),
            "acme",
            std::path::Path::new("a.tar.gz"),
            None,
            &mut send,
        )
        .expect_err("no credential must fail");
        assert!(
            format!("{error:#}").contains("needs a credential"),
            "{error:#}"
        );
        assert!(log.borrow().is_empty(), "{host} sent a request anyway");
    }
}

#[test]
fn a_single_step_host_still_reports_one_step() {
    // The common path stays one request; the sequence driver must not add
    // ceremony for the 27 hosts that do not need it.
    let (mut send, log) = recorder(vec![reply(200, "{}", None)]);
    let steps = publish_sequence(
        &route(NativeHost::Npm),
        "@acme/client",
        std::path::Path::new("acme.tgz"),
        Some("npm-secret"),
        &mut send,
    )
    .expect("npm publishes in one request");
    assert_eq!(steps.len(), 1);
    assert!(
        steps[0]
            .description
            .starts_with("PUT https://registry.npmjs.org/")
    );
    assert_eq!(log.borrow().len(), 1);
}
