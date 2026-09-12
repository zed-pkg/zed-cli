use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use zed_cli::config::Config;
use zed_cli::install_graph::prefetch;
use zed_cli::pack::pack;
use zed_cli::store::Store;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::registry::{self, PackageMetadata, VersionMetadata};
use zed_interfaces::vcs::Vcs;

const ORG: &str = "recursive-concurrency";
const VERSION: &str = "1.0.0";
const PACKAGE_COUNT: usize = 12;
const EXPECTED_CONCURRENCY: usize = 5;

#[derive(Clone)]
struct Response {
    content_type: &'static str,
    body: Arc<Vec<u8>>,
    artifact: bool,
}

#[derive(Default)]
struct GateState {
    arrived: usize,
    open: bool,
    timed_out: bool,
}

struct ServerState {
    responses: HashMap<String, Response>,
    active_artifacts: AtomicUsize,
    max_active_artifacts: AtomicUsize,
    artifact_requests: AtomicUsize,
    handler_failed: AtomicBool,
    gate: Mutex<GateState>,
    gate_ready: Condvar,
}

struct RegistryServer {
    address: SocketAddr,
    state: Arc<ServerState>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl RegistryServer {
    fn start(listener: TcpListener, responses: HashMap<String, Response>) -> Result<Self> {
        let address = listener.local_addr()?;
        let state = Arc::new(ServerState {
            responses,
            active_artifacts: AtomicUsize::new(0),
            max_active_artifacts: AtomicUsize::new(0),
            artifact_requests: AtomicUsize::new(0),
            handler_failed: AtomicBool::new(false),
            gate: Mutex::new(GateState::default()),
            gate_ready: Condvar::new(),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_state = Arc::clone(&state);
        let accept_shutdown = Arc::clone(&shutdown);
        let accept_thread = thread::Builder::new()
            .name("recursive-registry".to_string())
            .spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    if accept_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    let handler_state = Arc::clone(&accept_state);
                    thread::spawn(move || {
                        if let Err(error) = handle_connection(stream, &handler_state) {
                            handler_state.handler_failed.store(true, Ordering::SeqCst);
                            eprintln!("test registry handler failed: {error:#}");
                        }
                    });
                }
            })
            .context("starting test registry accept loop")?;
        Ok(Self {
            address,
            state,
            shutdown,
            accept_thread: Some(accept_thread),
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn max_active_artifacts(&self) -> usize {
        self.state.max_active_artifacts.load(Ordering::SeqCst)
    }

    fn artifact_requests(&self) -> usize {
        self.state.artifact_requests.load(Ordering::SeqCst)
    }

    fn gate_timed_out(&self) -> bool {
        self.state
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .timed_out
    }
}

impl Drop for RegistryServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &ServerState) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .context("HTTP request has a path")?
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();

    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }

    let Some(response) = state.responses.get(&path) else {
        return write_response(&mut stream, "404 Not Found", "text/plain", b"not found\n");
    };

    if !response.artifact {
        return write_response(
            &mut stream,
            "200 OK",
            response.content_type,
            response.body.as_slice(),
        );
    }

    state.artifact_requests.fetch_add(1, Ordering::SeqCst);
    let active = state.active_artifacts.fetch_add(1, Ordering::SeqCst) + 1;
    state
        .max_active_artifacts
        .fetch_max(active, Ordering::SeqCst);

