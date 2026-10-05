//! Fixed, loopback-only HTTP ingress probes for the three WASM adapters.

use serde_json::{Value, json};
use std::env;
use std::fs::{self, File};
use std::io::Seek as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};

const RESPONSE_LIMIT: usize = 1024 * 1024;
const HEADER_LIMIT: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
type Cancelled = Arc<AtomicBool>;

struct Options {
    platforms: Vec<&'static str>,
    skip_build: bool,
    readiness_timeout: Duration,
    record_observations: bool,
    root: PathBuf,
}

impl Options {
    fn parse() -> io::Result<Self> {
        let mut args = env::args().skip(1);
        let first = args.next();
        let (root, platform) = if first.as_deref() == Some("--repo-root") {
            let root = PathBuf::from(
                args.next()
                    .ok_or_else(|| io::Error::other("missing repository root"))?,
            )
            .canonicalize()?;
            (root, args.next())
        } else {
            (
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../..")
                    .canonicalize()?,
                first,
            )
        };
        let platforms = match platform.as_deref() {
            Some("all") => vec!["fastly", "cloudflare", "spin"],
            Some("fastly") => vec!["fastly"],
            Some("cloudflare") => vec!["cloudflare"],
            Some("spin") => vec!["spin"],
            _ => {
                return Err(io::Error::other(
                    "usage: request-fidelity-runner <all|fastly|cloudflare|spin> [--skip-build] [--readiness-timeout SECONDS] [--record-observations]",
                ));
            }
        };
        let mut options = Self {
            platforms,
            skip_build: false,
            readiness_timeout: Duration::from_secs(40),
            record_observations: false,
            root,
        };
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--repo-root" => {
                    options.root = PathBuf::from(
                        args.next()
                            .ok_or_else(|| io::Error::other("missing repository root"))?,
                    )
                    .canonicalize()?;
                }
                "--skip-build" => options.skip_build = true,
                "--record-observations" => options.record_observations = true,
                "--readiness-timeout" => {
                    let seconds = args
                        .next()
                        .ok_or_else(|| io::Error::other("missing readiness timeout"))?
                        .parse::<f64>()
                        .map_err(io::Error::other)?;
                    options.readiness_timeout =
                        Duration::try_from_secs_f64(seconds).map_err(io::Error::other)?;
                    if options.readiness_timeout.is_zero() {
                        return Err(io::Error::other("readiness timeout must be positive"));
                    }
                }
                _ => return Err(io::Error::other(format!("unknown argument: {argument}"))),
            }
        }
        Ok(options)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _result = writeln!(io::stderr(), "request fidelity: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute() -> io::Result<()> {
    let options = Options::parse()?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut worker = tokio::task::spawn_blocking(move || run(options, &worker_cancelled));
    tokio::select! {
        result = &mut worker => result.map_err(io::Error::other)?,
        _ = interrupt.recv() => {
            cancelled.store(true, Ordering::SeqCst);
            let _result = worker.await;
            Err(io::Error::new(io::ErrorKind::Interrupted, "SIGINT: owned children stopped"))
        }
        _ = terminate.recv() => {
            cancelled.store(true, Ordering::SeqCst);
            let _result = worker.await;
            Err(io::Error::new(io::ErrorKind::Interrupted, "SIGTERM: owned children stopped"))
        }
        _ = hangup.recv() => {
            cancelled.store(true, Ordering::SeqCst);
            let _result = worker.await;
            Err(io::Error::new(io::ErrorKind::Interrupted, "SIGHUP: owned children stopped"))
        }
    }
}

fn check_cancelled(cancelled: &Cancelled) -> io::Result<()> {
    if cancelled.load(Ordering::SeqCst) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"))
    } else {
        Ok(())
    }
}

struct OwnedProcess {
    child: Child,
    group: u32,
}

impl OwnedProcess {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        command.process_group(0);
        let child = command.spawn()?;
        let group = child.id();
        Ok(Self { child, group })
    }

    fn wait(&mut self, cancelled: &Cancelled) -> io::Result<()> {
        loop {
            check_cancelled(cancelled)?;
            if let Some(status) = self.child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "fixture command exits with {status}"
                    )))
                };
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

