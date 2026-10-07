use crate::{evidence::*, net::Backend, *};
use std::collections::HashMap;
fn cargo_build() -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(root())
        .env("CARGO_TARGET_DIR", root().join("target"));
    command
}

fn built_executable(output: &[u8], name: &str) -> Result<PathBuf> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|record| {
            (record["reason"] == "compiler-artifact" && record["target"]["name"] == name)
                .then(|| record["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .ok_or_else(|| format!("Cargo did not report an executable for {name}").into())
}
/// Copies a built artifact (file or directory) into the run directory, so a
/// concurrent build into the shared target cannot swap it under a running
/// server; the copy is what gets served and fingerprinted.
fn stage(from: &Path, run: &Path) -> Result<PathBuf> {
    let to = run
        .join("artifacts")
        .join(from.file_name().ok_or("artifact has no file name")?);
    copy_tree(from, &to)?;
    Ok(to)
}
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        fs::create_dir_all(to.parent().ok_or("artifact has no parent")?)?;
        fs::copy(from, to)?;
    }
    Ok(())
}
/// Teardown shared by every runtime run. The runtime lives inside `outcome`'s
/// computation, so it has stopped by now: record client completions and guest
/// logs, then a failure carrying `fields` and `failure` context.
fn settle(
    records: &mut Records,
    log: &Path,
    fields: Value,
    failure: Value,
    outcome: Result<()>,
) -> Result<()> {
    records.collect_clients(fields.clone())?;
    for event in logs(log) {
        records.push(annotate(event, fields.clone()))?;
    }
    if let Err(error) = &outcome {
        records.push(annotate(
            annotate(json!({"status":"fail","error":error.to_string()}), fields),
            failure,
        ))?;
    }
    outcome
}
fn wrangler_runtime_config(package: &Path, main: &Path) -> Result<String> {
    let config = fs::read_to_string(package.join("wrangler.toml"))?;
    let mut skip = false;
    let mut lines = Vec::new();
    for line in config.lines() {
        if line.trim().starts_with('[') {
            skip = line.trim() == "[build]";
        }
        if skip {
            continue;
        }
        lines.push(if line.starts_with("main =") {
            format!("main = {main:?}")
        } else {
            line.to_owned()
        });
    }
    Ok(lines.join("\n") + "\n")
}

fn wasm(run: &Path, variant: &str) -> PathBuf {
    run.join("artifacts").join(wasm_name(variant))
}
fn wasm_name(variant: &str) -> String {
    let name = match variant {
        "a" => "single-request",
        "b" => "rebuild-per-request",
        "c" => "retained-app",
        "custom-a" => "custom-single-request",
        "custom-b" => "custom-rebuild-per-request",
        "custom-c" => "custom-retained-app",
        other => other,
    };
    format!("fixture-fastly-{name}.wasm")
}
fn stage_fastly_artifacts(run: &Path) -> Result<()> {
    let release = root().join("target/wasm32-wasip1/release");
    for variant in [
        "a",
        "b",
        "c",
        "custom-a",
        "custom-b",
        "custom-c",
        "logger-negative",
        "limit-requests",
        "limit-lifetime",
        "limit-memory",
        "faults",
    ] {
        stage(&release.join(wasm_name(variant)), run)?;
    }
    Ok(())
}
fn fastly_command(exe: &Path, config: &Path, run: &Path, variant: &str, port: u16) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(["serve", "--addr", &format!("127.0.0.1:{port}"), "--config"])
        .arg(config)
        .arg(wasm(run, variant));
    cmd
}

fn custom_initialization_contract(
    exe: &Path,
    config: &Path,
    run: &Path,
    records: &mut Records,
) -> Result<()> {
    let log = run.join("custom-initialization.log");
    let outcome = (|| -> Result<()> {
        let port = net::port()?;
        let _process = Process::start(
            &mut fastly_command(exe, config, run, "custom-c", port),
            &log,
            port,
        )?;
        let mut instances = Vec::new();
        for (path, status, attempts, initialized) in [
            ("/health", 200, 0, false),
            ("/initialization-error", 503, 1, false),
            ("/health", 200, 1, false),
            ("/probe/recovered", 200, 2, true),
            ("/probe/retained", 200, 2, true),
        ] {
            let reply = req(port, path)?;
            require(
                reply["status"] == status,
                "custom initialization status mismatch",
            )?;
            let value: Value = serde_json::from_str(reply["body"].as_str().ok_or("missing body")?)?;
            instances.push(value["instance"].clone());
            let same_instance = instances.iter().all(|id| id == &instances[0]);
            if same_instance {
                require(
                    value["ordinal"] == instances.len(),
                    "recovery ordinal mismatch",
                )?;
                if initialized {
                    require(value["builds"] == 1, "recovered application was rebuilt")?;
                } else {
                    require(
                        value["retained"] == false,
                        "health or failure retained an app",
                    )?;
                }
                require(
                    reply["headers"].as_array().unwrap().iter().any(|h| {
                        h[0].as_str()
                            .is_some_and(|name| name.eq_ignore_ascii_case("x-fixture-attempts"))
                            && h[1].as_str().and_then(|v| v.parse::<u64>().ok()) == Some(attempts)
                    }),
                    "lazy build attempt count mismatch",
                )?;
            }
            records.push(annotate(
                reply,
                json!({"variant":"custom-initialization", "guest":value}),
            ))?;
        }
        records.push(json!({"assertion":"custom_lazy_initialization_recovery", "status":if instances.iter().all(|id| id == &instances[0]) {"pass"} else {"unverified"}, "reason":"recovery requires all five callbacks in the same guest"}))
    })();
    settle(
        records,
        &log,
        json!({"variant":"custom-initialization","repetition":0}),
        json!({"adapter":"fastly"}),
        outcome,
    )
}

