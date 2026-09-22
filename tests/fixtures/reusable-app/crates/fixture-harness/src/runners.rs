use crate::{evidence::*, net::Backend, *};
use std::collections::HashMap;
fn wasm(variant: &str) -> PathBuf {
    let name = match variant {
        "a" => "single-request",
        "b" => "rebuild-per-request",
        "c" => "retained-app",
        "custom-a" => "custom-single-request",
        "custom-b" => "custom-rebuild-per-request",
        "custom-c" => "custom-retained-app",
        other => other,
    };
    root().join(format!(
        "target/wasm32-wasip1/release/fixture-fastly-{name}.wasm"
    ))
}
fn fastly_command(exe: &Path, config: &Path, variant: &str, port: u16) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(["serve", "--addr", &format!("127.0.0.1:{port}"), "--config"])
        .arg(config)
        .arg(wasm(variant));
    cmd
}

fn custom_initialization_contract(
    exe: &Path,
    config: &Path,
    run: &Path,
    records: &mut Records,
) -> Result<()> {
    let port = net::port()?;
    let process = Process::start(
        &mut fastly_command(exe, config, "custom-c", port),
        &run.join("custom-initialization.log"),
        port,
    )?;
    let outcome = (|| -> Result<()> {
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
    drop(process);
    outcome
}

pub fn fastly(args: &Args, run: &Path, records: &mut Records) -> Result<()> {
    let Some(exe) = executable("VICEROY_BIN", "viceroy") else {
        return records.push(json!({"status":"unsupported","reason":"Viceroy unavailable"}));
    };
    checked(
        Command::new("cargo")
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
            .current_dir(root())
            .env("FIXTURE_BUILD_ROUNDS", args.construction_rounds.to_string())
            .env("FIXTURE_MAX_REQUESTS", args.max_requests.to_string()),
    )?;
    records.push(json!({"event":"runtime","adapter":"fastly","executable":exe,"version":String::from_utf8_lossy(&Command::new(&exe).arg("--version").output()?.stdout).trim()}))?;
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
            records.push(json!({"event":"build","variant":variant,"repetition":repetition,"target":"wasm32-wasip1","artifact":artifact(&wasm(variant))?,"config":artifact(&config_snapshot)?}))?;
            let port = net::port()?;
            let log = run.join(format!("{variant}-{repetition}.log"));
            let backend = Backend::start()?;
            let process = Process::start(
                &mut fastly_command(&exe, &config_snapshot, variant, port),
                &log,
                port,
            )?;
            let outcome = (|| -> Result<()> {
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
                    if let Some(previous) =
                        seen.get(value["instance"].as_str().ok_or("missing instance")?)
                    {
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
                                h[0].as_str().is_some_and(|name| {
                                    name.eq_ignore_ascii_case("x-fixture-finalized")
                                }) && h[1] == token
                            }),
                            "request-specific response extension was lost or leaked",
                        )?;
                    }
                    match *variant {
                        "c" | "custom-c" => require(
                            value["ordinal"]
                                .as_u64()
                                .is_some_and(|n| n <= args.max_requests as u64)
                                && value["builds"] == 1
                                && value["configures"] == 1,
                            "retained app initialization mismatch",
                        )?,
                        "b" | "custom-b" => require(
                            value["builds"] == value["ordinal"]
                                && value["configures"] == value["ordinal"],
                            "per-request initialization mismatch",
                        )?,
                        _ => require(
                            value["ordinal"] == 1 && value["builds"] == 1,
                            "single-request initialization mismatch",
                        )?,
                    }
                    seen.insert(value["instance"].as_str().unwrap().into(), value.clone());
                    records.push(annotate(
                        reply,
                        json!({"variant":variant,"repetition":repetition,"guest":value}),
                    ))?;
                }
                if ["b", "c", "custom-b", "custom-c"].contains(variant) && !reused {
                    records.push(json!({"status":"unverified","variant":variant,"repetition":repetition,"reason":"no actual guest reuse observed"}))?;
                }
                if ["c", "custom-c"].contains(variant) {
                    thread::sleep(Duration::from_millis(800));
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
                if !variant.starts_with("custom-") {
                    require(
                        guest(&req(port, "/bindings")?)?
                            == json!({"config":true,"kv":true,"secrets":true,"unknown":false}),
                        "binding mismatch",
                    )?;
                }
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
                    await_evidence(|| {
                        validate_post_send(&token, &backend.receipts.lock().unwrap(), &logs(&log))
                    })?;
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
                    records.push(annotate(large,json!({"variant":variant,"repetition":repetition,"assertion":"large_progressive_stream"})))?;
                    let failure_token = crate::token();

                    let failure = net::request(
                        port,
                        "/stream-error",
                        &failure_token,
                        Some(&backend.url(&format!("/abort?token={failure_token}"))),
                        None,
                    )
                    .expect_err("interrupted stream unexpectedly completed");
                    let partial = failure.downcast_ref::<net::InterruptedResponse>().ok_or(
                        "stream failure occurred before response body: not post-commit evidence",
                    )?;
                    require(
                        partial.status == 200 && partial.body == b"partial",
                        "missing partial committed response",
                    )?;
                    await_evidence(|| {
                        require(
                            logs(&log).iter().any(|e| {
                                e["event"] == "post_commit_error" && e["token"] == failure_token
                            }),
                            "missing correlated post-commit error",
                        )
                    })?;
                    records.push(json!({"event":"expected_stream_error","variant":variant,"repetition":repetition,"token":failure_token,"status_code":partial.status,"partial_body":String::from_utf8_lossy(&partial.body),"source":"controlled_backend_abort"}))?;
                }
                if args.suite == "smoke" {
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
                    if variant.starts_with("custom-") {
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
                            failed_guest = logs(&log)
                                .iter()
                                .find(|e| e["event"] == "request_start" && e["token"] == token)
                                .map(|e| e["instance"].clone())
                                .ok_or("missing panic attempt")?;
                            Ok(())
                        })?;
                        require(value["instance"] != failed_guest, "terminated guest reused")?;
                        records.push(annotate(fresh,json!({"variant":variant,"repetition":repetition,"assertion":"terminated_guest_not_reused","failed_guest":failed_guest})))?;
                    }
                }
                if variant.starts_with("limit-") {
                    await_evidence(|| validate_limit_summaries(&logs(&log)))?;
                }
                records.push(json!({"status":"pass","variant":variant,"repetition":repetition,"assertions":["initialization","isolation","cookies","rendered_error"]}))?;
                Ok(())
            })();
            drop(process);
            records.collect_clients(json!({"variant":variant,"repetition":repetition}))?;
            for event in logs(&log) {
                records.push(annotate(
                    event,
                    json!({"variant":variant,"repetition":repetition,"repetition":repetition}),
                ))?;
            }
            outcome?;
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
    for variant in ["b", "c", "logger-negative"] {
        let port = net::port()?;
        let log = run.join(format!("logging-{variant}.log"));
        let process = Process::start(&mut fastly_command(exe, &config, variant, port), &log, port)?;
        let mut replies = Vec::new();
        for _ in 0..if variant == "logger-negative" { 2 } else { 3 } {
            replies.push(req(port, "/probe/log")?);
        }
        drop(process);
        records.collect_clients(json!({"variant":format!("logging-{variant}"),"repetition":0}))?;
        for event in logs(&log) {
            records.push(annotate(
                event,
                json!({"variant":format!("logging-{variant}"),"repetition":0}),
            ))?;
        }
        records.push(json!({"event":"logging_check","variant":variant,"statuses":replies.iter().map(|r|r["status"].clone()).collect::<Vec<_>>(),"log_file":log.file_name()}))?;
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
    }
    Ok(())
}
pub fn provider(name: &str, args: &Args, run: &Path, records: &mut Records) -> Result<()> {
    let package = root().join("crates").join(format!("fixture-{name}"));
    let exe = if name == "axum" {
        checked(
            Command::new("cargo")
                .args(["build", "--locked", "-p", "fixture-axum"])
                .current_dir(root()),
        )?;
        root().join("target/debug/fixture-axum")
    } else {
        let (variable, tool) = if name == "cloudflare" {
            ("WRANGLER_BIN", "wrangler")
        } else {
            ("SPIN_BIN", "spin")
        };
        let Some(exe) = executable(variable, tool) else {
            return records.push(json!({"status":"unsupported","adapter":name,"reason":format!("{tool} unavailable")}));
        };
        records.push(json!({"event":"runtime","adapter":name,"executable":exe,"version":String::from_utf8_lossy(&Command::new(&exe).arg("--version").output()?.stdout).trim()}))?;
        if name == "spin" {
            let help = Command::new(&exe).args(["up", "--help"]).output()?;
            fs::write(run.join("spin-up-help.txt"), &help.stdout)?;
            records.push(json!({"event":"runtime_controls","adapter":"spin","selected":"host defaults","source":"spin-up-help.txt"}))?;

            checked(
                Command::new("cargo")
                    .args([
                        "build",
                        "--locked",
                        "--release",
                        "-p",
                        "fixture-spin",
                        "--target",
                        "wasm32-wasip2",
                    ])
                    .current_dir(root()),
            )?;
        } else {
            let Some(builder) = executable("WORKER_BUILD_BIN", "worker-build") else {
                return records
                    .push(json!({"status":"unsupported","reason":"worker-build unavailable"}));
            };
            checked(
                Command::new(builder)
                    .args(["--release", ".", "--", "--locked"])
                    .current_dir(&package),
            )?;
        }
        exe
    };
    let built = match name {
        "axum" => root().join("target/debug/fixture-axum"),
        "spin" => root().join("target/wasm32-wasip2/release/fixture_spin.wasm"),
        _ => package.join("build/index_bg.wasm"),
    };
    records.push(json!({"event":"build","adapter":name,"artifact":artifact(&built)?}))?;
    if name != "axum" {
        let filename = if name == "spin" {
            "spin.toml"
        } else {
            "wrangler.toml"
        };
        fs::copy(package.join(filename), run.join(filename))?;
    }
    let modes = if name == "axum" {
        vec!["retained"]
    } else {
        vec!["per-request", "retained"]
    };
    for mode in modes {
        let port = net::port()?;
        let mut command = Command::new(&exe);
        command
            .current_dir(&package)
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
                Command::new(&exe)
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
                    .current_dir(&package),
            )?;
            command
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
                    "../../target/",
                    &format!("{}/", root().join("target").display()),
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
        let log = run.join(format!("{name}-{mode}.log"));

        let process = Process::start(&mut command, &log, port)?;
        let outcome = (|| -> Result<()> {
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
                for _ in 0..requests {
                    let token = token();
                    let reply = net::request(port, &format!("/probe/{token}"), &token, None, None)?;
                    let value = guest(&reply)?;
                    require(
                        value["token"] == token && value["path"] == token && value["body"] == token,
                        "probe request isolation mismatch",
                    )?;
                    if mode == "retained" {
                        require(
                            value["builds"] == 1 && value["configures"] == 1,
                            "retained initialization mismatch",
                        )?;
                    }
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
                        json!({"guest":value,"adapter":name,"mode":mode,"repetition":repetition}),
                    ))?;
                }
            }
            if !reused {
                records.push(json!({"status":"unverified","adapter":name,"mode":mode,"reason":"no same-guest sequential reuse"}))?;
            }
            if mode == "retained" && name != "axum" {
                let failed = req(port, "/binding-failure")?;
                require(
                    failed["status"] == 503,
                    "injected required binding did not fail",
                )?;
                let failed_guest: Value = serde_json::from_str(failed["body"].as_str().unwrap())?;
                let recovered = guest(&req(port, "/probe/after-binding-failure")?)?;
                require(
                    recovered["builds"] == 1,
                    "app rebuilt after binding failure",
                )?;
                records.push(json!({"adapter":name,"mode":mode,"status":if failed_guest["instance"] == recovered["instance"] {"pass"} else {"unverified"},"assertion":"same_guest_binding_recovery","source":"injected_required_binding"}))?;
            }
            let bindings = req(port, "/bindings")?;

            require(
                guest(&bindings)?
                    == json!({"config":true,"kv":true,"secrets":true,"unknown":false}),
                "binding mismatch",
            )?;
            records.push(annotate(
                bindings,
                json!({"adapter":name,"mode":mode,"assertion":"binding_reads"}),
            ))?;
            let cookies = req(port, "/cookies")?;
            if cookie_values(&cookies["headers"]) != vec!["first=1; Path=/", "second=2; Path=/"] {
                records.push(annotate(cookies,json!({"status":"fail","adapter":name,"mode":mode,"reason":"duplicate Set-Cookie values were not preserved"})))?;
            }
            require(
                req(port, "/rendered-error")?["status"] == 400,
                "rendered error mismatch",
            )?;
            if name != "axum" {
                require(
                    req(port, "/other")?["body"] == "other-app",
                    "alternate app mismatch",
                )?;
            }
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
                                net::request(
                                    port,
                                    &format!("/overlap/{token}"),
                                    token,
                                    Some(&url),
                                    None,
                                )
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
            records.push(json!({"status":if observed_overlap {"pass"} else {"unverified"},"adapter":name,"mode":mode,"assertion":"same_guest_overlap","attempt_limit":3}))?;

            Ok(())
        })();
        drop(process);
        records.collect_clients(json!({"adapter":name,"mode":mode}))?;
        for event in logs(&log) {
            records.push(annotate(event, json!({"adapter":name,"mode":mode})))?;
        }
        outcome?;
    }
    Ok(())
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
        let port = net::port()?;
        let log = run.join(format!("fault-{index}.log"));
        let process = Process::start(
            &mut fastly_command(exe, &config, "faults", port),
            &log,
            port,
        )?;
        let outcome = (|| -> Result<()> {
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
            let reply = net::request(port, path, &failed_token, None, None)?;
            let recoverable = *path == "/recoverable-store-error";
            require(
                reply["status"] == if recoverable { 503 } else { 500 },
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
            if recoverable {
                let status = if value["instance"] == failed_guest {
                    "pass"
                } else {
                    "unverified"
                };
                require(
                    value["builds"] == 1,
                    "retained app rebuilt after handled store failure",
                )?;
                require(
                    guest(&req(port, "/bindings")?)?
                        == json!({"config":true,"kv":true,"secrets":true,"unknown":false}),
                    "bindings failed to recover",
                )?;
                records.push(json!({"status":status,"assertion":"injected_selector_failure_recovery","instance":failed_guest,"source":"custom_callback_handles_real_registry_error"}))?;
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
        drop(process);
        records.collect_clients(json!({"variant":"faults","repetition":index}))?;
        for event in logs(&log) {
            records.push(annotate(
                event,
                json!({"variant":"faults","repetition":index}),
            ))?;
        }
        outcome?;
    }
    Ok(())
}