fn signal_group(group: u32, name: &str) {
    let _result = Command::new("/bin/kill")
        .args([name, "--", &format!("-{group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // Signal the owned group even if its direct wrapper already exited.
        signal_group(self.group, "-TERM");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.child.try_wait().is_ok_and(|status| status.is_some())
                || Instant::now() >= deadline
            {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
        signal_group(self.group, "-KILL");
        let _result = self.child.wait();
    }
}

fn command(fixture: &Path, program: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(fixture)
        .env("WRANGLER_SEND_METRICS", "false");
    command
}

fn run_command(
    fixture: &Path,
    program: &str,
    arguments: &[&str],
    cancelled: &Cancelled,
    capture: bool,
) -> io::Result<String> {
    check_cancelled(cancelled)?;
    let mut command = command(fixture, program, arguments);
    // A file avoids pipe backpressure while waiting interruptibly for build tools.
    let mut output = tempfile::tempfile()?;
    command.stdout(output.try_clone()?);
    command.stderr(Stdio::inherit());
    let result = OwnedProcess::spawn(&mut command)?.wait(cancelled);
    output.rewind()?;
    if !capture {
        io::copy(&mut output, &mut io::stderr())?;
    }
    result?;
    if capture {
        let mut value = String::new();
        output.read_to_string(&mut value)?;
        Ok(value)
    } else {
        Ok(String::new())
    }
}

fn verify_version(
    fixture: &Path,
    program: &str,
    expected: &str,
    cancelled: &Cancelled,
) -> io::Result<()> {
    let output = run_command(fixture, program, &["--version"], cancelled, true)?;
    let actual = if program == "worker-build" {
        output.trim()
    } else {
        output.split_whitespace().nth(1).unwrap_or("")
    };
    if actual != expected {
        return Err(io::Error::other(format!(
            "{program} requires version {expected}"
        )));
    }
    Ok(())
}

fn build(platform: &str, fixture: &Path, cancelled: &Cancelled) -> io::Result<()> {
    run_command(
        fixture,
        "cargo",
        &["metadata", "--locked", "--format-version", "1", "--no-deps"],
        cancelled,
        true,
    )?;
    if platform == "cloudflare" {
        verify_version(fixture, "worker-build", "0.8.3", cancelled)?;
        run_command(fixture, "npm", &["ci"], cancelled, false)?;
        let lock = fs::read(fixture.join("Cargo.lock"))?;
        run_command(
            fixture,
            "worker-build",
            &[
                ".",
                "--release",
                "--no-opt",
                "--out-dir",
                "build/cloudflare",
                "--",
                "--features",
                "cloudflare",
                "--locked",
                "--target-dir",
                "target/cloudflare",
            ],
            cancelled,
            false,
        )?;
        if fs::read(fixture.join("Cargo.lock"))? != lock {
            return Err(io::Error::other(
                "worker-build changes the locked fixture dependencies",
            ));
        }
    } else {
        let mut arguments = vec![
            "build",
            "--locked",
            "--release",
            "--features",
            platform,
            "--target-dir",
        ];
        arguments.extend(if platform == "fastly" {
            [
                "target/fastly",
                "--target",
                "wasm32-wasip1",
                "--bin",
                "request-fidelity-fastly",
            ]
            .as_slice()
        } else {
            ["target/spin", "--target", "wasm32-wasip2", "--lib"].as_slice()
        });
        run_command(fixture, "cargo", &arguments, cancelled, false)?;
    }
    Ok(())
}

fn run(options: Options, cancelled: &Cancelled) -> io::Result<()> {
    let fixture = options.root.join("tests/fixtures/request-fidelity");
    let mut observations = json!({});
    for platform in &options.platforms {
        check_cancelled(cancelled)?;
        if *platform == "spin" {
            let manifest: toml::Value =
                toml::from_str(&fs::read_to_string(fixture.join("Cargo.toml"))?)
                    .map_err(io::Error::other)?;
            let pin = manifest["package"]["metadata"]["request-fidelity"]["spin-cli"]
                .as_str()
                .ok_or_else(|| io::Error::other("missing Spin runtime pin"))?;
            verify_version(&fixture, "spin", pin, cancelled)?;
        } else if *platform == "fastly" {
            let versions = fs::read_to_string(options.root.join(".tool-versions"))?;
            let pin = versions
                .lines()
                .find_map(|line| line.strip_prefix("viceroy "))
                .ok_or_else(|| io::Error::other("missing Viceroy pin"))?;
            verify_version(&fixture, "viceroy", pin.trim(), cancelled)?;
        }
        if !options.skip_build {
            build(platform, &fixture, cancelled)?;
        }
        let scratch = tempfile::tempdir()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        writeln!(io::stderr(), "{platform} loopback fixture uses port {port}")?;
        let address = format!("127.0.0.1:{port}");
        let port_string = port.to_string();
        let scratch_string = scratch.path().to_string_lossy();
        let log_dir = scratch.path().join("logs");
        let log_dir_string = log_dir.to_string_lossy();
        let mut runtime = match *platform {
            "fastly" => command(
                &fixture,
                "viceroy",
                &[
                    "serve",
                    "-C",
                    "fastly.toml",
                    "--addr",
                    &address,
                    "target/fastly/wasm32-wasip1/release/request-fidelity-fastly.wasm",
                ],
            ),
            "spin" => command(
                &fixture,
                "spin",
                &[
                    "up",
                    "--from",
                    "spin.toml",
                    "--listen",
                    &address,
                    "--state-dir",
                    &scratch_string,
                    "--log-dir",
                    &log_dir_string,
                ],
            ),
            _ => command(
                &fixture,
                "node_modules/.bin/wrangler",
                &[
                    "dev",
                    "--local",
                    "--ip",
                    "127.0.0.1",
                    "--port",
                    &port_string,
                ],
            ),
        };
        let log_path = scratch.path().join("runtime.log");
        let log = File::create(&log_path)?;
        runtime.stdout(log.try_clone()?).stderr(log);
        let mut process = OwnedProcess::spawn(&mut runtime)?;
        let result = (|| {
            let deadline = Instant::now() + options.readiness_timeout;
            loop {
                check_cancelled(cancelled)?;
                if process.child.try_wait()?.is_some() {
                    return Err(io::Error::other("runtime exits before readiness"));
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::other("runtime readiness deadline exceeded"));
                }
                if exchange(port, &ready_case(), cancelled, deadline)
                    .is_ok_and(|(status, _)| status == 200)
                {
                    break;
                }
                thread::sleep(POLL_INTERVAL);
            }
            let observed = probes(platform, port, cancelled)?;
            if !options.record_observations {
                let expected: Value =
                    serde_json::from_slice(&fs::read(fixture.join("observations.json"))?)
                        .map_err(io::Error::other)?;
                if expected.get(*platform) != Some(&observed) {
                    return Err(io::Error::other(format!(
                        "{platform}: full runtime observations changed; inspect before updating baseline"
                    )));
                }
            }
            Ok(observed)
        })();
        drop(process);
        match result {
            Ok(observed) => observations[*platform] = observed,
            Err(error) => {
                if let Ok(log) = fs::read_to_string(&log_path) {
                    writeln!(io::stderr(), "{log}")?;
                }
                return Err(error);
            }
        }
    }
    serde_json::to_writer_pretty(io::stdout(), &observations).map_err(io::Error::other)?;
    writeln!(io::stdout())
}

fn request_packet(case: &Case) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(case.method);
    packet.push(b' ');
    packet.extend_from_slice(case.target);
    packet.extend_from_slice(b" HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\nX-Fixture: ");
    packet.extend_from_slice(case.name.as_bytes());
    packet.extend_from_slice(b"\r\n");
    for (name, value) in &case.headers {
        packet.extend_from_slice(name);
        packet.extend_from_slice(b": ");
        packet.extend_from_slice(value);
        packet.extend_from_slice(b"\r\n");
    }
    packet.extend_from_slice(b"\r\n");
    packet.extend_from_slice(case.body);
    packet
}

struct TimedSocket<'a> {
    socket: TcpStream,
    cancelled: &'a Cancelled,
    deadline: Instant,
}