pub fn fastly(args: &Args, run: &Path, records: &mut Records) -> Result<()> {
    let Some(exe) = executable("VICEROY_BIN", "viceroy") else {
        return records.push(json!({"status":"unsupported","reason":"Viceroy unavailable"}));
    };
    let version = Command::new(&exe).arg("--version").output()?;
    require(version.status.success(), "Viceroy version command failed")?;
    let version = String::from_utf8(version.stdout)?.trim().to_owned();
    let pins = fs::read_to_string(root().join("../../../.tool-versions"))?;
    let pin = pins
        .lines()
        .find_map(|line| line.strip_prefix("viceroy "))
        .map(str::trim)
        .ok_or("missing Viceroy pin")?;
    records.push(json!({"event":"runtime","adapter":"fastly","executable":exe,"version":version,"expected":pin}))?;
    if version.split_whitespace().nth(1) != Some(pin) {
        // Keep going: an unpinned Viceroy still gets signal, and a failure still exits 1.
        records.push(json!({"status":"unverified","adapter":"fastly","reason":format!("Viceroy version {version} does not match pinned {pin}")}))?;
    }
    checked(
        cargo_build()
            .args([
                "build",
                "--locked",
                "--release",
                "-p",
                "fixture-fastly",
                "--bins",
                "--target",
                "wasm32-wasip1",
            ])
            .env("FIXTURE_BUILD_ROUNDS", args.construction_rounds.to_string())
            .env("FIXTURE_MAX_REQUESTS", args.max_requests.to_string()),
    )?;
    stage_fastly_artifacts(run)?;
    let config_snapshot = run.join("fastly.toml");
    fs::copy(
        root().join("crates/fixture-fastly/fastly.toml"),
        &config_snapshot,
    )?;
    if args.suite == "smoke" {
        custom_initialization_contract(&exe, &config_snapshot, run, records)?;
    }
    let mut variants = vec!["a", "b", "c", "custom-a", "custom-b", "custom-c"];
    if args.suite == "smoke" {
        variants.extend(["limit-requests", "limit-lifetime", "limit-memory"]);
    }
    let mut rng = args.seed;
    for repetition in 0..if args.suite == "benchmark" {
        args.repetitions
    } else {
        1
    } {
        // A recorded deterministic Fisher-Yates order; independent of language RNG versions.
        for i in (1..variants.len()).rev() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            variants.swap(i, (rng as usize) % (i + 1));
        }
        records
            .push(json!({"event":"variant_order","repetition":repetition,"variants":variants}))?;
        for variant in &variants {
            fastly_variant(
                args,
                &exe,
                &config_snapshot,
                run,
                variant,
                repetition,
                records,
            )?;
        }
    }
    if args.suite == "smoke" {
        logging(&exe, run, records)?;
        faults(&exe, run, records)?;
    }
    if records
        .values
        .iter()
        .any(|e| e["event"] == "heap_preflight" && e["heap_mib"].is_null())
    {
        records.push(json!({"status":"unsupported","reason":"heap snapshot unavailable; memory limit unverified"}))?;
    }
    Ok(())
}
fn fastly_variant(
    args: &Args,
    exe: &Path,
    config: &Path,
    run: &Path,
    variant: &str,
    repetition: usize,
    records: &mut Records,
) -> Result<()> {
    records.push(json!({"event":"build","variant":variant,"repetition":repetition,"target":"wasm32-wasip1","artifact":artifact(&wasm(run, variant))?,"config":artifact(config)?}))?;
    let log = run.join(format!("{variant}-{repetition}.log"));
    let outcome = (|| -> Result<()> {
        let backend = Backend::start()?;
        let port = net::port()?;
        let _process = Process::start(
            &mut fastly_command(exe, config, run, variant, port),
            &log,
            port,
        )?;
        let seen = fastly_probes(args, port, variant, repetition, records)?;
        if ["c", "custom-c"].contains(&variant) {
            thread::sleep(Duration::from_secs(2));
            let reply = req(port, "/probe/after-idle")?;
            require(
                !seen.contains_key(
                    guest(&reply)?["instance"]
                        .as_str()
                        .ok_or("missing instance")?,
                ),
                "idle guest was reused",
            )?;
            records.push(annotate(
                reply,
                json!({"variant":variant,"repetition":repetition,"assertion":"idle_fresh_initialization"}),
            ))?;
        }
        let bindings = req(port, "/bindings")?;
        require(
            guest(&bindings)? == json!({"config":true,"kv":true,"secrets":true,"unknown":false}),
            "binding mismatch",
        )?;
        records.push(annotate(
            bindings,
            json!({"variant":variant,"repetition":repetition,"assertion":"binding_reads"}),
        ))?;
        require(
            cookie_values(&req(port, "/cookies")?["headers"])
                == vec!["first=1; Path=/", "second=2; Path=/"],
            "duplicate cookies lost",
        )?;
        require(
            req(port, "/rendered-error")?["status"] == 400,
            "rendered error mismatch",
        )?;
        require(
            req(port, "/probe/after")?["status"] == 200,
            "request after error failed",
        )?;
        if variant.starts_with("custom-") {
            custom_stream_checks(port, &backend, &log, variant, repetition, records)?;
        }
        if args.suite == "smoke" {
            smoke_origin_checks(port, &backend, &log, variant, repetition, records)?;
        }
        if variant.starts_with("limit-") {
            await_evidence(|| validate_limit_summaries(&logs(&log)))?;
        }
        records.push(json!({"status":"pass","variant":variant,"repetition":repetition,"assertions":["initialization","isolation","cookies","rendered_error"]}))
    })();
    settle(
        records,
        &log,
        json!({"variant":variant,"repetition":repetition}),
        json!({"adapter":"fastly"}),
        outcome,
    )
}
fn fastly_probes(
    args: &Args,
    port: u16,
    variant: &str,
    repetition: usize,
    records: &mut Records,
) -> Result<HashMap<String, Value>> {
    let mut seen: HashMap<String, Value> = HashMap::new();
    let mut reused = false;
    for _ in 0..if args.suite == "benchmark" {
        args.requests
    } else {
        5
    } {
        let token = token();
        let reply = net::request(port, &format!("/probe/{token}"), &token, None, None)?;
        let value = guest(&reply)?;
        require(
            value["token"] == token && value["path"] == token && value["body"] == token,
            format!("request isolation: {value}"),
        )?;
        if let Some(previous) = seen.get(value["instance"].as_str().ok_or("missing instance")?) {
            reused = true;
            require(
                value["ordinal"].as_u64() > previous["ordinal"].as_u64()
                    && value["correlation"] != previous["correlation"],
                "reuse ordinal/correlation mismatch",
            )?;
        }
        require(
            value["mutated"] == variant.starts_with("custom-"),
            "raw request mutation mismatch",
        )?;
        if variant.starts_with("custom-") {
            require(
                reply["headers"].as_array().unwrap().iter().any(|h| {
                    h[0].as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("x-fixture-finalized"))
                        && h[1] == token
                }),
                "request-specific response extension was lost or leaked",
            )?;
        }
        match variant {
            "c" | "custom-c" => require(
                value["ordinal"]
                    .as_u64()
                    .is_some_and(|n| n <= args.max_requests as u64)
                    && value["builds"] == 1,
                "retained app initialization mismatch",
            )?,
            "b" | "custom-b" => require(
                value["builds"] == value["ordinal"],
                "per-request initialization mismatch",
            )?,
            _ => require(
                value["ordinal"] == 1 && value["builds"] == 1,
                "single-request initialization mismatch",
            )?,
        }
        validate_shared_state(&value, ["c", "custom-c"].contains(&variant))?;
        seen.insert(value["instance"].as_str().unwrap().into(), value.clone());
        records.push(annotate(
            reply,
            json!({"variant":variant,"repetition":repetition,"guest":value,"workload":"probe"}),
        ))?;
    }
    if ["b", "c", "custom-b", "custom-c"].contains(&variant) && !reused {
        records.push(json!({"status":"unverified","variant":variant,"repetition":repetition,"reason":"no actual guest reuse observed"}))?;
    }
    Ok(seen)
}
fn custom_stream_checks(
    port: u16,
    backend: &Backend,
    log: &Path,
    variant: &str,
    repetition: usize,
    records: &mut Records,
) -> Result<()> {
    let token = token();
    let stream = net::request(
        port,
        "/stream",
        &token,
        Some(&backend.url(&format!("/chunks?token={token}"))),
        Some(&backend.release),
    )?;
    require(
        stream["body"] == "firstsecond",
        "progressive stream mismatch",
    )?;
    require(
        stream["headers"].as_array().unwrap().iter().any(|h| {
            h[0].as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("x-fixture-finalized"))
                && h[1] == token
        }),
        "response not finalized",
    )?;
    await_evidence(|| validate_post_send(&token, &backend.receipts.lock().unwrap(), &logs(log)))?;
    records.push(annotate(
        stream,
        json!({"variant":variant,"repetition":repetition,"assertion":"progressive_stream_and_post_send"}),
    ))?;
    *backend.release.0.lock().unwrap() = false;
    let large_token = crate::token();
    let large = net::request(
        port,
        "/stream",
        &large_token,
        Some(&backend.url("/chunks?large=1")),
        Some(&backend.release),
    )?;
    let large_body = large["body"].as_str().ok_or("missing large body")?;
    require(
        large_body.starts_with("first") && large_body.len() == 5 + 256 * 1024,
        "large stream truncated",
    )?;
    records.push(annotate(
        large,
        json!({"variant":variant,"repetition":repetition,"assertion":"large_progressive_stream"}),
    ))?;
    // Trigger truncation only after the downstream client has observed the partial body.
    *backend.release.0.lock().unwrap() = false;
    let failure_token = crate::token();
    let failure = net::request(
        port,
        "/stream-error",
        &failure_token,
        Some(&backend.url(&format!("/abort?gated=1&token={failure_token}"))),
        Some(&backend.release),
    );
    let failure = match failure {
        Ok(reply) => {
            return Err(format!("interrupted stream unexpectedly completed: {reply}").into());
        }
        Err(error) => error,
    };
    let partial = failure
        .downcast_ref::<net::InterruptedResponse>()
        .ok_or("stream failure occurred before response body: not post-commit evidence")?;
    require(
        partial.status == 200 && partial.body == b"partial",
        "missing partial committed response",
    )?;
    await_evidence(|| {
        require(
            logs(log)
                .iter()
                .any(|e| e["event"] == "post_commit_error" && e["token"] == failure_token),
            "missing correlated post-commit error",
        )
    })?;
    records.push(json!({"event":"expected_stream_error","variant":variant,"repetition":repetition,"token":failure_token,"status_code":partial.status,"partial_body":String::from_utf8_lossy(&partial.body),"source":"controlled_backend_abort"}))
}
fn smoke_origin_checks(
    port: u16,
    backend: &Backend,
    log: &Path,
    variant: &str,
    repetition: usize,
    records: &mut Records,
) -> Result<()> {
    for repeat in 0..3 {
        let reply = net::request(
            port,
            "/origin/repeated",
            &token(),
            Some(&backend.url("/ok")),
            None,
        )?;
        require(
            reply["status"] == 200 && reply["body"] == "ok",
            "repeated origin failed",
        )?;
        records.push(annotate(reply, json!({"variant":variant,"repetition":repetition,"origin_repeat":repeat,"assertion":"bounded_repeated_origin"})))?;
    }
    for origin in 0..3 {
        let distinct = Backend::start()?;
        let reply = net::request(
            port,
            &format!("/origin/{origin}"),
            &token(),
            Some(&distinct.url("/ok")),
            None,
        )?;
        require(
            reply["status"] == 200 && reply["body"] == "ok",
            "distinct origin failed",
        )?;
        records.push(annotate(
            reply,
            json!({"variant":variant,"repetition":repetition,"assertion":"bounded_distinct_origin"}),
        ))?;
    }
    if ["custom-b", "custom-c"].contains(&variant) {
        let token = token();
        if let Ok(failed) = net::request(port, "/panic", &token, None, None) {
            require(
                failed["status"].as_u64().is_some_and(|s| s >= 500),
                "panic did not fail",
            )?;
        }
        let fresh = req(port, "/probe/fresh")?;
        let value = guest(&fresh)?;
        let mut failed_guest = Value::Null;
        await_evidence(|| {
            failed_guest = logs(log)
                .iter()
                .find(|e| e["event"] == "request_start" && e["token"] == token)
                .map(|e| e["instance"].clone())
                .ok_or("missing panic attempt")?;
            Ok(())
        })?;
        require(value["instance"] != failed_guest, "terminated guest reused")?;
        records.push(annotate(fresh,json!({"variant":variant,"repetition":repetition,"assertion":"terminated_guest_not_reused","failed_guest":failed_guest})))?;
    }
    Ok(())
}
fn logging(exe: &Path, run: &Path, records: &mut Records) -> Result<()> {
    let Some(service) = records
        .values
        .iter()
        .find_map(|e| e["service_id"].as_str())
        .map(str::to_owned)
    else {
        return records.push(json!({"status":"unverified","reason":"service ID unavailable for logging configuration"}));
    };
    let base = fs::read_to_string(root().join("crates/fixture-fastly/fastly.toml"))?;
    let config = run.join("logging.toml");
    fs::write(
        &config,
        format!(
            "{base}\n[local_server.config_stores.edgezero_runtime_env]\nformat = \"inline-toml\"\n[local_server.config_stores.edgezero_runtime_env.contents]\n\"EDGEZERO__SERVICES__{service}__LOGGING__ENDPOINT\" = \"fixture-logs\"\n"
        ),
    )?;
    for variant in ["b", "c", "custom-c", "logger-negative"] {
        let log = run.join(format!("logging-{variant}.log"));
        let outcome = (|| -> Result<()> {
            let port = net::port()?;
            let process = Process::start(
                &mut fastly_command(exe, &config, run, variant, port),
                &log,
                port,
            )?;
            let mut replies = Vec::new();
            for _ in 0..if variant == "logger-negative" { 2 } else { 3 } {
                replies.push(req(port, "/probe/log")?);
            }
            drop(process);
            records.push(json!({"event":"logging_check","variant":variant,"statuses":replies.iter().map(|r|r["status"].clone()).collect::<Vec<_>>(),"log_file":log.file_name().map(|n| n.to_string_lossy())}))?;
            if variant == "logger-negative" {
                let starts = logs(&log)
                    .into_iter()
                    .filter(|e| e["event"] == "request_start")
                    .collect::<Vec<_>>();
                let status = validate_negative_logging(&replies, &starts)?;
                records.push(json!({"status":status,"assertion":"repeated_logger_installation","variant":variant}))?;
            } else {
                let guests = replies.iter().map(guest).collect::<Result<Vec<_>>>()?;
                let text = fs::read_to_string(&log)?;
                let delivered = text
                    .lines()
                    .filter(|l| l.starts_with("fixture-logs :: "))
                    .filter_map(|l| {
                        l.rsplit_once("fixture request ")
                            .map(|(_, v)| v.trim().to_owned())
                    })
                    .collect::<Vec<_>>();
                let status = validate_logging(&guests, &delivered)?;
                records.push(json!({"status":status,"assertion":"named_endpoint_receipt","variant":variant,"received":delivered.len()}))?;
            }
            Ok(())
        })();
        settle(
            records,
            &log,
            json!({"variant":format!("logging-{variant}"),"repetition":0}),
            json!({"adapter":"fastly"}),
            outcome,
        )?;
    }
    Ok(())
}
pub fn provider(name: &str, args: &Args, run: &Path, records: &mut Records) -> Result<()> {
    let package = root().join("crates").join(format!("fixture-{name}"));
    let Some((exe, staged)) = provider_build(name, &package, run, records)? else {
        return Ok(());
    };
    let modes = if name == "axum" {
        vec!["retained"]
    } else {
        vec!["per-request", "retained"]
    };
    for mode in modes {
        let port = net::port()?;
        let mut command = provider_command(name, mode, &exe, &staged, &package, run, port)?;
        let log = run.join(format!("{name}-{mode}.log"));
        let mut failure = json!({"phase":"startup"});
        let outcome = Process::start(&mut command, &log, port)
            .and_then(|_process| provider_checks(name, mode, port, args, records, &mut failure));
        settle(
            records,
            &log,
            json!({"adapter":name,"mode":mode}),
            failure,
            outcome,
        )?;
    }
    Ok(())
}
/// Builds the provider fixture and stages its artifact in the run directory.
/// Returns the runtime executable and the staged artifact, or `None` when a
/// required tool is unavailable.
fn provider_build(
    name: &str,
    package: &Path,
    run: &Path,
    records: &mut Records,
) -> Result<Option<(PathBuf, PathBuf)>> {
    if name == "axum" {
        let output = cargo_build()
            .args([
                "build",
                "--locked",
                "-p",
                "fixture-axum",
                "--message-format=json-render-diagnostics",
            ])
            .stderr(Stdio::inherit())
            .output()?;
        require(output.status.success(), "Axum fixture build failed")?;
        let staged = stage(&built_executable(&output.stdout, "fixture-axum")?, run)?;
        records.push(json!({"event":"build","adapter":name,"artifact":artifact(&staged)?}))?;
        return Ok(Some((staged.clone(), staged)));
    }
    let (variable, tool) = if name == "cloudflare" {
        ("WRANGLER_BIN", "wrangler")
    } else {
        ("SPIN_BIN", "spin")
    };
    let Some(exe) = executable(variable, tool) else {
        records.push(
            json!({"status":"unsupported","adapter":name,"reason":format!("{tool} unavailable")}),
        )?;
        return Ok(None);
    };
    records.push(json!({"event":"runtime","adapter":name,"executable":exe,"version":String::from_utf8_lossy(&Command::new(&exe).arg("--version").output()?.stdout).trim()}))?;
    let (staged, wasm) = if name == "spin" {
        let help = Command::new(&exe).args(["up", "--help"]).output()?;
        fs::write(run.join("spin-up-help.txt"), &help.stdout)?;
        records.push(json!({"event":"runtime_controls","adapter":"spin","selected":"host defaults","source":"spin-up-help.txt"}))?;
        checked(cargo_build().args([
            "build",
            "--locked",
            "--release",
            "-p",
            "fixture-spin",
            "--target",
            "wasm32-wasip2",
        ]))?;
        let staged = stage(
            &root().join("target/wasm32-wasip2/release/fixture_spin.wasm"),
            run,
        )?;
        (staged.clone(), staged)
    } else {
        let Some(builder) = executable("WORKER_BUILD_BIN", "worker-build") else {
            records.push(json!({"status":"unsupported","reason":"worker-build unavailable"}))?;
            return Ok(None);
        };
        checked(
            Command::new(builder)
                .args(["--release", ".", "--", "--locked"])
                .env("CARGO_TARGET_DIR", root().join("target"))
                .current_dir(package),
        )?;
        let staged = stage(&package.join("build"), run)?;
        // Only the explicit builder runs before artifact fingerprinting.
        fs::write(
            run.join("wrangler.toml"),
            wrangler_runtime_config(package, &staged.join("worker/shim.mjs"))?,
        )?;
        let wasm = staged.join("index_bg.wasm");
        (staged, wasm)
    };
    records.push(json!({"event":"build","adapter":name,"artifact":artifact(&wasm)?}))?;
    Ok(Some((exe, staged)))
}
fn provider_command(
    name: &str,
    mode: &str,
    exe: &Path,
    staged: &Path,
    package: &Path,
    run: &Path,
    port: u16,
) -> Result<Command> {
    let mut command = Command::new(exe);
    command
        .env("CARGO_TARGET_DIR", root().join("target"))
        .current_dir(package)
        .env("EDGEZERO__ADAPTER__HOST", "127.0.0.1")
        .env("EDGEZERO__ADAPTER__PORT", port.to_string());
    if name == "axum" {
        let state = run.join("axum-state");
        fs::create_dir_all(state.join(".edgezero"))?;
        fs::write(
            state.join(".edgezero/local-config-fixture_config.json"),
            "{\"marker\":\"local-fixture\"}",
        )?;
        command
            .current_dir(state)
            .env("fixture_marker", "fixture-only-value");
    } else if name == "cloudflare" {
        let state = run.join(mode).join("worker-state");
        checked(
            Command::new(exe)
                .args([
                    "kv",
                    "key",
                    "put",
                    "marker",
                    "local-fixture",
                    "--binding",
                    "fixture_config",
                    "--local",
                    "--persist-to",
                ])
                .arg(&state)
                .current_dir(package),
        )?;
        command
            .arg("--config")
            .arg(run.join("wrangler.toml"))
            .args([
                "dev",
                "--local",
                "--ip",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--var",
                &format!("FIXTURE_MODE:{mode}"),
                "--persist-to",
            ])
            .arg(state);
    } else {
        let manifest = fs::read_to_string(package.join("spin.toml"))?
            .replace(
                "../../target/wasm32-wasip2/release/fixture_spin.wasm",
                &staged.display().to_string(),
            )
            .replace(
                "FIXTURE_MODE = \"retained\"",
                &format!("FIXTURE_MODE = \"{mode}\""),
            );
        let config = run.join(format!("spin-{mode}.toml"));
        fs::write(&config, manifest)?;
        command
            .args(["up", "--listen", &format!("127.0.0.1:{port}"), "--from"])
            .arg(config)
            .arg("--runtime-config-file")
            .arg(package.join("runtime-config.toml"));
    }
    Ok(command)
}
/// Runs the provider assertions against a listening runtime, keeping
/// `failure` pointed at the current phase so a failure record names its step.
fn provider_checks(
    name: &str,
    mode: &str,
    port: u16,
    args: &Args,
    records: &mut Records,
    failure: &mut Value,
) -> Result<()> {
    let mut seen: HashMap<String, Value> = HashMap::new();
    let mut reused = false;
    let repetitions = if args.suite == "benchmark" {
        args.repetitions
    } else {
        1
    };
    let requests = if args.suite == "benchmark" {
        args.requests
    } else {
        5
    };
    for repetition in 0..repetitions {
        *failure = json!({"phase":"probe","repetition":repetition});
        for _ in 0..requests {
            let token = token();
            let reply = net::request(port, &format!("/probe/{token}"), &token, None, None)?;
            let value = guest(&reply)?;
            require(
                value["token"] == token && value["path"] == token && value["body"] == token,
                "probe request isolation mismatch",
            )?;
            if mode == "retained" {
                require(value["builds"] == 1, "retained initialization mismatch")?;
            }
            validate_shared_state(&value, mode == "retained")?;
            let instance = value["instance"].as_str().ok_or("missing instance")?;
            if let Some(previous) = seen.get(instance) {
                reused = true;
                require(
                    value["ordinal"].as_u64() > previous["ordinal"].as_u64(),
                    "ordinal did not increase",
                )?;
                if mode == "per-request" {
                    require(
                        value["builds"].as_u64() > previous["builds"].as_u64(),
                        "per-request app was not rebuilt",
                    )?;
                }
            }
            seen.insert(instance.into(), value.clone());
            records.push(annotate(
                reply,
                json!({"guest":value,"adapter":name,"mode":mode,"repetition":repetition,"workload":"probe"}),
            ))?;
        }
    }
    if !reused {
        records.push(json!({"status":"unverified","adapter":name,"mode":mode,"reason":"no same-guest sequential reuse"}))?;
    }
    if mode == "retained" && name != "axum" {
        *failure = json!({"phase":"binding_failure"});
        let failed = req(port, "/binding-failure")?;
        require(
            failed["status"] == 503,
            "injected required binding did not fail",
        )?;
        let failed_guest: Value = serde_json::from_str(failed["body"].as_str().unwrap())?;
        require(
            failed_guest["error"]
                .as_str()
                .is_some_and(|error| error.contains("missing_fixture_kv")),
            "binding failure did not identify the injected missing KV binding",
        )?;
        let recovered = guest(&req(port, "/probe/after-binding-failure")?)?;
        require(
            recovered["builds"] == 1,
            "app rebuilt after binding failure",
        )?;
        records.push(json!({"adapter":name,"mode":mode,"status":if failed_guest["instance"] == recovered["instance"] {"pass"} else {"unverified"},"assertion":"same_guest_binding_recovery","source":"injected_required_binding"}))?;
    }
    *failure = json!({"phase":"bindings"});
    let bindings = req(port, "/bindings")?;
    require(
        guest(&bindings)? == json!({"config":true,"kv":true,"secrets":true,"unknown":false}),
        "binding mismatch",
    )?;
    records.push(annotate(
        bindings,
        json!({"adapter":name,"mode":mode,"assertion":"binding_reads"}),
    ))?;
    *failure = json!({"phase":"cookies"});
    let cookies = req(port, "/cookies")?;
    if cookie_values(&cookies["headers"]) != vec!["first=1; Path=/", "second=2; Path=/"] {
        records.push(annotate(cookies,json!({"status":"fail","adapter":name,"mode":mode,"phase":"cookies","reason":"duplicate Set-Cookie values were not preserved"})))?;
    }
    *failure = json!({"phase":"rendered_error"});
    require(
        req(port, "/rendered-error")?["status"] == 400,
        "rendered error mismatch",
    )?;
    if name != "axum" {
        *failure = json!({"phase":"alternate_app"});
        require(
            req(port, "/other")?["body"] == "other-app",
            "alternate app mismatch",
        )?;
    }
    *failure = json!({"phase":"overlap"});
    overlap_check(name, mode, port, records)
}
fn overlap_check(name: &str, mode: &str, port: u16, records: &mut Records) -> Result<()> {
    let mut observed_overlap = false;
    for attempt in 0..3 {
        let backend = Backend::start()?;
        let tokens = [token(), token()];
        let replies = thread::scope(|scope| {
            let jobs = tokens
                .iter()
                .map(|token| {
                    let url = backend.url("/barrier");
                    scope.spawn(move || {
                        net::request(port, &format!("/overlap/{token}"), token, Some(&url), None)
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .map(|job| {
                    job.join()
                        .map_err(|_| "overlap request thread panicked".into())
                        .and_then(|v| v)
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let values = replies.iter().map(guest).collect::<Result<Vec<_>>>()?;
        for (value, token) in values.iter().zip(&tokens) {
            require(
                value["token"] == *token
                    && value["before"]["token"] == *token
                    && value["path"] == *token
                    && value["before"]["path"] == *token,
                "overlap request-local values changed",
            )?;
        }
        if let Err(error) = validate_overlap(&values[0], &values[1]) {
            records.push(json!({"event":"overlap_observation","adapter":name,"mode":mode,"attempt":attempt,"reason":error.to_string(),"observations":values}))?;
            continue;
        }
        observed_overlap = true;
        break;
    }
    records.push(json!({"status":if observed_overlap {"pass"} else {"unverified"},"adapter":name,"mode":mode,"assertion":"same_guest_overlap","attempt_limit":3}))
}

fn faults(exe: &Path, run: &Path, records: &mut Records) -> Result<()> {
    let backend = Backend::start()?;
    let config = run.join("faults.toml");
    fs::write(
        &config,
        format!(
            "{}\n[local_server.backends.fault-origin]\nurl = {:?}\n",
            fs::read_to_string(root().join("crates/fixture-fastly/fastly.toml"))?,
            backend.url("/")
        ),
    )?;
    for (index, path) in [
        "/constructor-panic",
        "/collect-error",
        "/inbound-error",
        "/terminal-store-error",
        "/recoverable-store-error",
    ]
    .iter()
    .enumerate()
    {
        let log = run.join(format!("fault-{index}.log"));
        let outcome = (|| -> Result<()> {
            let port = net::port()?;
            let _process = Process::start(
                &mut fastly_command(exe, &config, run, "faults", port),
                &log,
                port,
            )?;
            let malformed_token = token();
            let rejected = net::malformed_request(port, &malformed_token)?;
            require(
                rejected.contains(" 400 "),
                format!("malformed request was not rejected: {rejected}"),
            )?;
            require(
                !logs(&log).iter().any(|e| e["token"] == malformed_token),
                "malformed request reached callback unexpectedly",
            )?;
            records.push(json!({"status":"pass","assertion":"malformed_rejected_before_dispatch","token":malformed_token,"status_line":rejected}))?;
            let failed_token = token();
            let reply = if *path == "/collect-error" {
                let failure = net::request(port, path, &failed_token, None, None)
                    .err()
                    .ok_or("owned egress must abandon the failed response stream")?;
                if let Some(partial) = failure.downcast_ref::<net::InterruptedResponse>() {
                    require(
                        partial.status == 200 && partial.body.is_empty(),
                        "egress source failure sent a replacement response",
                    )?;
                    json!({"status":partial.status,"body":"","interrupted":true})
                } else {
                    // An immediate source error can reset delivery before the head is observed.
                    require(
                        failure.downcast_ref::<net::ResponseHeadAborted>().is_some(),
                        format!("unexpected response-head failure: {failure}"),
                    )?;
                    json!({"no_response_head":true,"interrupted":true,"error":failure.to_string()})
                }
            } else {
                net::request(port, path, &failed_token, None, None)?
            };
            let recoverable = *path == "/recoverable-store-error";
            let rendered_ingress = *path == "/inbound-error";
            require(
                (*path == "/collect-error" && reply["no_response_head"] == true)
                    || reply["status"]
                        == if *path == "/collect-error" {
                            200
                        } else if recoverable {
                            503
                        } else {
                            500
                        },
                format!("fault not observed: {path}: {reply}"),
            )?;
            let following = req(port, "/probe/recovered")?;
            let value = guest(&following)?;
            let mut failed_guest = Value::Null;
            await_evidence(|| {
                let events = logs(&log);
                failed_guest = events
                    .iter()
                    .find(|e| e["event"] == "request_start" && e["token"] == failed_token)
                    .map(|e| e["instance"].clone())
                    .ok_or("missing fault attempt")?;
                let event = if *path == "/constructor-panic" {
                    "constructor_panic"
                } else if rendered_ingress {
                    "ingress_error_response"
                } else {
                    "adapter_error"
                };
                require(
                    events
                        .iter()
                        .any(|e| e["event"] == event && e["instance"] == failed_guest),
                    "missing actual failure boundary",
                )
            })?;
            if *path == "/collect-error" {
                let terminal = logs(&log)
                    .into_iter()
                    .filter(|event| {
                        event["event"] == "egress_terminal" && event["token"] == failed_token
                    })
                    .collect::<Vec<_>>();
                require(
                    terminal.len() == 1
                        && terminal[0]["outcome"] == "SourceError"
                        && terminal[0]["body_kind"] == "Application"
                        && terminal[0]["bytes_written"] == 0
                        && terminal[0]["fallback"].is_null(),
                    "immediate source failure must terminate once without fallback delivery",
                )?;
            }
            if recoverable || rendered_ingress {
                let status = if value["instance"] == failed_guest {
                    "pass"
                } else {
                    "unverified"
                };
                require(
                    value["builds"] == 1,
                    "retained app rebuilt after handled failure",
                )?;
                require(
                    guest(&req(port, "/bindings")?)?
                        == json!({"config":true,"kv":true,"secrets":true,"unknown":false}),
                    "bindings failed to recover",
                )?;
                if rendered_ingress {
                    records.push(json!({"status":status,"assertion":"admitted_body_failure_rendered","instance":failed_guest,"source":"canonical_admitted_request_dispatch"}))?;
                } else {
                    records.push(json!({"status":status,"assertion":"injected_selector_failure_recovery","instance":failed_guest,"source":"custom_callback_handles_real_registry_error"}))?;
                }
            } else {
                require(
                    value["instance"] != failed_guest,
                    "terminal failure guest reused",
                )?;
                if *path != "/constructor-panic" {
                    await_evidence(|| {
                        require(
                            logs(&log).iter().any(|e| {
                                e["event"] == "sdk_summary"
                                    && e["instance"] == failed_guest
                                    && e["attempted"] == 1
                            }),
                            "missing terminal attempt summary",
                        )
                    })?;
                }
                records.push(json!({"status":"pass","assertion":"terminal_fault","path":path,"failed_guest":failed_guest,"source":"controlled_fixture_input"}))?;
            }
            records.push(annotate(
                reply,
                json!({"variant":"faults","repetition":index,"assertion":path}),
            ))?;
            Ok(())
        })();
        settle(
            records,
            &log,
            json!({"variant":"faults","repetition":index}),
            json!({"adapter":"fastly","path":path}),
            outcome,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cargo_artifacts_select_the_fresh_executable_in_a_configured_target() {
        let output = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"dependency\"},\"executable\":null}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"fixture-axum\"},\"executable\":\"/tmp/target/host-triple/debug/fixture-axum\"}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
        );
        assert_eq!(
            built_executable(output.as_bytes(), "fixture-axum").unwrap(),
            PathBuf::from("/tmp/target/host-triple/debug/fixture-axum")
        );
        assert!(built_executable(b"", "fixture-axum").is_err());
    }
    #[test]
    fn wrangler_runtime_configuration_does_not_rebuild_fingerprinted_artifacts() {
        let package = root().join("crates/fixture-cloudflare");
        let staged = Path::new("/run/artifacts/build/worker/shim.mjs");
        let config = wrangler_runtime_config(&package, staged).unwrap();
        assert!(!config.contains("[build]") && !config.contains("worker-build"));
        assert!(config.contains(&format!("main = {staged:?}")));
        assert!(config.contains("binding = \"fixture_kv\""));
    }
    #[test]
    fn builds_share_a_fixed_target_directory() {
        let command = cargo_build();
        let target = root().join("target");
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "CARGO_TARGET_DIR" && value == Some(target.as_os_str()))
        );
        assert_eq!(command.get_current_dir(), Some(root().as_path()));
    }
    #[test]
    fn served_artifacts_are_copies_inside_the_run_directory() {
        let run = std::env::temp_dir().join(format!("fixture-harness-stage-{}", token()));
        let source = run.join("source/fixture-fastly-retained-app.wasm");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"first").unwrap();
        let staged = stage(&source, &run).unwrap();
        assert_eq!(staged, wasm(&run, "c"));
        fs::write(&source, b"second").unwrap();
        assert_eq!(fs::read(&staged).unwrap(), b"first");
        fs::remove_dir_all(&run).unwrap();
    }
}
