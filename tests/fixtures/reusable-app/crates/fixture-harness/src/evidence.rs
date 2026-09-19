use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}
pub fn validate_logging(guests: &[Value], delivered: &[String]) -> Result<&'static str> {
    let mut seen = HashMap::new();
    let mut reused = false;
    for guest in guests {
        if let Some(previous) = seen.insert(guest["instance"].to_string(), guest) {
            require(
                guest["ordinal"].as_u64() > previous["ordinal"].as_u64(),
                "logging ordinal did not increase",
            )?;
            require(
                guest["correlation"] != previous["correlation"],
                "logging correlation repeated",
            )?;
            reused = true;
        }
    }
    if !reused {
        return Ok("unverified");
    }
    require(
        guests.iter().all(|g| {
            delivered
                .iter()
                .any(|d| g["correlation"].as_str() == Some(d))
        }),
        "missing correlated endpoint receipt",
    )?;
    Ok("pass")
}
pub fn validate_negative_logging(replies: &[Value], starts: &[Value]) -> Result<&'static str> {
    let guests: HashMap<_, _> = starts
        .iter()
        .map(|v| (v["token"].to_string(), v["instance"].to_string()))
        .collect();
    let mut seen = HashSet::new();
    let mut reused = false;
    for reply in replies {
        let guest = guests
            .get(&reply["token"].to_string())
            .ok_or("missing negative logger request_start")?;
        let repeated = !seen.insert(guest);
        require(
            reply["status"] == if repeated { 500 } else { 200 },
            "unexpected negative logger status",
        )?;
        reused |= repeated;
    }
    Ok(if reused { "pass" } else { "unverified" })
}
pub fn validate_post_send(token: &str, receipts: &[String], events: &[Value]) -> Result<()> {
    require(
        receipts.contains(&format!("/post-send?token={token}")),
        "missing post-send backend receipt",
    )?;
    require(
        events
            .iter()
            .any(|e| e["event"] == "guest_completed" && e["token"] == token),
        "missing guest completion",
    )
}
pub fn validate_limit_summaries(events: &[Value]) -> Result<()> {
    let mut attempts = HashMap::new();
    for event in events.iter().filter(|e| e["event"] == "request_start") {
        *attempts
            .entry(event["instance"].to_string())
            .or_insert(0u64) += 1;
    }
    let summaries: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "sdk_summary")
        .collect();
    require(!attempts.is_empty(), "missing attempts")?;
    require(
        summaries.len() == attempts.len(),
        "missing or duplicate limit summaries",
    )?;
    let mut seen = HashSet::new();
    for summary in summaries {
        let instance = summary["instance"].to_string();
        require(seen.insert(instance.clone()), "duplicate summary instance")?;
        require(
            attempts.get(&instance) == Some(&1) && summary["attempted"] == 1,
            "limit summary attempts do not match",
        )?;
    }
    Ok(())
}
pub fn validate_overlap(left: &Value, right: &Value) -> Result<()> {
    require(
        left["instance"] == right["instance"],
        "different guests do not prove overlap",
    )?;
    require(
        left["max_inflight"]
            .as_u64()
            .unwrap_or(0)
            .min(right["max_inflight"].as_u64().unwrap_or(0))
            >= 2,
        "callbacks did not overlap",
    )
}
#[cfg(test)]
pub fn metric_delta(before: Option<u64>, after: Option<u64>) -> Result<Option<u64>> {
    match (before, after) {
        (Some(a), Some(b)) => Ok(Some(b.checked_sub(a).ok_or("negative metric delta")?)),
        _ => Ok(None),
    }
}
pub fn cookie_values(headers: &Value) -> Vec<&str> {
    headers
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| {
            h[0].as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("set-cookie"))
        })
        .filter_map(|h| h[1].as_str())
        .collect()
}
pub fn attempt_counts(events: &[Value]) -> (usize, usize) {
    (
        events
            .iter()
            .filter(|e| e["event"] == "request_start")
            .count(),
        events
            .iter()
            .filter(|e| e["event"] == "client_completed")
            .count(),
    )
}
fn quantiles(mut values: Vec<u64>) -> Value {
    values.sort_unstable();
    let mut result = json!({});
    for p in [50usize, 95, 99] {
        result[format!("p{p}")] = json!(values[(p * values.len()).div_ceil(100).saturating_sub(1)]);
    }
    result
}
fn phase_delta(before: Option<f64>, after: Option<f64>) -> Value {
    match (before, after) {
        (Some(a), Some(b)) if a >= 0.0 && b >= a => {
            json!({"status":"observed", "value":b-a})
        }
        (Some(_), Some(_)) => json!({"status":"invalid", "value":null}),
        _ => json!({"status":"unknown", "value":null}),
    }
}
fn variant(record: &Value) -> String {
    record["variant"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{}:{}",
                record["adapter"].as_str().unwrap_or("unknown"),
                record["mode"].as_str().unwrap_or("unknown")
            )
        })
}
fn memory_analysis(events: &[&Value], phase: &str) -> Value {
    let snapshots = events
        .iter()
        .flat_map(|e| {
            ["heap_mib", "heap_before_mib", "heap_after_mib"]
                .into_iter()
                .filter_map(|key| e[key].as_f64().filter(|v| *v >= 0.0))
        })
        .collect::<Vec<_>>();
    let mut cleanup = BTreeMap::new();
    let mut invalid_cleanup = false;
    for event in events.iter().filter(|e| e["event"] == phase) {
        match (event["ordinal"].as_u64(), event["heap_mib"].as_f64()) {
            (Some(ordinal), Some(heap)) if heap >= 0.0 => {
                invalid_cleanup |= cleanup.insert(ordinal, heap).is_some();
            }
            _ => invalid_cleanup = true,
        }
    }
    let points = cleanup.into_iter().collect::<Vec<_>>();
    let slope = if points.len() >= 2 && !invalid_cleanup {
        let n = points.len() as f64;
        let x = points.iter().map(|(x, _)| *x as f64).sum::<f64>() / n;
        let y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
        Some(
            points
                .iter()
                .map(|(a, b)| (*a as f64 - x) * (b - y))
                .sum::<f64>()
                / points
                    .iter()
                    .map(|(a, _)| (*a as f64 - x).powi(2))
                    .sum::<f64>(),
        )
    } else {
        None
    };
    let change = if points.len() >= 2 && !invalid_cleanup {
        Some(points.last().unwrap().1 - points[0].1)
    } else {
        None
    };
    let mut plateau = json!({"status":"unknown", "window_samples":3});
    if points.len() >= 3 && !invalid_cleanup {
        let tail = &points[points.len() - 3..];
        plateau = json!({"status":if tail.iter().all(|(_, heap)| *heap == tail[0].1) {
            "observed" } else { "not_observed" }, "window_samples":3,
            "first_ordinal":tail[0].0, "last_ordinal":tail[2].0});
    }
    json!({"sample_phase":phase,"source":"SDK host-inclusive rounded MiB snapshots",
        "scope":"observed request span only; rounded samples cannot establish absence of leaks; not guest linear-memory high-water or host RSS",
        "snapshot_samples":snapshots.len(),
        "peak_observed_mib":snapshots.into_iter().reduce(f64::max),
        "samples":points.len(), "samples_invalid_or_missing":invalid_cleanup,
        "samples_by_ordinal":points.iter().map(|(ordinal,heap)| json!({"ordinal":ordinal,"heap_mib":heap})).collect::<Vec<_>>(),
        "slope_mib_per_ordinal":slope, "slope_method":"least squares over observed phase ordinals",
        "change_mib":change, "plateau":plateau})
}
fn guest_analysis(records: &[Value]) -> Vec<Value> {
    let mut guests: BTreeMap<(String, String, String), Vec<&Value>> = BTreeMap::new();
    for event in records {
        if !event["instance"].is_null() {
            guests
                .entry((
                    variant(event),
                    event["repetition"].to_string(),
                    event["instance"].to_string(),
                ))
                .or_default()
                .push(event);
        }
    }
    guests.into_iter().map(|((variant, _, _), events)| {
        let count = |name: &str| events.iter().filter(|e| e["event"] == name).count();
        let summaries = events.iter().filter(|e| e["event"] == "sdk_summary").collect::<Vec<_>>();
        let attempted = if summaries.len() == 1 { summaries[0]["attempted"].as_u64() } else { None };
        let initializations = events.iter().filter(|e| e["event"] == "initialization")
            .map(|e| json!({"ordinal":e["ordinal"],"token":e["token"],"wall_ns":e["wall_ns"],
                "cpu_ms":phase_delta(e["cpu_before_ms"].as_f64(),e["cpu_after_ms"].as_f64()),
                "heap_before_mib":e["heap_before_mib"],"heap_after_mib":e["heap_after_mib"]}))
            .collect::<Vec<_>>();
        let mut requests: BTreeMap<(u64,String),Vec<&Value>> = BTreeMap::new();
        for event in &events {
            if let (Some(ordinal), Some(token)) = (event["ordinal"].as_u64(),event["token"].as_str()) {
                requests.entry((ordinal,token.to_owned())).or_default().push(event);
            }
        }
        let requests = requests.into_iter().map(|((ordinal,token),rows)| {
            let phase = |name: &str| {
                let matches = rows.iter().filter(|e| e["event"] == name).collect::<Vec<_>>();
                if matches.len() == 1 { matches[0]["cpu_ms"].as_f64() } else { None }
            };
            json!({"ordinal":ordinal,"token":token,
                "cpu_to_conversion_ms":phase_delta(phase("request_start"),phase("conversion_completed")),
                "cpu_to_commit_ms":phase_delta(phase("request_start"),phase("response_committed")),
                "cpu_after_commit_ms":phase_delta(phase("response_committed"),phase("guest_completed")),
                "cpu_request_ms":phase_delta(phase("request_start"),phase("guest_completed")),
                "response_committed":if rows.iter().any(|e| e["event"] == "response_committed") {json!(true)} else {Value::Null},
                "guest_completed":if rows.iter().any(|e| e["event"] == "guest_completed") {json!(true)} else {Value::Null},
                "terminal_error":rows.iter().any(|e| e["event"] == "terminal_error")})
        }).collect::<Vec<_>>();
        let client_completions = records.iter().filter(|r| r["event"] == "client_completed"
            && crate::evidence::variant(r) == variant && r["repetition"] == events[0]["repetition"]
            && (r["instance"] == events[0]["instance"] || r["guest"]["instance"] == events[0]["instance"]
                || requests.iter().any(|request| r["token"] == request["token"]))).count();
        json!({"variant":variant,"repetition":events[0]["repetition"],"instance":events[0]["instance"],
            "request_starts":count("request_start"),"response_commitments":count("response_committed"),
            "guest_completions":count("guest_completed"),"terminal_errors":count("terminal_error"),
            "client_completions":client_completions,"sdk_summary_records":summaries.len(),"sdk_attempted":attempted,
            "attempt_reconciliation":match attempted {Some(n) if n == count("request_start") as u64 => "match", Some(_) => "mismatch", None => "unknown"},
            "initialization_count":initializations.len(),"initialization":initializations,"requests":requests,
            "cpu_scope":"SDK samples between named observation points: standard request_start is after initialization; custom request_start is before initialization. Concurrent work may contribute. Not total request CPU or cross-run performance evidence.",
            "memory":memory_analysis(&events,"guest_completed"),
            "conversion_memory":memory_analysis(&events,"conversion_completed")})
    }).collect()
}
pub fn summarize(records: &[Value]) -> Value {
    let mut builds = BTreeMap::new();
    let mut mixed_builds = false;
    for record in records.iter().filter(|r| r["event"] == "build") {
        let key = (variant(record), record["repetition"].to_string());
        if let Some(previous) = builds.insert(key, record["artifact"]["fnv1a64"].clone()) {
            mixed_builds |= previous != record["artifact"]["fnv1a64"];
        }
    }
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    let mut memory: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for r in records {
        if r["event"] == "client_completed" && r.get("guest").is_some() {
            let cohort = if r["guest"]["ordinal"] == 1 {
                "cold"
            } else {
                "reused"
            };
            let variant = r["variant"].as_str().map(str::to_owned).unwrap_or_else(|| {
                format!(
                    "{}:{}",
                    r["adapter"].as_str().unwrap_or("unknown"),
                    r["mode"].as_str().unwrap_or("unknown")
                )
            });
            groups
                .entry(format!("{variant}:probe:{cohort}"))
                .or_default()
                .push(r);
        }
        if let Some(heap) = r["heap_mib"].as_u64() {
            memory
                .entry(r["variant"].as_str().unwrap_or("unknown").to_owned())
                .or_default()
                .push(heap);
        }
    }
    let mut result = json!({"quantile_method":"nearest rank", "variants":{}, "memory_snapshots":{}, "cpu_comparison":"SDK phase readings only; cross-run performance unverified"});
    for (key, rows) in groups {
        let values = rows
            .iter()
            .filter_map(|r| r["complete_ns"].as_u64())
            .collect::<Vec<_>>();
        if values.is_empty() {
            continue;
        }
        result["variants"][&key] =
            json!({"samples":values.len(), "completion_ns":quantiles(values)});
        let first = rows
            .iter()
            .filter_map(|r| r["first_byte_ns"].as_u64())
            .collect::<Vec<_>>();
        if !first.is_empty() {
            result["variants"][&key]["first_byte_ns"] = quantiles(first);
        }
    }
    for (key, values) in memory {
        result["memory_snapshots"][key] = json!({"samples":values.len(),"min_mib":values.iter().min(),"max_mib":values.iter().max(),"source":"SDK host-inclusive rounded snapshot"});
    }
    result["build_identity"] = json!({"status":if mixed_builds {"invalid"} else if builds.is_empty() {"unknown"} else {"consistent"}});
    let (attempts, completions) = attempt_counts(records);
    result["guests"] = json!(guest_analysis(records));
    result["recorded_request_starts"] = json!(attempts);
    result["recorded_client_completions"] = json!(completions);
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_observation_is_unknown_and_mixed_builds_are_rejected() {
        let summary = summarize(&[
            json!({"event":"request_start","variant":"c","instance":"one","ordinal":1,"token":"a"}),
            json!({"event":"build","variant":"c","repetition":0,"artifact":{"fnv1a64":"a"}}),
            json!({"event":"build","variant":"c","repetition":0,"artifact":{"fnv1a64":"b"}}),
        ]);
        assert!(summary["guests"][0]["requests"][0]["response_committed"].is_null());
        assert_eq!(summary["build_identity"]["status"], "invalid");
    }
    fn measured(event: &str, ordinal: u64, cpu: u64, heap: u64) -> Value {
        json!({"event":event,"variant":"C","repetition":1,"instance":"same",
            "ordinal":ordinal,"token":format!("t{ordinal}"),"cpu_ms":cpu,"heap_mib":heap})
    }
    #[test]
    fn per_guest_phases_attempts_and_memory_are_bounded_observations() {
        let mut events = vec![
            json!({"event":"initialization","variant":"C","repetition":1,
            "instance":"same","ordinal":1,"token":"t1","cpu_before_ms":1,"cpu_after_ms":4,
            "heap_before_mib":5,"heap_after_mib":8}),
        ];
        for ordinal in 1..=4 {
            events.push(measured("request_start", ordinal, ordinal * 10, 8));
            events.push(measured(
                "response_committed",
                ordinal,
                ordinal * 10 + 2,
                10,
            ));
            events.push(measured("guest_completed", ordinal, ordinal * 10 + 5, 9));
        }
        events.push(json!({"event":"sdk_summary","variant":"C","repetition":1,"instance":"same","attempted":5}));
        let summary = summarize(&events);
        let guest = &summary["guests"][0];
        assert_eq!(guest["request_starts"], 4);
        assert_eq!(guest["guest_completions"], 4);
        assert_eq!(guest["sdk_attempted"], 5);
        assert_eq!(guest["attempt_reconciliation"], "mismatch");
        assert_eq!(guest["initialization"][0]["cpu_ms"]["value"], 3.0);
        assert_eq!(guest["requests"][0]["cpu_to_commit_ms"]["value"], 2.0);
        assert_eq!(guest["requests"][0]["cpu_after_commit_ms"]["value"], 3.0);
        assert_eq!(guest["memory"]["slope_mib_per_ordinal"], 0.0);
        assert_eq!(guest["memory"]["peak_observed_mib"], 10.0);
        assert_eq!(guest["memory"]["change_mib"], 0.0);
        assert_eq!(guest["memory"]["plateau"]["status"], "observed");
        assert_eq!(guest["memory"]["plateau"]["first_ordinal"], 2);
        assert_eq!(guest["memory"]["plateau"]["last_ordinal"], 4);
    }
    #[test]
    fn missing_and_decreasing_cpu_are_not_zero_or_success() {
        let mut events = vec![
            measured("request_start", 1, 10, 8),
            measured("response_committed", 1, 9, 8),
        ];
        events.push(measured("terminal_error", 1, 11, 8));
        let summary = summarize(&events);
        let guest = &summary["guests"][0];
        assert!(guest["sdk_attempted"].is_null());
        assert_eq!(guest["attempt_reconciliation"], "unknown");
        assert_eq!(guest["terminal_errors"], 1);
        assert_eq!(guest["guest_completions"], 0);
        assert_eq!(
            guest["requests"][0]["cpu_to_commit_ms"]["status"],
            "invalid"
        );
        assert_eq!(
            guest["requests"][0]["cpu_after_commit_ms"]["status"],
            "unknown"
        );
        assert!(guest["requests"][0]["cpu_to_commit_ms"]["value"].is_null());
        assert_eq!(guest["memory"]["plateau"]["status"], "unknown");
    }
    #[test]
    fn guest_identity_includes_variant_and_repetition_and_slope_uses_ordinals() {
        let mut events = vec![
            measured("guest_completed", 1, 1, 10),
            measured("guest_completed", 3, 2, 14),
        ];
        let mut other = events[0].clone();
        other["repetition"] = json!(2);
        events.push(other.clone());
        other["variant"] = json!("B");
        events.push(other);
        let summary = summarize(&events);
        let guests = summary["guests"].as_array().unwrap();
        assert_eq!(guests.len(), 3);
        let guest = guests
            .iter()
            .find(|g| g["variant"] == "C" && g["repetition"] == 1)
            .unwrap();
        assert_eq!(guest["memory"]["slope_mib_per_ordinal"], 2.0);
        assert_eq!(guest["memory"]["change_mib"], 4.0);
        assert_eq!(guest["memory"]["plateau"]["status"], "unknown");
    }
    #[test]
    fn conversion_samples_do_not_claim_guest_cleanup_or_commitment() {
        let events = vec![
            measured("conversion_completed", 1, 5, 8),
            measured("conversion_completed", 2, 6, 10),
        ];
        let summary = summarize(&events);
        let guest = &summary["guests"][0];
        assert_eq!(
            guest["conversion_memory"]["sample_phase"],
            "conversion_completed"
        );
        assert_eq!(guest["conversion_memory"]["slope_mib_per_ordinal"], 2.0);
        assert_eq!(guest["memory"]["samples"], 0);
        assert_eq!(guest["response_commitments"], 0);
        assert_eq!(guest["guest_completions"], 0);
        assert_eq!(guest["requests"][0]["cpu_request_ms"]["status"], "unknown");
    }
    #[test]
    fn failed_attempt_is_not_completion() {
        assert_eq!(
            attempt_counts(&[json!({"event":"request_start"}), json!({"event":"error"})]),
            (1, 0)
        );
    }
    #[test]
    fn unavailable_metrics() {
        assert_eq!(metric_delta(None, Some(4)).unwrap(), None);
        assert!(metric_delta(Some(5), Some(4)).is_err());
    }
    #[test]
    fn duplicate_cookies() {
        assert_eq!(
            cookie_values(&json!([["Set-Cookie", "a=1"], ["Set-Cookie", "b=2"]])),
            vec!["a=1", "b=2"]
        );
    }
    #[test]
    fn overlap_requires_same_guest_and_concurrency() {
        let left = json!({"instance":"a","max_inflight":2});
        for right in [
            json!({"instance":"b","max_inflight":2}),
            json!({"instance":"a","max_inflight":1}),
        ] {
            assert!(validate_overlap(&left, &right).is_err());
        }
        validate_overlap(&left, &left).unwrap();
    }
    #[test]
    fn unmatched_workloads_excluded() {
        let s = summarize(&[
            json!({"event":"client_completed","variant":"c","guest":{"ordinal":1},"complete_ns":10}),
            json!({"event":"client_completed","variant":"c","guest":{"ordinal":2},"complete_ns":20}),
            json!({"event":"client_completed","variant":"c","assertion":"idle","complete_ns":999}),
        ]);
        assert_eq!(s["variants"]["c:probe:cold"]["completion_ns"]["p50"], 10);
        assert_eq!(s["variants"]["c:probe:reused"]["completion_ns"]["p50"], 20);
    }
    #[test]
    fn provider_modes_separate() {
        let records=["retained","per-request"].map(|mode|json!({"event":"client_completed","adapter":"spin","mode":mode,"guest":{"ordinal":2},"complete_ns":10}));
        assert_eq!(
            summarize(&records)["variants"].as_object().unwrap().len(),
            2
        );
    }
    #[test]
    fn logging_requires_reuse_and_correlated_delivery() {
        let cold = (0..3)
            .map(|i| json!({"instance":i.to_string(),"ordinal":1,"correlation":i.to_string()}))
            .collect::<Vec<_>>();
        let delivered = (0..3).map(|i| i.to_string()).collect::<Vec<_>>();
        assert_eq!(validate_logging(&cold, &delivered).unwrap(), "unverified");
        let warm = (0..3)
            .map(|i| json!({"instance":"same","ordinal":i+1,"correlation":i.to_string()}))
            .collect::<Vec<_>>();
        assert!(validate_logging(&warm, &["0".into(), "0".into(), "0".into()]).is_err());
        assert_eq!(validate_logging(&warm, &delivered).unwrap(), "pass");
    }
    #[test]
    fn negative_logger_eviction() {
        let mut starts = vec![
            json!({"instance":"one","token":"a"}),
            json!({"instance":"two","token":"b"}),
        ];
        let mut replies = vec![
            json!({"token":"a","status":200}),
            json!({"token":"b","status":200}),
        ];
        assert_eq!(
            validate_negative_logging(&replies, &starts).unwrap(),
            "unverified"
        );
        starts[1]["instance"] = json!("one");
        assert!(validate_negative_logging(&replies, &starts).is_err());
        replies[1]["status"] = json!(500);
        assert_eq!(
            validate_negative_logging(&replies, &starts).unwrap(),
            "pass"
        );
    }
    #[test]
    fn post_send_requires_both_receipts() {
        assert!(validate_post_send("x", &[], &[]).is_err());
        let receipts = vec!["/post-send?token=x".into()];
        assert!(validate_post_send("x", &receipts, &[]).is_err());
        validate_post_send(
            "x",
            &receipts,
            &[json!({"event":"guest_completed","token":"x"})],
        )
        .unwrap();
    }
    #[test]
    fn limit_summaries_match_attempts() {
        let mut events = vec![json!({"event":"request_start","instance":"one"})];
        assert!(validate_limit_summaries(&events).is_err());
        for count in [0, 2] {
            events.truncate(1);
            events.push(json!({"event":"sdk_summary","instance":"one","attempted":count}));
            assert!(validate_limit_summaries(&events).is_err());
        }
        events[1]["attempted"] = json!(1);
        validate_limit_summaries(&events).unwrap();
        events[1]["instance"] = json!("other");
        assert!(validate_limit_summaries(&events).is_err());
        events[1]["instance"] = json!("one");
        events.push(events[1].clone());
        assert!(validate_limit_summaries(&events).is_err());
        events.push(json!({"event":"request_start","instance":"two"}));
        assert!(validate_limit_summaries(&events).is_err());
    }
}