impl Read for TimedSocket<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            // Buffered/exact reads retry Interrupted, so cancellation must be terminal.
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(io::Error::other("cancelled"));
            }
            if Instant::now() >= self.deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "HTTP response deadline exceeded",
                ));
            }
            match self.socket.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }
}

fn exchange(
    port: u16,
    case: &Case,
    cancelled: &Cancelled,
    deadline: Instant,
) -> io::Result<(u16, Value)> {
    check_cancelled(cancelled)?;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(5));
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "HTTP connection deadline exceeded",
        ));
    }
    let mut socket = TcpStream::connect_timeout(&address, timeout)?;
    socket.set_write_timeout(Some(timeout))?;
    socket.set_read_timeout(Some(POLL_INTERVAL))?;
    socket.write_all(&request_packet(case))?;
    let mut reader = BufReader::new(TimedSocket {
        socket,
        cancelled,
        deadline,
    });
    let (status, payload) = read_response(&mut reader)?;
    Ok((
        status,
        serde_json::from_slice(&payload).unwrap_or(Value::Null),
    ))
}

fn http_line(reader: &mut impl BufRead, remaining: &mut usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(*remaining).map_err(io::Error::other)?)
        .read_until(b'\n', &mut bytes)?;
    *remaining -= bytes.len();
    if !bytes.ends_with(b"\r\n") {
        return Err(io::Error::other("missing CRLF or oversized HTTP header"));
    }
    bytes.truncate(bytes.len() - 2);
    String::from_utf8(bytes).map_err(io::Error::other)
}