    let mut gate = state
        .gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gate.arrived += 1;
    if gate.arrived >= EXPECTED_CONCURRENCY {
        gate.open = true;
        state.gate_ready.notify_all();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gate.open {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            gate.timed_out = true;
            gate.open = true;
            state.gate_ready.notify_all();
            break;
        }
        let (next, timeout) = state
            .gate_ready
            .wait_timeout(gate, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gate = next;
        if timeout.timed_out() && !gate.open {
            gate.timed_out = true;
            gate.open = true;
            state.gate_ready.notify_all();
        }
    }
    drop(gate);

    // Keep the first wave alive briefly so an implementation that exceeds the
    // configured bound is visible in max_active_artifacts rather than hidden
    // by very small fixture archives.
    thread::sleep(Duration::from_millis(75));
    let result = write_response(
        &mut stream,
        "200 OK",
        response.content_type,
        response.body.as_slice(),
    );
    state.active_artifacts.fetch_sub(1, Ordering::SeqCst);
    result
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

struct PublishedPackage {
    name: String,
    package: PackageMetadata,
    version: VersionMetadata,
    artifact: Vec<u8>,
}

fn package_manifest(name: &str) -> String {
    format!(
        r#"[package]
org = "{ORG}"
name = "{name}"
version = "{VERSION}"
description = "bounded recursive install fixture"

[package.repository]
vcs = "git"
url = "https://example.invalid/{ORG}/{name}"
"#,
    )
}

fn build_package(scratch: &Path, name: &str) -> Result<PublishedPackage> {
    let source = scratch.join(format!("source-{name}"));
    fs::create_dir_all(&source)?;
    let manifest_text = package_manifest(name);
    fs::write(source.join(".zpkg.toml"), &manifest_text)?;
    fs::write(
        source.join("payload.txt"),
        format!("{ORG}/{name}@{VERSION}\n"),
    )?;
    let manifest = Manifest::parse(&manifest_text)?;
    let packed = pack(
        &source,
        &manifest,
        Some(&scratch.join(format!("packed-{name}"))),
    )?;
    let artifact = fs::read(&packed.path)?;
    Ok(PublishedPackage {
        name: name.to_string(),
        package: PackageMetadata {
            org: ORG.to_string(),
            name: name.to_string(),
            description: Some(format!("fixture {name}")),
            vcs: Vcs::Git,
            repo_url: format!("https://example.invalid/{ORG}/{name}"),
            version_scheme: manifest.package.version_scheme,
            latest: Some(VERSION.to_string()),
            tags: Vec::new(),
            versions: vec![VERSION.to_string()],
        },
        version: VersionMetadata {
            org: ORG.to_string(),
            name: name.to_string(),
            version: VERSION.to_string(),
            sha256: packed.sha256,
            size: packed.size,
            format: packed.format,
            vcs_tag: format!("v{VERSION}"),
            vcs_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            download_url: String::new(),
            published_at: "1970-01-01T00:00:00Z".to_string(),
            yanked: false,
            mirrors: Vec::new(),
        },
        artifact,
    })
}

fn registry_responses(
    address: SocketAddr,
    packages: &mut [PublishedPackage],
) -> Result<HashMap<String, Response>> {
    let mut responses = HashMap::new();
    for package in packages {
        let artifact_path = registry::artifact_path(&package.version.sha256);
        package.version.download_url = format!("http://{address}{artifact_path}");
        responses.insert(
            registry::package_path(ORG, &package.name),
            Response {
                content_type: "application/json",
                body: Arc::new(serde_json::to_vec(&package.package)?),
                artifact: false,
            },
        );
        responses.insert(
            registry::version_path(ORG, &package.name, VERSION),
            Response {
                content_type: "application/json",
                body: Arc::new(serde_json::to_vec(&package.version)?),
                artifact: false,
            },
        );
        responses.insert(
            artifact_path,
            Response {
                content_type: "application/octet-stream",
                body: Arc::new(package.artifact.clone()),
                artifact: true,
            },
        );
    }
    Ok(responses)
}

fn write_consumer(project: &Path, packages: &[PublishedPackage]) -> Result<()> {
    fs::create_dir_all(project)?;
    let mut manifest = String::from(
        r#"[package]
org = "recursive-consumer"
name = "wide-frontier"
version = "0.1.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/recursive-consumer/wide-frontier"

[dependencies]
"#,
    );
    for package in packages {
        manifest.push_str(&format!("\"{ORG}/{}\" = \"={VERSION}\"\n", package.name));
    }
    fs::write(project.join(".zpkg.toml"), manifest)?;
    Ok(())
}

fn test_config(registry: &str, home: PathBuf) -> Config {
    Config {
        registry: registry.to_string(),
        home,
        token: None,
        auth_url: "http://127.0.0.1/unused".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
    }
}

#[test]
fn recursive_http_prefetch_saturates_at_five_and_warm_runs_do_not_redownload() -> Result<()> {
    if let Ok(raw) = std::env::var("ZED_PKG_INSTALL_CONCURRENCY") {
        assert_eq!(
            raw, "5",
            "this contract test expects the default five-worker setting or an explicit value of 5"
        );
    }

    let temp = tempfile::tempdir()?;
    let scratch = temp.path().join("scratch");
    let project = temp.path().join("project");
    let home = temp.path().join("home");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;

    let mut packages = (0..PACKAGE_COUNT)
        .map(|index| build_package(&scratch, &format!("pkg-{index:02}")))
        .collect::<Result<Vec<_>>>()?;
    write_consumer(&project, &packages)?;
    let server = RegistryServer::start(listener, registry_responses(address, &mut packages)?)?;
    let config = test_config(&server.base_url(), home.clone());

    let cold = prefetch(&project, &config, false)?;
    assert_eq!(cold.resolved, PACKAGE_COUNT);
    assert_eq!(cold.downloaded, PACKAGE_COUNT);
    assert_eq!(server.artifact_requests(), PACKAGE_COUNT);
    assert_eq!(server.max_active_artifacts(), EXPECTED_CONCURRENCY);
    assert!(
        !server.gate_timed_out(),
        "five artifact handlers never became active together"
    );
    assert!(
        !server.state.handler_failed.load(Ordering::SeqCst),
        "the fixture HTTP registry reported a handler failure"
    );

    let warm = prefetch(&project, &config, false)?;
    assert_eq!(warm.resolved, PACKAGE_COUNT);
    assert_eq!(warm.downloaded, 0);
    assert_eq!(
        server.artifact_requests(),
        PACKAGE_COUNT,
        "a warm content-addressed store must not fetch artifact bytes again"
    );

    let store = Store::new(&home);
    for package in &packages {
        assert!(
            store.has(&package.version.sha256),
            "missing store entry for {}/{}",
            package.version.org,
            package.version.name
        );
    }

    let artifact_locks = fs::read_dir(home.join("locks"))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("artifact-"))
        .count();
    assert_eq!(artifact_locks, PACKAGE_COUNT);

    let cache_entries = fs::read_dir(home.join("cache"))?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(cache_entries.len(), PACKAGE_COUNT);
    assert!(
        cache_entries
            .iter()
            .all(|entry| entry.file_type().is_ok_and(|kind| kind.is_file())),
        "atomic download staging directories must be removed"
    );
    Ok(())
}
