mod evidence;
mod net;
mod runners;
use evidence::{Result, require};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct Args {
    adapter: String,
    suite: String,
    output: Option<PathBuf>,
    requests: usize,
    repetitions: usize,
    seed: u64,
    construction_rounds: u64,
    max_requests: usize,
}
impl Args {
    fn parse() -> Result<Self> {
        let mut a = Self {
            adapter: "fastly".into(),
            suite: "smoke".into(),
            output: None,
            requests: 100,
            repetitions: 3,
            seed: 856,
            construction_rounds: 0,
            max_requests: 10,
        };
        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            if key == "--require-runtime" {
                continue;
            }
            if key == "--help" || key == "-h" {
                println!(
                    "fixture-harness [--adapter fastly|cloudflare|spin|axum|all] [--suite smoke|benchmark] [--output EMPTY_DIR] [--requests 100] [--repetitions 3] [--seed 856] [--construction-rounds 0] [--max-requests 10] [--require-runtime]"
                );
                std::process::exit(0);
            }
            let value = args.next().ok_or(format!("missing value for {key}"))?;
            match key.as_str() {
                "--adapter" => a.adapter = value,
                "--suite" => a.suite = value,
                "--output" => a.output = Some(value.into()),
                "--requests" => a.requests = value.parse()?,
                "--repetitions" => a.repetitions = value.parse()?,
                "--seed" => a.seed = value.parse()?,
                "--construction-rounds" => a.construction_rounds = value.parse()?,
                "--max-requests" => a.max_requests = value.parse()?,
                _ => return Err(format!("unknown option {key}").into()),
            }
        }
        require(
            ["fastly", "cloudflare", "spin", "axum", "all"].contains(&a.adapter.as_str()),
            "unknown adapter",
        )?;
        require(
            ["smoke", "benchmark"].contains(&a.suite.as_str()),
            "unknown suite",
        )?;
        require(
            a.requests > 0 && a.repetitions > 0,
            "requests and repetitions must be positive",
        )?;
        require(
            (1..=1000).contains(&a.max_requests),
            "max-requests must be 1..1000",
        )?;
        Ok(a)
    }
}
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("fixture workspace root")
}
pub fn token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:x}-{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
pub struct Records {
    path: PathBuf,
    values: Vec<Value>,
}
impl Records {
    fn collect_clients(&mut self, fields: Value) -> Result<()> {
        for event in net::take_client_events() {
            if !self.values.iter().any(|existing| {
                existing["event"] == "client_completed" && existing["token"] == event["token"]
            }) {
                self.push(annotate(event, fields.clone()))?;
            }
        }
        Ok(())
    }
    fn push(&mut self, v: Value) -> Result<()> {
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
            "{v}"
        )?;
        self.values.push(v);
        Ok(())
    }
}
pub fn logs(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
pub fn await_evidence(mut check: impl FnMut() -> Result<()>) -> Result<()> {
    let start = Instant::now();
    loop {
        match check() {
            Ok(()) => return Ok(()),
            Err(e) if start.elapsed() >= Duration::from_secs(5) => return Err(e),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}
pub fn executable(variable: &str, default: &str) -> Option<PathBuf> {
    let value = std::env::var(variable).unwrap_or_else(|_| default.into());
    let path = PathBuf::from(&value);
    if path.components().count() > 1 {
        return path.canonicalize().ok();
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|p| p.join(&value))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
}
pub fn checked(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    require(
        status.success(),
        format!("command {command:?} failed: {status}"),
    )
}
pub struct Process(Child);
impl Process {
    pub fn start(command: &mut Command, log: &Path, port: u16) -> Result<Self> {
        let file = fs::File::create(log)?;
        let mut p = Self(
            command
                .stdout(Stdio::from(file.try_clone()?))
                .stderr(Stdio::from(file))
                .spawn()?,
        );
        let start = Instant::now();
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(p);
            }
            require(
                p.0.try_wait()?.is_none(),
                format!("runtime exited; see {}", log.display()),
            )?;
            require(
                start.elapsed() < Duration::from_secs(45),
                "runtime did not listen",
            )?;
            thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub fn req(port: u16, path: &str) -> Result<Value> {
    net::request(port, path, &token(), None, None)
}
pub fn guest(reply: &Value) -> Result<Value> {
    require(
        reply["status"] == 200,
        format!("unexpected response: {reply}"),
    )?;
    Ok(serde_json::from_str(
        reply["body"].as_str().ok_or("missing body")?,
    )?)
}
pub fn annotate(mut value: Value, fields: Value) -> Value {
    value
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    value
}
pub fn artifact(path: &Path) -> Result<Value> {
    let bytes = fs::read(path)?;
    let fingerprint = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    Ok(
        json!({"path":path,"bytes":bytes.len(),"fnv1a64":format!("{fingerprint:016x}"),"purpose":"non-security build identity"}),
    )
}
fn run() -> Result<i32> {
    let args = Args::parse()?;
    let path = args
        .output
        .clone()
        .unwrap_or_else(|| root().join(".runs").join(token()));
    if path.exists() {
        require(
            fs::read_dir(&path)?.next().is_none(),
            "output directory must be empty; refusing to overwrite evidence",
        )?;
    }
    fs::create_dir_all(&path)?;
    let path = path.canonicalize()?;
    fs::copy(root().join("Cargo.lock"), path.join("Cargo.lock"))?;
    let mut records = Records {
        path: path.join("events.jsonl"),
        values: Vec::new(),
    };
    records.push(json!({"event":"run","seed":args.seed,"suite":args.suite,"construction_rounds":args.construction_rounds,"max_requests":args.max_requests}))?;
    records.push(json!({"event":"build_environment","rustc":String::from_utf8_lossy(&Command::new("rustc").arg("--version").output()?.stdout).trim(),"host_os":std::env::consts::OS,"host_arch":std::env::consts::ARCH,"lockfile":artifact(&path.join("Cargo.lock"))?}))?;
    let adapters = if args.adapter == "all" {
        vec!["fastly", "cloudflare", "spin", "axum"]
    } else {
        vec![args.adapter.as_str()]
    };
    for adapter in adapters {
        let result = if adapter == "fastly" {
            runners::fastly(&args, &path, &mut records)
        } else {
            runners::provider(adapter, &args, &path, &mut records)
        };
        if let Err(error) = result {
            records.push(json!({"status":"fail","adapter":adapter,"error":error.to_string()}))?;
        }
    }
    let code = if records.values.iter().any(|r| r["status"] == "fail") {
        1
    } else if records
        .values
        .iter()
        .any(|r| r["status"] == "unverified" || r["status"] == "unsupported")
    {
        2
    } else {
        0
    };
    let summary = evidence::summarize(&records.values);
    let code = if summary["build_identity"]["status"] == "invalid" {
        1
    } else {
        code
    };
    fs::write(
        path.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    for (key, value) in summary["variants"].as_object().unwrap() {
        println!(
            "{key}: {} samples, completion p50 {:.3} ms",
            value["samples"],
            value["completion_ns"]["p50"].as_f64().unwrap_or(0.) / 1_000_000.
        );
    }
    println!("{}", json!({"exit_code":code,"evidence":path}));
    Ok(code)
}
fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("fixture-harness: {error}");
            std::process::exit(1);
        }
    }
}