fn read_response(reader: &mut impl BufRead) -> io::Result<(u16, Vec<u8>)> {
    let mut remaining = HEADER_LIMIT;
    loop {
        let status_line = http_line(reader, &mut remaining)?;
        let mut fields = status_line.split_whitespace();
        if !matches!(fields.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
            return Err(io::Error::other("invalid HTTP response status line"));
        }
        let status = fields
            .next()
            .ok_or_else(|| io::Error::other("missing response status"))?
            .parse::<u16>()
            .map_err(io::Error::other)?;
        let mut length = None;
        let mut chunked = false;
        loop {
            let line = http_line(reader, &mut remaining)?;
            if line.is_empty() {
                break;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| io::Error::other("invalid HTTP response header"))?;
            if name.eq_ignore_ascii_case("content-length") {
                let next = value.trim().parse::<usize>().map_err(io::Error::other)?;
                if length.is_some_and(|previous| previous != next) {
                    return Err(io::Error::other("conflicting response lengths"));
                }
                length = Some(next);
            }
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value
                    .split(',')
                    .any(|item| item.trim().eq_ignore_ascii_case("chunked"));
            }
        }
        if (100..200).contains(&status) {
            continue;
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                let line = http_line(reader, &mut remaining)?;
                let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                    .map_err(io::Error::other)?;
                if size == 0 {
                    while !http_line(reader, &mut remaining)?.is_empty() {}
                    break;
                }
                append_body(reader, &mut body, size)?;
                let mut delimiter = [0; 2];
                reader.read_exact(&mut delimiter)?;
                if delimiter != *b"\r\n" {
                    return Err(io::Error::other("invalid chunk delimiter"));
                }
            }
        } else if let Some(length) = length {
            append_body(reader, &mut body, length)?;
        } else {
            reader
                .take(u64::try_from(RESPONSE_LIMIT + 1).map_err(io::Error::other)?)
                .read_to_end(&mut body)?;
            if body.len() > RESPONSE_LIMIT {
                return Err(io::Error::other("oversized HTTP response body"));
            }
        }
        return Ok((status, body));
    }
}

fn append_body(reader: &mut impl Read, body: &mut Vec<u8>, length: usize) -> io::Result<()> {
    let end = body
        .len()
        .checked_add(length)
        .filter(|end| *end <= RESPONSE_LIMIT)
        .ok_or_else(|| io::Error::other("oversized HTTP response body"))?;
    let start = body.len();
    body.resize(end, 0);
    reader.read_exact(&mut body[start..])
}

fn ready_case() -> Case {
    Case {
        name: "ready",
        method: b"GET",
        target: b"/reserved",
        headers: Vec::new(),
        body: b"",
    }
}

fn cases() -> Vec<Case> {
    let mut cases: Vec<_> = [
        "extension",
        "dot",
        "encoded-dot",
        "slashes",
        "absolute",
        "high-byte",
        "utf8",
        "unicode",
        "repeat",
        "compound-invalid",
        "compound-valid",
        "spoof",
        "length-identical",
        "length-conflict",
        "te-cl",
        "chunked",
        "continue",
    ]
    .into_iter()
    .map(|name| Case {
        name,
        ..ready_case()
    })
    .collect();
    for case in &mut cases {
        match case.name {
            "extension" => case.method = b"EXAMPLE-METHOD",
            "dot" => case.target = b"/reserved/../ordinary",
            "encoded-dot" => case.target = b"/reserved/%2e%2e/ordinary",
            "slashes" => case.target = b"/reserved//%2F?q=a%2Fb",
            "absolute" => case.target = b"http://example.com/reserved?fixed=1",
            "high-byte" => case.headers = vec![(b"Cookie", b"a=\xff")],
            "utf8" => case.headers = vec![(b"Cookie", b"a=\xc3\xa9")],
            "unicode" => case.headers = vec![(b"Cookie", b"a=\xc4\x80")],
            "repeat" => {
                case.headers = vec![
                    (b"Cookie", b"a=1"),
                    (b"Origin", b"https://example.com"),
                    (b"X-Control", b"first"),
                    (b"COOKIE", b"b=2"),
                    (b"Origin", b"https://other.example.com"),
                    (b"x-control", b"second"),
                ]
            }
            "compound-invalid" | "compound-valid" => {
                case.headers = vec![
                    (b"Cookie", b"diagnostics=1"),
                    (
                        b"Cookie",
                        if case.name == "compound-invalid" {
                            b"a=\xff"
                        } else {
                            b"a=\xef\xbf\xbd"
                        },
                    ),
                ];
            }
            "spoof" => {
                case.headers = vec![
                    (b"Origin", b"https://spoof.example.com"),
                    (b"Forwarded", b"proto=https;host=spoof.example.com"),
                    (b"spin-full-url", b"https://spoof.example.com/reserved"),
                    (b"spin-client-addr", b"192.0.2.1:1"),
                ]
            }
            "length-identical" | "length-conflict" => {
                case.method = b"POST";
                case.headers = vec![
                    (b"Content-Length", b"0"),
                    (
                        b"Content-Length",
                        if case.name == "length-identical" {
                            b"0"
                        } else {
                            b"1"
                        },
                    ),
                ];
            }
            "te-cl" | "chunked" => {
                case.method = b"POST";
                case.headers = vec![(b"Transfer-Encoding", b"chunked")];
                if case.name == "te-cl" {
                    case.headers.push((b"Content-Length", b"0"));
                }
                case.body = b"0\r\n\r\n";
            }
            "continue" => {
                case.method = b"POST";
                case.target = b"/missing";
            }
            _ => return Vec::new(),
        }
    }
    cases
}

fn require(condition: bool, platform: &str, case: &str, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(format!("{platform}/{case}: {message}")))
    }
}

fn verify_observation(platform: &str, case: &str, status: u16, verdict: &Value) -> io::Result<()> {
    if status == 200 {
        if case == "continue" {
            return require(
                *verdict == json!({"ordinary": true}),
                platform,
                case,
                "continuation must run ordinary handler",
            );
        }
        require(
            verdict["hook"] == true && verdict["ordinary"] == false,
            platform,
            case,
            "hook must intercept before ordinary lifecycle",
        )?;
        if platform == "fastly" {
            require(
                verdict["native_client_preserved"] == true,
                platform,
                case,
                "native provenance lost",
            )?;
        }
        require(
            verdict["global_order_unavailable"] == true
                && verdict["spoofed_origin_ignored"] == true
                && verdict["origin_available"] == true
                && verdict["origin_matches_expected"] == true,
            platform,
            case,
            "unavailable order and origin provenance must remain explicit",
        )?;
        require(
            verdict["target_available"] == (platform != "fastly"),
            platform,
            case,
            "unexpected target availability",
        )?;
        if platform != "fastly" {
            require(
                verdict["target_matches_runtime"] == true,
                platform,
                case,
                "target must match runtime",
            )?;
        }
        match case {
            "extension" => require(
                verdict["extension_method"] == true,
                platform,
                case,
                "extension token lost",
            )?,
            "high-byte" if platform == "cloudflare" => {
                require(
                    verdict["cookie_replacement"] == true && verdict["cookie_octets"] == false,
                    platform,
                    case,
                    "expected documented UTF-8 replacement, not original FF preservation",
                )?;
                writeln!(
                    io::stderr(),
                    "UNSUPPORTED local workerd replaces invalid UTF-8 FF before Rust; hook sees UTF-8 replacement bytes."
                )?;
            }
            "high-byte" => require(
                verdict["cookie_octets"] == true,
                platform,
                case,
                "original cookie FF lost",
            )?,
            "utf8" => require(
                verdict["cookie_utf8"] == true && verdict["cookie_latin1"] == false,
                platform,
                case,
                "valid UTF-8 C3 A9 must remain exact",
            )?,
            "unicode" => require(
                verdict["cookie_unicode"] == true,
                platform,
                case,
                "valid UTF-8 C4 80 must remain exact",
            )?,
            "repeat" if platform != "cloudflare" => require(
                verdict["cookie_count"] == 2
                    && verdict["same_name_order"] == true
                    && verdict["origin_count"] == 2
                    && verdict["control_count"] == 2,
                platform,
                case,
                "ordinary repeated fields or same-name order lost",
            )?,
            "repeat" => require(
                verdict["cookie_count"] == 1
                    && verdict["cookie_repeat_comma_joined"] == true
                    && verdict["cookie_repeat_semicolon_joined"] == false,
                platform,
                case,
                "must establish exact comma joining rather than infer cookie semantics from counts",
            )?,
            "compound-invalid" | "compound-valid" if platform == "cloudflare" => require(
                verdict["cookie_count"] == 1 && verdict["cookie_compound_comma_joined"] == true,
                platform,
                case,
                "invalid FF and original UTF-8 replacement must have the same documented runtime representation",
            )?,
            "compound-invalid" => require(
                verdict["cookie_count"] == 2 && verdict["cookie_compound_original"] == true,
                platform,
                case,
                "must retain valid-looking diagnostics alongside unrelated original FF",
            )?,
            "compound-valid" => require(
                verdict["cookie_count"] == 2 && verdict["cookie_compound_runtime_utf8"] == true,
                platform,
                case,
                "must retain the client's original valid UTF-8 replacement bytes",
            )?,
            "te-cl" | "chunked" => {
                let count = verdict["transfer_encoding_count"].as_u64();
                let chunked = verdict["transfer_encoding_chunked"].as_bool();
                require(
                    matches!(count, Some(0 | 1)) && chunked == count.map(|count| count == 1),
                    platform,
                    case,
                    "must record whether chunked Transfer-Encoding survives conversion",
                )?;
            }
            _ => {}
        }
        return Ok(());
    }
    match (platform, case, status) {
        ("cloudflare", "length-identical" | "length-conflict", 500) => writeln!(
            io::stderr(),
            "UNSUPPORTED local Wrangler duplicate Content-Length ingress (500); no hook verdict."
        )?,
        ("cloudflare", "absolute", 500) => writeln!(
            io::stderr(),
            "UNSUPPORTED local Wrangler absolute-form ingress (500); no hook verdict."
        )?,
        ("cloudflare", "extension", 501) => writeln!(
            io::stderr(),
            "PENDING local workerd rejects extension methods before Worker invocation (501)."
        )?,
        ("spin", "high-byte" | "compound-invalid", 500) => writeln!(
            io::stderr(),
            "PENDING Spin 4.0 rejects non-UTF-8 header bytes before component invocation (500)."
        )?,
        (_, "length-identical" | "length-conflict" | "te-cl" | "absolute", 400) => {}
        _ => {
            return Err(io::Error::other(format!(
                "{platform}/{case}: unexpected HTTP status {status}"
            )));
        }
    }
    require(
        verdict.is_null(),
        platform,
        case,
        "transport rejection must have no hook verdict",
    )
}

fn probes(platform: &str, port: u16, cancelled: &Cancelled) -> io::Result<Value> {
    let mut observed = json!({});
    let cases = cases();
    require(
        cases.len() == 17,
        platform,
        "fixture",
        "must execute all 17 fixed ingress cases",
    )?;
    for case in cases {
        let (status, verdict) = exchange(
            port,
            &case,
            cancelled,
            Instant::now() + Duration::from_secs(5),
        )?;
        verify_observation(platform, case.name, status, &verdict)?;
        observed[case.name] = json!({ "status": status, "verdict": verdict });
    }
    if platform == "fastly" {
        let case = Case {
            name: "snapshot-origin",
            ..ready_case()
        };
        let (status, verdict) = exchange(
            port,
            &case,
            cancelled,
            Instant::now() + Duration::from_secs(5),
        )?;
        verify_observation(platform, case.name, status, &verdict)?;
        observed[case.name] = json!({"status": status, "verdict": verdict});
        for (name, target) in [
            ("native", b"/native".as_slice()),
            ("native-dot", b"/reserved/../native".as_slice()),
        ] {
            let case = Case {
                name,
                target,
                ..ready_case()
            };
            let (status, verdict) = exchange(
                port,
                &case,
                cancelled,
                Instant::now() + Duration::from_secs(5),
            )?;
            require(
                status == 200 && verdict == json!({"native": true, "raw_target_supported": false}),
                platform,
                name,
                "native shortcut must report unavailable raw-target support",
            )?;
            observed[name] = json!({"status": status, "verdict": verdict});
        }
        writeln!(
            io::stderr(),
            "PENDING Fastly original-target and native-shortcut exclusion: safe SDK accessor required."
        )?;
    }
    Ok(observed)
}

struct Case {
    name: &'static str,
    method: &'static [u8],
    target: &'static [u8],
    headers: Vec<(&'static [u8], &'static [u8])>,
    body: &'static [u8],
}

#[cfg(test)]
mod tests {
    use super::{
        Case, HEADER_LIMIT, OwnedProcess, TimedSocket, cases, read_response, request_packet,
        verify_observation,
    };
    use serde_json::json;
    use std::io::{self, BufRead as _, BufReader, Cursor, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn cancelled_response_reads_do_not_retry_until_deadline() {
        for prefix in [
            b"".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n".as_slice(),
        ] {
            let listener =
                TcpListener::bind("127.0.0.1:0").expect("should bind a silent loopback server");
            let socket = TcpStream::connect(listener.local_addr().expect("should get port"))
                .expect("should connect to silent server");
            let (mut server, _) = listener.accept().expect("should accept test client");
            server
                .write_all(prefix)
                .expect("should write partial response");
            socket
                .set_read_timeout(Some(Duration::from_millis(10)))
                .expect("should bound individual reads");
            let cancelled = Arc::new(AtomicBool::new(false));
            let mut reader = BufReader::new(TimedSocket {
                socket,
                cancelled: &cancelled,
                deadline: Instant::now() + Duration::from_millis(150),
            });
            if !prefix.is_empty() {
                let _buffer = reader.fill_buf().expect("should buffer response headers");
            }
            cancelled.store(true, Ordering::SeqCst);
            // Bound the old retry loop as well, so a regression fails rather than hangs.
            let fallback_cancelled = Arc::clone(&cancelled);
            let fallback = thread::spawn(move || {
                thread::sleep(Duration::from_millis(200));
                fallback_cancelled.store(false, Ordering::SeqCst);
            });
            let error = read_response(&mut reader)
                .expect_err("should cancel both header and exact-body reads");
            fallback.join().expect("should join retry-loop fallback");
            assert_eq!(
                error.kind(),
                io::ErrorKind::Other,
                "should return a terminal cancellation error instead of a retryable interrupt"
            );
            assert_eq!(
                error.to_string(),
                "cancelled",
                "should preserve cancellation reason"
            );
        }
    }

    #[test]
    fn fixed_registry_contains_unicode_and_standalone_chunked_probes() {
        let cases = cases();
        assert_eq!(cases.len(), 17, "should include every fixed ingress case");
        let unicode = cases
            .iter()
            .find(|case| case.name == "unicode")
            .expect("should include unicode case");
        assert_eq!(
            unicode.headers,
            [(b"Cookie".as_slice(), b"a=\xc4\x80".as_slice())],
            "should send exact UTF-8 C4 80"
        );
        let chunked = cases
            .iter()
            .find(|case| case.name == "chunked")
            .expect("should include chunked case");
        assert_eq!(
            chunked.headers,
            [(b"Transfer-Encoding".as_slice(), b"chunked".as_slice())],
            "should probe Transfer-Encoding without Content-Length"
        );
        assert_eq!(
            chunked.body, b"0\r\n\r\n",
            "should terminate the empty chunked body"
        );
    }

    #[test]
    fn observation_verification_rejects_utf8_corruption_even_during_collection() {
        let verdict = json!({"hook": true, "ordinary": false, "global_order_unavailable": true, "spoofed_origin_ignored": true, "target_available": true, "target_matches_runtime": true, "cookie_utf8": false, "cookie_latin1": true});
        let _error = verify_observation("cloudflare", "utf8", 200, &verdict)
            .expect_err("should reject the former C3 A9 to E9 defect");
        let _error = verify_observation("cloudflare", "unicode", 500, &serde_json::Value::Null)
            .expect_err("should reject the former C4 80 conversion error");
    }

    #[test]
    fn cancellation_stops_and_reaps_the_owned_process_group() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = OwnedProcess::spawn(&mut command).expect("should spawn owned group");
        let group = process.group;
        let worker_cancelled = Arc::clone(&cancelled);
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            worker_cancelled.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        let error = process
            .wait(&cancelled)
            .expect_err("should stop waiting on interruption");
        assert_eq!(
            error.kind(),
            io::ErrorKind::Interrupted,
            "should preserve interruption reason"
        );
        drop(process);
        stopper.join().expect("should join cancellation trigger");
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "should bound cleanup even when interrupted"
        );
        let status = Command::new("/bin/kill")
            .args(["-0", "--", &format!("-{group}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("should inspect owned group");
        assert!(
            !status.success(),
            "should leave no live owned process group"
        );
    }

    #[test]
    fn packet_preserves_case_order_high_bytes_and_chunk_body() {
        let case = Case {
            name: "fixed",
            method: b"EXAMPLE-METHOD",
            target: b"/reserved//%2F?q=a%2Fb",
            headers: vec![(b"Cookie", b"a=\xff"), (b"COOKIE", b"b=2")],
            body: b"0\r\n\r\n",
        };
        assert_eq!(
            request_packet(&case),
            b"EXAMPLE-METHOD /reserved//%2F?q=a%2Fb HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\nX-Fixture: fixed\r\nCookie: a=\xff\r\nCOOKIE: b=2\r\n\r\n0\r\n\r\n",
            "should preserve the exact raw request bytes"
        );
    }

    #[test]
    fn response_decodes_chunks_extensions_and_trailers() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;fixed=yes\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\nX-Fixture: fixed\r\n\r\n";
        let (status, body) = read_response(&mut Cursor::new(raw)).expect("should parse chunks");
        assert_eq!(status, 200, "should preserve status");
        assert_eq!(body, b"{\"a\":1}", "should decode the chunk framing");
    }

    #[test]
    fn response_handles_interim_status_and_content_length() {
        let raw =
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 418 Fixed\r\nContent-Length: 2\r\n\r\n{}extra";
        let (status, body) = read_response(&mut Cursor::new(raw)).expect("should parse response");
        assert_eq!(status, 418, "should use final response status");
        assert_eq!(body, b"{}", "should read exactly Content-Length");
    }

    #[test]
    fn response_rejects_truncation_and_oversized_body() {
        for raw in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}XX".as_slice(),
        ] {
            let _error = read_response(&mut Cursor::new(raw))
                .expect_err("should reject incomplete or oversized responses");
        }
    }

    #[test]
    fn response_reads_eof_framing_and_rejects_excessive_headers() {
        let (status, body) = read_response(&mut Cursor::new(b"HTTP/1.0 200 OK\r\n\r\n{}"))
            .expect("should parse EOF-framed response");
        assert_eq!(status, 200, "should retain status");
        assert_eq!(body, b"{}", "should read the EOF-framed body");
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nX-Fixture: {}\r\n\r\n",
            "a".repeat(HEADER_LIMIT)
        );
        let _error = read_response(&mut Cursor::new(oversized))
            .expect_err("should enforce a total header budget");
    }
}
