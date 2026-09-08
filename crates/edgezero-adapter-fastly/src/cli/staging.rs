use std::env;
use std::io::{ErrorKind, Write as _};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::service_scoped_runtime_env_key;

use super::provision_cloud::looks_like_already_exists;
use super::push_cloud::{
    find_config_store_id, redact_describe_response, redact_stderr, strict_stdout,
};
use super::run::find_fastly_manifest;
use super::{ConfigStoreLookup, FASTLY_INSTALL_HINT, RUNTIME_ENV_STORE_NAME};

/// Base name of the staging twin of [`RUNTIME_ENV_STORE_NAME`]. The actual store is
/// named PER SERVICE — [`staging_selector_store_name`] appends the service id —
/// because Fastly config stores are account-wide, versionless resources: a
/// single shared twin would let a staged deploy of service B destructively
/// overwrite the selectors a staged version of service A is reading.
///
/// A staged deploy clones the active version, and a clone inherits its resource
/// links — so without a second store the staged version reads production's
/// selector, and therefore production's config. Fastly resource links are
/// per-version and carry an overridable NAME, so the staged draft links THIS
/// store under the name `edgezero_runtime_env`. The runtime opens that name and
/// gets staged config; the active version is untouched.
const RUNTIME_ENV_STAGING_STORE_PREFIX: &str = "edgezero_runtime_env_staging";

/// Env var carrying the Fastly API token (read by the Fastly CLI and
/// forwarded to the Fastly API via the `Fastly-Key` header).
const FASTLY_API_TOKEN_ENV: &str = "FASTLY_API_TOKEN";
/// Env var carrying the default Fastly service id, used when
/// `--service-id` is not passed explicitly.
const FASTLY_SERVICE_ID_ENV: &str = "FASTLY_SERVICE_ID";

/// Bound every Fastly API call so an outage or a stalled connection cannot hang the
/// job — potentially until the surrounding workflow timeout, hours later — during a
/// time-sensitive operation like a rollback. `curl` exits 28 when either limit is
/// hit, which `curl_config_capture` turns into an explicit timeout error.
const FASTLY_API_CONNECT_TIMEOUT_SECS: u64 = 10;
const FASTLY_API_MAX_TIME_SECS: u64 = 30;
/// curl's exit code for an operation that exceeded `--connect-timeout`/`--max-time`.
const CURL_EXIT_TIMEOUT: i32 = 28;

/// Flags `fastly compute update` accepts that take a VALUE (either
/// `--flag value` or `--flag=value`). Verified against
/// `fastly compute update --help` (Fastly CLI v15): the command's
/// `--service-id`/`-s`, `--service-name`, `--package`/`-p`, `--version`,
/// plus the global `--token`/`-t`.
const COMPUTE_UPDATE_VALUE_FLAGS: &[&str] = &[
    "--service-id",
    "-s",
    "--service-name",
    "--package",
    "-p",
    "--version",
    "--token",
    "-t",
];

/// Boolean flags `fastly compute update` accepts: the command's
/// `--autoclone` plus the Fastly CLI globals. NOTE the absence of
/// `--comment` -- `compute update` does NOT support it (unlike
/// `compute deploy`), which is why an operator `--comment` is routed to
/// `service-version update` instead (see `deploy_staged`).
const COMPUTE_UPDATE_BOOL_FLAGS: &[&str] = &[
    "--autoclone",
    "--accept-defaults",
    "-d",
    "--auto-yes",
    "-y",
    "--debug-mode",
    "--non-interactive",
    "-i",
    "--quiet",
    "-q",
    "--verbose",
    "-v",
];

/// An operator passthrough arg list split for a staged deploy (see
/// `split_staged_passthrough`).
struct StagedPassthrough {
    /// The `--comment` value, applied to the version separately via
    /// `fastly service-version update --comment` (`compute update` has
    /// no `--comment` flag).
    comment: Option<String>,
    /// Args `compute update` does not support; dropped with a warning
    /// rather than forwarded (forwarding them makes the CLI exit
    /// non-zero and fails the whole staged deploy).
    dropped: Vec<String>,
    /// Args that `fastly compute update` actually supports.
    forwarded: Vec<String>,
}

fn arg_value<'args>(args: &'args [String], flag: &str) -> Option<&'args str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| idx.checked_add(1))
        .and_then(|idx| args.get(idx))
        .map(String::as_str)
}

/// Whether a boolean `flag` (e.g. `--staging`) is present in `args`.
fn arg_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

/// Copy of `args` with `--flag value` removed (both tokens). Used to
/// forward operator passthrough (e.g. `--comment`) to `fastly compute
/// update` without re-passing `--service-id`, which is threaded
/// explicitly.
fn args_without_flag_value(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == flag {
            skip = true;
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Split an arg on a leading `--flag=value`, returning `(flag, value)`.
fn split_inline_value(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with('-') => (flag, Some(value)),
        Some(_) | None => (arg, None),
    }
}

/// Partition operator passthrough args for a staged deploy: forward only
/// what `fastly compute update` supports, lift `--comment` out (it is a
/// `compute deploy` / `service-version update` flag, NOT a
/// `compute update` one), and drop the rest.
///
/// Both `--comment value` and `--comment=value` are recognised.
fn split_staged_passthrough(args: &[String]) -> StagedPassthrough {
    let mut split = StagedPassthrough {
        forwarded: Vec::with_capacity(args.len()),
        comment: None,
        dropped: Vec::new(),
    };
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let (flag, inline) = split_inline_value(arg);
        if flag == "--comment" {
            split.comment = match inline {
                Some(value) => Some(value.to_owned()),
                None => iter.next().cloned(),
            };
        } else if COMPUTE_UPDATE_VALUE_FLAGS.contains(&flag) {
            split.forwarded.push(arg.clone());
            if inline.is_none()
                && let Some(value) = iter.next()
            {
                split.forwarded.push(value.clone());
            }
        } else if COMPUTE_UPDATE_BOOL_FLAGS.contains(&flag) {
            split.forwarded.push(arg.clone());
        } else {
            // Unsupported by `compute update`. Consume a detached value
            // too, so a stray `stage` from `--env stage` is not left
            // behind as a bogus positional.
            split.dropped.push(flag.to_owned());
            if inline.is_none() && iter.peek().is_some_and(|next| !next.starts_with('-')) {
                iter.next();
            }
        }
    }
    split
}

/// Resolve the target service id from `--service-id` or, failing that,
/// `FASTLY_SERVICE_ID`.
fn resolve_service_id(args: &[String]) -> Result<String, String> {
    if let Some(value) = arg_value(args, "--service-id") {
        return Ok(value.to_owned());
    }
    env::var(FASTLY_SERVICE_ID_ENV).map_err(|_err| {
        format!("no service id: pass `--service-id <id>` or set {FASTLY_SERVICE_ID_ENV}")
    })
}

/// Read the required Fastly API token from the environment.
fn require_token() -> Result<String, String> {
    env::var(FASTLY_API_TOKEN_ENV)
        .map_err(|_err| format!("{FASTLY_API_TOKEN_ENV} must be set in the environment"))
}

/// Whether an HTTP status counts as healthy (2xx only).
///
/// A passing probe gates against an automatic rollback, so a 3xx is deliberately
/// NOT healthy: a staged version answering `301` to an error page (the probe does
/// not follow redirects) would otherwise mask a bad deploy as healthy.
fn is_healthy_status(code: u16) -> bool {
    (200..300).contains(&code)
}

/// Digits immediately following `marker` in `lower` (a lowercased
/// haystack), for the LAST occurrence of `marker`. The number must be
/// terminated by `terminator` — so a partial/confusable match (e.g. a
/// semver `15.2.0`) yields `None` rather than a bogus version.
fn last_version_after(lower: &str, marker: &str, terminator: char) -> Option<u64> {
    let mut result = None;
    for (idx, _) in lower.match_indices(marker) {
        let after = idx.saturating_add(marker.len());
        let Some(rest) = lower.get(after..) else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || rest.chars().nth(digits.len()) != Some(terminator) {
            continue;
        }
        if let Ok(parsed) = digits.parse::<u64>() {
            result = Some(parsed);
        }
    }
    result
}

/// Parse a Fastly service version out of Fastly CLI output, accepting
/// ONLY the shapes the CLI actually emits, in precedence order:
///
///   1. Our canonical `version=<N>` contract line.
///   2. The CLI's success line, whose Go format string is
///      `"Updated package (service %s, version %v)"` (and
///      `"Deployed package (...)"` for `compute deploy`) — matched as
///      `, version <N>)`. This names the version the package landed on,
///      so it wins over (3).
///   3. The `--autoclone` notice, `"... Now operating on version %d."` —
///      the freshly-cloned draft, used when the success line is absent.
///
/// Everything else yields `None` and the caller FAILS CLOSED.
///
/// Deliberately strict. A previous implementation took ANY digits
/// appearing after the word "version" and let the last match win, so:
///   * `Uploaded package to service 12345, version unchanged` parsed as
///     version 12345, and
///   * the autoclone notice's *pre-clone* version
///     (`Service version 3 is not editable...`) could beat the real one,
///     since stdout and stderr are concatenated and their relative order
///     is not guaranteed.
///
/// A misparse here stages, comments, or rolls back the WRONG service
/// version, so ambiguity must be an error, not a guess.
fn parse_fastly_version(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    parse_canonical_version_line(&lower)
        .or_else(|| last_version_after(&lower, ", version ", ')'))
        .or_else(|| last_version_after(&lower, "now operating on version ", '.'))
}

/// Last standalone `version=<N>` line (the whole trimmed line must be
/// exactly that, so a `--version=active` flag echoed in a command line
/// cannot masquerade as one).
fn parse_canonical_version_line(lower: &str) -> Option<u64> {
    lower.lines().rev().find_map(|line| {
        let digits = line.trim().strip_prefix("version=")?;
        (!digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// Resolve the active version from a Fastly version-list JSON
/// (`fastly service-version list --json`, or the Fastly API
/// `/service/<id>/version` array).
///
/// `Ok(Some(n))` — exactly one version is active. `Ok(None)` — the list parsed
/// but NO version is active (a first-ever deploy; the caller records an empty
/// rollback target and proceeds). `Err(_)` — the payload could not be parsed as
/// a version list, OR it is MALFORMED (a non-boolean `active` on ANY entry, an
/// `active: true` entry whose `number` is missing or not an unsigned integer, or
/// MORE THAN ONE active version). All are OPERATIONAL failures the caller must
/// NOT silently treat as "no active version" — otherwise a garbled or ambiguous
/// response would fail open and let a production deploy proceed with no rollback
/// target.
///
/// The ENTIRE list is scanned (not short-circuited at the first active entry) so
/// that a malformed `active` field or a second active version anywhere in the
/// response is caught rather than ignored.
fn resolve_active_version(json: &str) -> Result<Option<u64>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| format!("failed to parse the Fastly version list as JSON: {err}"))?;
    let array = value.as_array().ok_or_else(|| {
        "the Fastly version list was not a JSON array; the API may have changed its schema"
            .to_owned()
    })?;
    // A real Fastly service always has at least an initial (inactive) version, so
    // an EMPTY list is an invalid response — fail closed rather than read it as a
    // legitimate "no active version yet" (first deploy).
    if array.is_empty() {
        return Err(
            "the Fastly version list is empty; a service always has at least an initial version, so this response cannot be trusted".to_owned()
        );
    }
    let mut active_version: Option<u64> = None;
    for entry in array {
        // EVERY entry must be a well-formed version object with an unsigned
        // integer `number` — Fastly includes it on every version. A `null`, a
        // non-object, or a missing/non-integer `number` means the response
        // cannot be trusted; treating such an entry as merely "not active" would
        // let a garbled payload read as "no active version" (fail open).
        let Some(object) = entry.as_object() else {
            return Err(format!(
                "a Fastly version list element is not an object; the API may have changed its schema. Element: {entry}"
            ));
        };
        let number = object.get("number").and_then(serde_json::Value::as_u64).ok_or_else(|| {
            format!(
                "a Fastly version entry has no unsigned-integer `number`; the API may have changed its schema. Entry: {entry}"
            )
        })?;
        // `active` is optional (an omitted field means not active), but a PRESENT
        // non-boolean is schema drift.
        let active = match object.get("active") {
            None => false,
            Some(active_field) => active_field.as_bool().ok_or_else(|| {
                format!(
                    "a Fastly version entry has a non-boolean `active` field; the API may have changed its schema. Entry: {entry}"
                )
            })?,
        };
        if active {
            if active_version.is_some() {
                return Err(format!(
                    "the Fastly version list reports more than one active version ({} and {number}); the response is ambiguous, refusing to pick one",
                    active_version.unwrap_or_default()
                ));
            }
            active_version = Some(number);
        }
    }
    Ok(active_version)
}

/// Best-effort staleness guard for a production rollback: the version being
/// rolled back FROM (`from_version`, the caller's `--version`) must still be the
/// ACTIVE version. A rollback can run long after its deploy; if a newer version
/// was activated since, activating the old target would clobber it — so refuse.
///
/// This narrows but does NOT close the race: the caller reads the active version
/// and activates in two separate requests, and Fastly's activate endpoint has no
/// precondition, so a deploy landing between them can still be clobbered.
/// Service-scoped serialization is required to eliminate it.
fn ensure_rollback_from_is_active(
    active: Option<u64>,
    from_version: u64,
    service_id: &str,
) -> Result<(), String> {
    match active {
        Some(active_version) if active_version == from_version => Ok(()),
        Some(active_version) => Err(format!(
            "refusing to roll back service {service_id}: the active version is now {active_version}, not the {from_version} being rolled back from -- a newer deploy is live and rolling back would clobber it"
        )),
        None => Err(format!(
            "refusing to roll back service {service_id}: it has no active version"
        )),
    }
}

/// First staging IP found in a Fastly
/// `GET /service/<id>/version/<n>/domain?include=staging_ips` response.
///
/// The response is an ARRAY of domain objects, and the staging address
/// is a SINGULAR, nullable STRING field named `staging_ip` on each
/// domain (`staging_ips` is only the `include=` query-param value, never
/// a field name). Verified against the go-fastly `Domain` model, whose
/// field is `StagingIP` with the mapstructure tag `staging_ip`, and its
/// recorded API fixture `fixtures/domains/list_with_staging_ips.yaml`,
/// plus Fastly's "working with staging" guide. The field is absent from
/// the published Domain data model, so it is treated as optional.
///
/// We also tolerate a plural `staging_ips` array, in case a Fastly
/// response (or a future API version) carries that shape.
fn parse_staging_ip(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    find_staging_ip(&value)
}

fn find_staging_ip(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            // The documented shape: a singular `staging_ip` string.
            if let Some(ip) = map.get("staging_ip").and_then(serde_json::Value::as_str) {
                return Some(ip.to_owned());
            }
            // Tolerated: a plural `staging_ips` array of strings.
            if let Some(ip) = map
                .get("staging_ips")
                .and_then(serde_json::Value::as_array)
                .and_then(|arr| arr.iter().find_map(serde_json::Value::as_str))
            {
                return Some(ip.to_owned());
            }
            map.values().find_map(find_staging_ip)
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_staging_ip),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => None,
    }
}

/// Build the `curl` argv for a health probe. Production probes the
/// domain directly; staging reroutes the TLS connection to the
/// resolved staging IP via `--connect-to ::<ip>:443`. `path` is the
/// URL path (always begins with '/'), applied identically to both.
fn build_curl_probe_args(
    domain: &str,
    path: &str,
    staging_ip: Option<&str>,
    timeout_secs: u64,
) -> Vec<String> {
    let mut args = vec![
        // `-q` first so curl never merges `~/.curlrc` into a probe (a planted
        // `proxy`/`output` there could otherwise redirect or corrupt the check).
        "-q".to_owned(),
        "-sS".to_owned(),
        // Disable curl's URL globbing: a valid probe path may contain `[` `]` `{`
        // `}` (e.g. `/health?ids[0]=1`), which curl would otherwise treat as a
        // glob — failing with exit 3 or firing multiple requests, and so
        // mis-reporting a healthy deployment as unhealthy.
        "--globoff".to_owned(),
        "-o".to_owned(),
        "/dev/null".to_owned(),
        "-w".to_owned(),
        "%{http_code}".to_owned(),
        "--max-time".to_owned(),
        timeout_secs.to_string(),
    ];
    if let Some(ip) = staging_ip {
        // `--connect-to ::HOST:PORT` reroutes the TLS connection to the staging
        // IP. An IPv6 literal must be bracketed or curl mis-parses the colons;
        // the caller has already validated `ip` parses as an `IpAddr`.
        let target = if ip.contains(':') {
            format!("::[{ip}]:443")
        } else {
            format!("::{ip}:443")
        };
        args.push("--connect-to".to_owned());
        args.push(target);
    }
    args.push(format!("https://{domain}{path}"));
    args
}

/// Validate a caller-supplied probe path. It is appended to
/// `https://{domain}` to form one curl argument, so it must begin with
/// '/' and carry no whitespace or control characters that would break
/// the URL or smuggle a second token.
fn validate_probe_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("healthcheck --path must begin with '/': '{path}'"));
    }
    if path.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(format!(
            "healthcheck --path must not contain whitespace or control characters: '{path}'"
        ));
    }
    Ok(())
}

/// Retry a health probe. Returns `Ok(code)` on the first healthy
/// status, or `Err((last_code, message))` after exhausting attempts.
/// `between` runs between attempts (not after the last) so it can be a
/// no-op in tests.
fn probe_with_retries<P, S>(
    retry: u32,
    mut prober: P,
    mut between: S,
) -> Result<u16, (Option<u16>, String)>
where
    P: FnMut() -> Result<u16, String>,
    S: FnMut(),
{
    let attempts = retry.max(1);
    let mut last_code = None;
    let mut last_msg = "no probe attempts were made".to_owned();
    for attempt in 0..attempts {
        match prober() {
            Ok(code) if is_healthy_status(code) => return Ok(code),
            Ok(code) => {
                last_code = Some(code);
                last_msg = format!("unhealthy HTTP status {code}");
            }
            Err(err) => last_msg = err,
        }
        if attempt.saturating_add(1) < attempts {
            between();
        }
    }
    Err((last_code, last_msg))
}

/// Run `fastly <args>` in `cwd`, inheriting stdio, and map a non-zero
/// exit to an error.
fn run_fastly_status(fastly_args: &[String], cwd: &Path) -> Result<(), String> {
    let status = Command::new("fastly")
        .args(fastly_args)
        .current_dir(cwd)
        .status()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to run fastly CLI: {err}")
            }
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`fastly {}` exited with status {status}",
            fastly_args.join(" ")
        ))
    }
}

/// Run `fastly <args>` in `cwd` capturing stdout+stderr (combined) for
/// version parsing. Errors on a non-zero exit.
fn run_fastly_capture(fastly_args: &[String], cwd: &Path) -> Result<String, String> {
    let output = Command::new("fastly")
        .args(fastly_args)
        .current_dir(cwd)
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to run fastly CLI: {err}")
            }
        })?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(combined)
    } else {
        Err(format!(
            "`fastly {}` exited with status {}\n{}",
            fastly_args.join(" "),
            output.status,
            combined.trim()
        ))
    }
}

/// Run `curl -q -sS --config -`, piping `config` (which carries the
/// `Fastly-Key` header + url) through stdin so the token never touches
/// argv. Returns stdout on a zero exit.
///
/// `-q` MUST be the first argument: without it curl reads `~/.curlrc`
/// (or `$CURL_HOME/.curlrc`) and merges it into this token-bearing
/// config, so a `proxy = …` directive planted by an earlier same-job
/// build step could exfiltrate the `Fastly-Key` header. `--connect-timeout`
/// / `--max-time` bound the call.
fn curl_config_capture(config: &str) -> Result<String, String> {
    let connect_timeout = FASTLY_API_CONNECT_TIMEOUT_SECS.to_string();
    let max_time = FASTLY_API_MAX_TIME_SECS.to_string();
    let mut child = Command::new("curl")
        .args([
            "-q",
            "-sS",
            "--connect-timeout",
            &connect_timeout,
            "--max-time",
            &max_time,
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                "`curl` not found on PATH; install curl and retry".to_owned()
            } else {
                format!("failed to spawn `curl`: {err}")
            }
        })?;
    // Take stdin OUT of the child and hand it to a helper BY VALUE, so it drops at
    // that helper's scope end — a natural drop rather than an explicit `drop(stdin)`,
    // which trips `clippy::drop_non_drop` on wasm targets where `ChildStdin` is not
    // `Drop`. The drop must precede `wait_with_output` so curl sees EOF (same pattern
    // as `write_value_to_fastly_stdin` on the fastly path).
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `curl`".to_owned())?;
    write_config_to_curl_stdin(stdin, config)?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait on `curl`: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else if output.status.code() == Some(CURL_EXIT_TIMEOUT) {
        Err(format!(
            "`curl` timed out after connect-timeout {FASTLY_API_CONNECT_TIMEOUT_SECS}s / max-time {FASTLY_API_MAX_TIME_SECS}s: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    } else {
        Err(format!(
            "`curl` exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Write `config` to curl's stdin, taking the handle BY VALUE so it drops at this
/// function's scope end. That natural drop closes the pipe (curl sees EOF) without
/// an explicit `drop(stdin)`, which trips `clippy::drop_non_drop` on wasm targets
/// where `ChildStdin` is not `Drop` (mirrors `write_value_to_fastly_stdin`).
fn write_config_to_curl_stdin(mut stdin: ChildStdin, config: &str) -> Result<(), String> {
    stdin
        .write_all(config.as_bytes())
        .map_err(|err| format!("failed to write curl config to stdin: {err}"))
}

/// Wrap `value` in a curl-config double-quoted string, escaping the
/// characters that would otherwise let a value terminate its quote and
/// inject additional curl options. Within a curl `--config` file a
/// double-quoted value only honours the escapes `\\`, `\"`, `\n`, `\r`,
/// `\t` (and the config is parsed line-by-line, so a raw newline ends
/// the directive regardless of quoting). We escape backslash and quote
/// so the value cannot break out of the quotes, and map raw control
/// characters to their escape form so NO raw newline (or CR/tab) is
/// ever written into the config file. This is the second half of the
/// injection defence: untrusted identifiers are also validated (see
/// `validate_service_id` / `validate_version_str` / `validate_domain`),
/// but the token is a secret we cannot constrain to a charset, so it
/// relies on this escaping alone.
fn curl_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len().saturating_add(2));
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Validate an operator-supplied Fastly service id before it is
/// interpolated into an API URL or runtime-env key. Fastly service ids are
/// opaque alphanumeric handles, so constrain them to `^[A-Za-z0-9]+$`.
/// Values carrying a quote, newline, or space could inject curl options via
/// the `--config` file.
fn validate_service_id(id: &str) -> Result<(), String> {
    if id.contains("__") {
        return Err(format!(
            "invalid service id {id:?}: `__` is the runtime-env namespace delimiter"
        ));
    }
    if !id.is_empty() && id.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(format!(
            "invalid service id {id:?}: expected only ASCII letters or digits"
        ))
    }
}

/// Validate a service-version string is a plain non-negative integer
/// before it is interpolated into an API URL. Returns the parsed value
/// so callers can reuse it.
fn validate_version_str(version: &str) -> Result<u64, String> {
    version.parse::<u64>().map_err(|err| {
        format!("invalid version {version:?}: expected a non-negative integer: {err}")
    })
}

/// Validate a domain is a plausible hostname before it is placed into a
/// `curl` URL. Rejects anything outside the DNS label charset
/// (`[A-Za-z0-9-.]`), empty / over-long values, leading/trailing dots,
/// and empty labels so an injected quote / slash / space / newline
/// cannot smuggle curl options or a second URL.
fn validate_domain(domain: &str) -> Result<(), String> {
    let charset_ok = domain
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.');
    let shape_ok = !domain.is_empty()
        && domain.len() <= 253
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..");
    if charset_ok && shape_ok {
        Ok(())
    } else {
        Err(format!(
            "invalid domain {domain:?}: expected a hostname like `example.com`"
        ))
    }
}

/// `GET https://api.fastly.com<path>` with the `Fastly-Key` header;
/// returns the response body ONLY on a 2xx status. Both the header (carrying the
/// secret token) and the URL are written through `curl_quote` so neither can
/// inject curl options into the `--config` document.
///
/// The HTTP status is captured explicitly via `write-out` (as the PUT helper
/// does) and required to be 2xx before the body is trusted. `--fail` alone would
/// reject 4xx/5xx but still accept a 3xx — whose (array-shaped) body could
/// otherwise be parsed as version data. No `location` directive is set, so a
/// redirect is never followed.
fn fastly_api_get(path: &str, token: &str) -> Result<String, String> {
    let header = curl_quote(&format!("Fastly-Key: {token}"));
    let url = curl_quote(&format!("https://api.fastly.com{path}"));
    // `write-out` appends the status on its own trailing line AFTER the body.
    let config = format!("header = {header}\nurl = {url}\nwrite-out = \"\\n%{{http_code}}\"\n");
    let out = curl_config_capture(&config)
        .map_err(|err| format!("Fastly API GET {path} failed: {err}"))?;
    let (body, status_line) = out
        .rsplit_once('\n')
        .ok_or_else(|| format!("Fastly API GET {path}: no HTTP status in the curl output"))?;
    let status: u16 = status_line.trim().parse().map_err(|err| {
        format!(
            "Fastly API GET {path}: could not parse the HTTP status {:?}: {err}",
            status_line.trim()
        )
    })?;
    if !(200..300).contains(&status) {
        return Err(format!("Fastly API GET {path} returned HTTP {status}"));
    }
    Ok(body.to_owned())
}

/// `PUT https://api.fastly.com<path>` with the `Fastly-Key` header;
/// returns the HTTP status, erroring on non-2xx. Fastly's version
/// activate/deactivate endpoints require `PUT` (not `POST`). Header and
/// URL are escaped via `curl_quote`; the literal `request`, `output`,
/// and `write-out` directives are fixed constants.
fn fastly_api_put(path: &str, token: &str) -> Result<u16, String> {
    let header = curl_quote(&format!("Fastly-Key: {token}"));
    let url = curl_quote(&format!("https://api.fastly.com{path}"));
    let config = format!(
        "request = \"PUT\"\nheader = {header}\nurl = {url}\noutput = \"/dev/null\"\nwrite-out = \"%{{http_code}}\"\n"
    );
    let out = curl_config_capture(&config)?;
    let code: u16 = out.trim().parse().map_err(|err| {
        format!(
            "could not parse HTTP status from curl output {:?}: {err}",
            out.trim()
        )
    })?;
    if (200..300).contains(&code) {
        Ok(code)
    } else {
        Err(format!("Fastly API PUT {path} returned HTTP {code}"))
    }
}

/// Create a Fastly store of `kind` named `name`, running the CLI in `cwd`.
/// Treats an "already exists" failure as idempotent success.
fn create_fastly_store_in(kind: &str, name: &str, cwd: &Path) -> Result<(), String> {
    let subcommand = format!("{kind}-store");
    let name_arg = format!("--name={name}");
    let mut command = Command::new("fastly");
    command
        .args([subcommand.as_str(), "create", name_arg.as_str()])
        .current_dir(cwd);
    let output = command.output().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
        } else {
            format!("failed to spawn `fastly`: {err}")
        }
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if looks_like_already_exists(&stderr, kind) {
        return Ok(());
    }
    Err(format!(
        "`fastly {subcommand} create --name={name}` exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
    ))
}

/// Shell `fastly config-store-entry update --upsert --stdin` in `cwd`, piping the
/// value through stdin instead of `--value=<value>` on argv (which would expose
/// the payload in process listings and be bounded by `ARG_MAX`). `--upsert` makes
/// the write idempotent.
fn create_config_store_entry_in(
    store_id: &str,
    key: &str,
    value: &str,
    cwd: &Path,
) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let mut child = Command::new("fastly")
        .args([
            "config-store-entry",
            "update",
            store_arg.as_str(),
            key_arg.as_str(),
            "--upsert",
            "--stdin",
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    // Take stdin OUT of the child and hand it to a helper that writes the value
    // and drops the handle on return — closing the pipe so the CLI sees EOF.
    // Dropping on scope-exit rather than via an explicit `drop()` keeps this
    // valid on targets where `ChildStdin` is a non-Drop stub.
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `fastly`".to_owned())?;
    write_value_to_fastly_stdin(stdin, value)?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait on `fastly`: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`fastly config-store-entry update --store-id={store_id} --key={key} --upsert --stdin` exited with status {}\nstderr: {}",
        output.status,
        redact_stderr(&String::from_utf8_lossy(&output.stderr))
    ))
}

/// Write `value` to the child's stdin, then drop the handle as it falls out of
/// scope on return — closing the pipe so the `fastly` CLI sees EOF. Taking
/// `stdin` by value gives a natural scope-end drop rather than an explicit
/// `drop()`, which also keeps this valid on targets where `ChildStdin` is a
/// non-Drop stub.
fn write_value_to_fastly_stdin(mut stdin: ChildStdin, value: &str) -> Result<(), String> {
    stdin
        .write_all(value.as_bytes())
        .map_err(|err| format!("failed to write value to `fastly` stdin: {err}"))
}

/// Read every `(key, value)` in config store `store_id` via
/// `fastly config-store-entry list --store-id=<id> --json`, run in `cwd`.
///
/// Accepts a bare array or an `{"items": [...]}` envelope, and reads each
/// entry's key/value from `item_key`/`item_value` (the field names
/// `config-store-entry describe` uses), falling back to `key`/`value`. A parse
/// failure is an error, NOT an empty list: a staged deploy mirrors this store,
/// and treating an unreadable listing as "no entries" would silently drop
/// production's overrides from the staged version.
fn read_config_store_entries(store_id: &str, cwd: &Path) -> Result<Vec<(String, String)>, String> {
    let stdout = run_fastly_capture(
        &[
            "config-store-entry".to_owned(),
            "list".to_owned(),
            format!("--store-id={store_id}"),
            "--json".to_owned(),
        ],
        cwd,
    )?;
    parse_config_store_entries(&stdout)
}

/// Parse the `config-store-entry list --json` payload into `(key, value)` pairs.
///
/// Split out from the CLI call so it is unit-testable — and, critically, so every
/// error path REDACTS the payload. The listing carries every entry's `item_value`,
/// which may be production config or secrets, and CLI status lines are logged
/// verbatim into commonly-retained CI logs. So a schema-drift / parse error must
/// summarise the response (size + top-level shape via `redact_describe_response`),
/// never echo the raw stdout.
fn parse_config_store_entries(stdout: &str) -> Result<Vec<(String, String)>, String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).map_err(|err| {
        format!(
            "failed to parse `fastly config-store-entry list --json` JSON: {err} ({})",
            redact_describe_response(stdout)
        )
    })?;
    let array = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))
        .ok_or_else(|| {
            format!(
                "`fastly config-store-entry list --json` output is neither a bare array nor an `items` envelope ({}); fastly CLI may have changed its schema",
                redact_describe_response(stdout)
            )
        })?;
    let mut entries = Vec::with_capacity(array.len());
    for entry in array {
        let key = entry
            .get("item_key")
            .or_else(|| entry.get("key"))
            .and_then(serde_json::Value::as_str);
        let value = entry
            .get("item_value")
            .or_else(|| entry.get("value"))
            .and_then(serde_json::Value::as_str);
        match (key, value) {
            (Some(found_key), Some(found_value)) => {
                entries.push((found_key.to_owned(), found_value.to_owned()));
            }
            _ => {
                return Err(format!(
                    "a `fastly config-store-entry list --json` entry has no string `item_key`/`item_value` fields ({}); fastly CLI may have changed its schema",
                    redact_describe_response(stdout)
                ));
            }
        }
    }
    Ok(entries)
}

/// `fastly config-store-entry delete --store-id=<id> --key=<k>`, run in the
/// app manifest directory so it resolves the right service context.
fn delete_config_store_entry_in(store_id: &str, key: &str, cwd: &Path) -> Result<(), String> {
    run_fastly_status(
        &[
            "config-store-entry".to_owned(),
            "delete".to_owned(),
            format!("--store-id={store_id}"),
            format!("--key={key}"),
        ],
        cwd,
    )
}

/// Look a config store up by name, running the CLI in `cwd`, and return the raw
/// [`ConfigStoreLookup`], so callers can tell "the account has no such store"
/// (`NotFound`) apart from "the lookup itself failed" (`Err` — CLI missing /
/// non-zero exit — or `SchemaDrift`). A staged deploy relies on that distinction
/// to decide whether to skip config isolation (genuinely no store) or fail
/// closed (couldn't tell).
fn classify_remote_config_store_in(name: &str, cwd: &Path) -> Result<ConfigStoreLookup, String> {
    let output = Command::new("fastly")
        .args(["config-store", "list", "--json"])
        .current_dir(cwd)
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if !output.status.success() {
        return Err(format!(
            "`fastly config-store list --json` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = strict_stdout(output.stdout, "config-store list --json")?;
    Ok(find_config_store_id(&stdout, name))
}

/// Resolve the platform config-store id by `name`, running the CLI in `cwd`.
///
/// `Ok(None)` ONLY when the list call SUCCEEDS and no store matches (a genuine
/// absence). An operational failure (missing binary, spawn/list failure, schema
/// drift) stays `Err`.
fn resolve_remote_config_store_id_in(name: &str, cwd: &Path) -> Result<Option<String>, String> {
    match classify_remote_config_store_in(name, cwd)? {
        ConfigStoreLookup::Found(id) => Ok(Some(id)),
        ConfigStoreLookup::NotFound => Ok(None),
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` output: {detail}.\n  The fastly CLI may have changed its JSON schema in a recent version. Please file a bug report at https://github.com/stackpop/edgezero/issues with the fastly CLI version (`fastly version`) and the raw stdout. Workaround: pin to a known-compatible fastly CLI version."
        )),
    }
}

/// Compute the staging selector store's entries from production's, given the
/// declared config-store logical ids.
///
/// The twin is a faithful mirror of this service's production runtime
/// overrides, with exactly one transform: every declared config store's
/// service-scoped selector points at `<logical>_staging`, the key
/// `config push --staging` writes. A declared store gets that selector even when
/// production has no explicit entry for it (production relies on the runtime's
/// default = the logical id; staging must NOT inherit that default, or it would
/// read production's key).
///
/// Pure so the transform is unit-testable without the fastly CLI.
fn staging_entries_from_production(
    production: &[(String, String)],
    service_id: &str,
    config_logical_ids: &[String],
) -> Vec<(String, String)> {
    let service_prefix = service_scoped_runtime_env_key(service_id, "EDGEZERO__");
    // Scoped selector key -> staging value, one per declared config store.
    let selectors: Vec<(String, String)> = config_logical_ids
        .iter()
        .map(|id| (runtime_env_key_for(service_id, id), format!("{id}_staging")))
        .collect();
    let is_selector = |key: &str| selectors.iter().any(|(selector, _)| selector == key);

    // Copy only current-service production overrides. Legacy unscoped entries
    // have no safe owner, and another service's namespace does not belong in
    // this per-service staging twin. Selectors are supplied below whether or
    // not production carried one.
    let mut out: Vec<(String, String)> = production
        .iter()
        .filter(|(key, _)| key.starts_with(&service_prefix) && !is_selector(key))
        .cloned()
        .collect();
    out.extend(selectors);
    out
}

/// The per-service staging twin store name — the base prefix plus the service
/// id, so concurrent staged deploys of different services on one account never
/// clobber each other's selectors.
fn staging_selector_store_name(service_id: &str) -> String {
    format!("{RUNTIME_ENV_STAGING_STORE_PREFIX}_{service_id}")
}

/// Resolve the staging twin store, creating it on demand. A staged deploy owns
/// this store end to end (it is never linked on the ACTIVE version), so it does
/// not depend on `provision` having created it first. Fails closed on a lookup
/// FAILURE rather than blindly creating a duplicate.
fn ensure_staging_selector_store(store_name: &str, cwd: &Path) -> Result<String, String> {
    match classify_remote_config_store_in(store_name, cwd)? {
        ConfigStoreLookup::Found(id) => Ok(id),
        ConfigStoreLookup::NotFound => {
            create_fastly_store_in("config", store_name, cwd)?;
            // We just created the store, so a None here is fail-closed (the
            // listing did not reflect our own create), not a genuine absence.
            resolve_remote_config_store_id_in(store_name, cwd)
                .map_err(|err| {
                    format!(
                        "created fastly config-store `{store_name}` but could not resolve its id: {err}"
                    )
                })?
                .ok_or_else(|| {
                    format!(
                        "created fastly config-store `{store_name}` but it did not appear in `config-store list`"
                    )
                })
        }
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` while resolving `{store_name}`: {detail}.\n  Refusing to stage. Pin a known-compatible fastly CLI version and retry."
        )),
    }
}

/// Reconcile the staging twin so it mirrors the current service's production
/// overrides, with only its config selectors redirected to `<logical>_staging`.
///
/// Upserts the full desired set FIRST, then deletes twin entries production no
/// longer has (so a removed override does not linger and diverge staging from
/// production). Runs while the staged draft is still editable, before the relink.
/// When production has NO override store, `production` is empty and the twin holds
/// only the derived staging selectors — staging is still isolated.
///
/// Order matters: this per-service twin can still be LINKED by a previously-staged
/// version of the same service, which reads it live. Upserting every desired entry
/// before deleting any stale one means that reader never observes a required
/// selector transiently absent (which would fall it back to PRODUCTION config), and
/// a mid-reconciliation failure leaves the twin a superset — never a store missing a
/// selector. `--upsert` (see `create_config_store_entry_in`) makes the writes
/// idempotent, so re-running is safe.
///
/// Residual limitation: two *concurrent* staged deploys of the SAME service still
/// race on this one twin. Serialize them with a per-service concurrency group in
/// the calling workflow; a shared store cannot make that race safe on its own.
fn mirror_production_to_staging(
    production: &[(String, String)],
    staging_id: &str,
    service_id: &str,
    config_logical_ids: &[String],
    cwd: &Path,
) -> Result<(), String> {
    let desired = staging_entries_from_production(production, service_id, config_logical_ids);

    for (key, value) in &desired {
        create_config_store_entry_in(staging_id, key, value, cwd)?;
    }
    let current = read_config_store_entries(staging_id, cwd)?;
    for (key, _) in &current {
        if !desired.iter().any(|(dk, _)| dk == key) {
            delete_config_store_entry_in(staging_id, key, cwd)?;
        }
    }
    Ok(())
}

fn canonical_runtime_env_key_for(logical_id: &str) -> String {
    format!(
        "EDGEZERO__STORES__CONFIG__{}__KEY",
        logical_id.to_ascii_uppercase()
    )
}

/// The service-scoped runtime-override entry naming the config-store key for a
/// logical store. The runtime converts this stored key back to canonical
/// `EDGEZERO__STORES__CONFIG__<ID>__KEY` before building `EnvConfig`.
fn runtime_env_key_for(service_id: &str, logical_id: &str) -> String {
    service_scoped_runtime_env_key(service_id, &canonical_runtime_env_key_for(logical_id))
}

/// Find the id of the resource link published under `link_name` in
/// `fastly resource-link list --json` output.
///
/// The link's own `name` is an alias that defaults to the linked resource's
/// name, so match on it rather than the resource name — the whole point of the
/// staging relink is that a store named `edgezero_runtime_env_staging` is linked
/// under the name `edgezero_runtime_env`.
///
/// Returns `None` when the version has no such link (nothing to delete).
fn find_resource_link_id(stdout: &str, link_name: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let array = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))?;
    array.iter().find_map(|entry| {
        let name = entry.get("name").and_then(serde_json::Value::as_str)?;
        if name != link_name {
            return None;
        }
        entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

/// Resolve the directory containing the Fastly manifest for a staged deploy.
///
/// The CLI resolves the `edgezero.toml` manifest — honouring `EDGEZERO_MANIFEST`
/// — and threads the manifest-configured `[adapters.fastly.adapter].manifest`
/// path in as `--manifest-path <abs fastly.toml>`. Prefer that so a monorepo with
/// multiple Fastly apps stages the app the operator actually selected, rather than
/// whichever `fastly.toml` a bare working-directory search happens to find first.
/// Only when no `--manifest-path` is threaded do we fall back to the
/// working-directory search.
fn resolve_manifest_dir(args: &[String]) -> Result<PathBuf, String> {
    if let Some(raw) = arg_value(args, "--manifest-path") {
        let path = PathBuf::from(raw);
        return path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("fastly manifest path {raw:?} has no parent directory"));
    }
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())
}

/// Whether the caller already supplied a non-interactive switch, so a staged
/// deploy does not append its own (passing it twice makes the Fastly CLI fail).
fn has_non_interactive(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--non-interactive" || arg == "-i")
}

/// `deploy --adapter fastly --service-id <id> --staging`:
/// build, upload to a new draft version (no activation), stage it, and
/// emit `version=<N>`.
pub(super) fn deploy_staged(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    // The Fastly CLI reads FASTLY_API_TOKEN from the env; fail fast
    // with a clear message when it's missing rather than deep in a
    // `fastly compute update` error.
    require_token()?;

    let manifest_dir_buf = resolve_manifest_dir(args)?;
    let manifest_dir = manifest_dir_buf.as_path();
    // The CLI threads the app's declared config-store logical ids as
    // `--edgezero-staging-config=<logical>` (one per store) so the staging relink
    // knows which selectors to redirect — read from the app manifest, never a
    // remote probe. These are EdgeZero-internal inline tokens; strip them so they
    // never reach `fastly compute update`.
    let config_logical_ids: Vec<String> = args
        .iter()
        .filter_map(|arg| {
            arg.strip_prefix("--edgezero-staging-config=")
                .map(str::to_owned)
        })
        .collect();
    let deploy_args: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with("--edgezero-staging-config="))
        .cloned()
        .collect();
    // Strip both the explicitly-threaded `--service-id` and the
    // CLI-injected `--manifest-path` (which `fastly compute update`
    // doesn't understand), then keep only the passthrough flags
    // `compute update` actually supports. `--comment` in particular is
    // NOT a `compute update` flag — it is lifted out here and applied to
    // the version below.
    let extra = args_without_flag_value(
        &args_without_flag_value(&deploy_args, "--service-id"),
        "--manifest-path",
    );
    let passthrough = split_staged_passthrough(&extra);
    if !passthrough.dropped.is_empty() {
        log::warn!(
            "[edgezero] ignoring deploy args not supported by `fastly compute update`: {}",
            passthrough.dropped.join(" ")
        );
    }

    // 1. Build the wasm package (no deploy / activation).
    run_fastly_status(
        &[
            "compute".to_owned(),
            "build".to_owned(),
            "--non-interactive".to_owned(),
        ],
        manifest_dir,
    )?;

    // 2. Clone the active version into a new draft and upload the
    //    package to it — `--autoclone` + `--version=active` keeps
    //    production traffic on the currently-active version.
    let mut update = vec![
        "compute".to_owned(),
        "update".to_owned(),
        "--autoclone".to_owned(),
        format!("--service-id={service_id}"),
        "--version=active".to_owned(),
    ];
    update.extend(passthrough.forwarded.iter().cloned());
    if !has_non_interactive(&passthrough.forwarded) {
        update.push("--non-interactive".to_owned());
    }
    let update_out = run_fastly_capture(&update, manifest_dir)?;

    // Resolve the new draft version from the update output. FAIL CLOSED:
    // if the version cannot be parsed with confidence we return an error
    // rather than guessing. A HIGHEST-version fallback under concurrent
    // deploys could silently stage/roll back a version created by someone
    // else's run.
    let version = parse_fastly_version(&update_out).ok_or_else(|| {
        format!(
            "could not determine the staged version from `fastly compute update` output; \
             refusing to guess (a wrong version would stage another deploy's changes). \
             Raw output:\n{update_out}"
        )
    })?;

    // 3. Apply the operator's `--comment` to the freshly-created draft.
    //    `compute update` has no `--comment`; the version comment is set
    //    with `service-version update`. Done BEFORE staging, while the
    //    version is still an editable draft (and without `--autoclone`,
    //    so it can never clone into yet another version).
    if let Some(comment) = passthrough.comment.as_deref() {
        run_fastly_status(
            &[
                "service-version".to_owned(),
                "update".to_owned(),
                format!("--service-id={service_id}"),
                format!("--version={version}"),
                "--comment".to_owned(),
                comment.to_owned(),
            ],
            manifest_dir,
        )?;
    }

    // 4. Point the draft's runtime-override link at the STAGING selector store,
    //    so this version reads staged config and production keeps reading its
    //    own. Done while the version is still an editable draft.
    relink_runtime_env_for_staging(&service_id, version, &config_logical_ids, manifest_dir)?;

    // 5. Mark the draft version staged (no activation).
    run_fastly_status(
        &[
            "service-version".to_owned(),
            "stage".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
        ],
        manifest_dir,
    )?;

    // 6. Emit the staged version (parseable contract).
    log::info!("version={version}");
    Ok(())
}

/// Point a staged draft's `edgezero_runtime_env` link at the STAGING selector
/// store, so the staged version reads staged config.
///
/// Why this exists: `compute update --autoclone --version=active` clones the
/// active version, and a clone inherits its resource links. Without this, a
/// staged version opens the SAME `edgezero_runtime_env` store as production and
/// therefore reads production's config key — `config push --staging` would write
/// `<key>_staging` that nothing ever reads. Flipping the shared store's selector
/// instead is worse: it redirects production too.
///
/// Fastly resource links are per-version and their `name` is an overridable
/// alias, so linking the staging store under the name `edgezero_runtime_env`
/// gives this draft (and only this draft) staged config.
///
/// Fails closed: if the staging store does not exist we refuse rather than stage
/// a version that would silently serve production config.
fn relink_runtime_env_for_staging(
    service_id: &str,
    version: u64,
    config_logical_ids: &[String],
    manifest_dir: &Path,
) -> Result<(), String> {
    // An app that declares no config stores has no selector to isolate, so
    // staging is still perfectly meaningful for it (staged CODE, no config): the
    // draft keeps the inherited production link and this is a no-op.
    if config_logical_ids.is_empty() {
        log::info!(
            "app declares no config stores, so staged version {version} has no config selector to isolate; keeping the inherited runtime-env link"
        );
        return Ok(());
    }

    // Read the PRODUCTION runtime-override entries to mirror. Fail CLOSED on a
    // lookup FAILURE (CLI missing / non-zero exit / schema drift) — treating
    // "couldn't tell" as "no store" would stage a version that silently reads
    // production config. A genuine `NotFound` is NOT a no-op here: the app
    // DECLARES config (checked above), so the staged version must still be
    // isolated. There is simply nothing to mirror — the twin gets only the
    // derived `<logical>_staging` selectors, and the staged draft is relinked to
    // it so it reads staged config while production keeps its default key.
    let production = match classify_remote_config_store_in(RUNTIME_ENV_STORE_NAME, manifest_dir)? {
        ConfigStoreLookup::Found(id) => read_config_store_entries(&id, manifest_dir)?,
        ConfigStoreLookup::NotFound => Vec::new(),
        ConfigStoreLookup::SchemaDrift(detail) => {
            return Err(format!(
                "could not parse `fastly config-store list --json` while resolving `{RUNTIME_ENV_STORE_NAME}` for a staged deploy: {detail}.\n  Refusing to stage rather than risk serving PRODUCTION config. Pin a known-compatible fastly CLI version and retry."
            ));
        }
    };

    // Mirror production's runtime overrides into the PER-SERVICE staging twin,
    // overriding only the config selectors to `<logical>_staging`, then point
    // THIS draft at the twin. Create the twin on demand so a staged deploy never
    // depends on a prior provision having created it.
    let staging_store_name = staging_selector_store_name(service_id);
    let staging_store_id = ensure_staging_selector_store(&staging_store_name, manifest_dir)?;
    mirror_production_to_staging(
        &production,
        &staging_store_id,
        service_id,
        config_logical_ids,
        manifest_dir,
    )?;

    // Drop the inherited production link first: a version cannot carry two links
    // under the same name.
    let existing = run_fastly_capture(
        &[
            "resource-link".to_owned(),
            "list".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
            "--json".to_owned(),
        ],
        manifest_dir,
    )?;
    if let Some(link_id) = find_resource_link_id(&existing, RUNTIME_ENV_STORE_NAME) {
        run_fastly_status(
            &[
                "resource-link".to_owned(),
                "delete".to_owned(),
                format!("--service-id={service_id}"),
                format!("--version={version}"),
                format!("--id={link_id}"),
            ],
            manifest_dir,
        )?;
    }

    // `--name` is the alias the runtime opens; the linked STORE is the staging
    // twin. No `--autoclone`: the draft is already editable, and cloning here
    // would silently move us onto yet another version.
    run_fastly_status(
        &[
            "resource-link".to_owned(),
            "create".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
            format!("--resource-id={staging_store_id}"),
            format!("--name={RUNTIME_ENV_STORE_NAME}"),
        ],
        manifest_dir,
    )?;

    log::info!("staged version {version} now reads `{staging_store_name}` for its config selector");
    Ok(())
}

/// Production companion to `deploy`: resolve the active service version via the
/// Fastly API and emit it as a `version=<N>` line.
///
/// Distinguishes "confirmed no active version" from an operational failure: a
/// service with no active version yet (a first-ever deploy) is NOT an error — it
/// emits an empty `version=` line and succeeds, so the caller records an empty
/// rollback target. Only a real failure (API/auth error, or a version list that
/// cannot be parsed) returns `Err`, so the caller can fail closed instead of
/// silently proceeding without a rollback target.
///
/// `--require-active` flips the no-active-version case to an error: it is passed
/// by the production-`deploy` version fallback, where a version was JUST
/// activated, so "no active version" is not a valid first-deploy state but an
/// operational failure the CLI must not report as success.
pub(super) fn emit_active_version(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let token = require_token()?;
    let json = fastly_api_get(&format!("/service/{service_id}/version"), &token)?;
    if let Some(version) =
        active_version_or_require(&json, arg_flag(args, "--require-active"), &service_id)?
    {
        log::info!("version={version}");
    } else {
        // Confirmed no active version (first-ever deploy), and it was not
        // required. Emit an explicit empty line so the caller records an empty
        // rollback target and succeeds — distinct from a failure (`Err`).
        log::info!("version=");
        log::info!(
            "service {service_id} has no active version yet; emitting an empty rollback target"
        );
    }
    Ok(())
}

/// Resolve the active version and apply the `--require-active` policy.
///
/// `Ok(Some(n))` — a version is active. `Ok(None)` — no active version and
/// `require_active` is false (a first-ever `active-version` call; the caller
/// records an empty rollback target). `Err` — the response was malformed
/// ([`resolve_active_version`]), OR no version is active while `require_active`
/// is true. The latter is the production-`deploy` fallback: a version was JUST
/// activated, so "no active version" is an error, not a valid empty result.
fn active_version_or_require(
    json: &str,
    require_active: bool,
    service_id: &str,
) -> Result<Option<u64>, String> {
    match resolve_active_version(json)? {
        Some(version) => Ok(Some(version)),
        None if require_active => Err(format!(
            "the deploy reported success but the Fastly API returns no active version for service {service_id}; refusing to report a deploy with no resolvable version"
        )),
        None => Ok(None),
    }
}

/// Require `version` to be the currently ACTIVE service version — the
/// production healthcheck's version contract.
///
/// The production probe hits the live domain, which serves whatever version is
/// active, so "healthcheck version N" is only a true statement about N while N is
/// active. `phase` (`before probing` / `after probing`) names when the check ran,
/// so a version activated by a concurrent deploy is reported clearly rather than
/// masquerading as a healthy `version`.
fn verify_version_active(
    service_id: &str,
    version: u64,
    token: &str,
    phase: &str,
) -> Result<(), String> {
    let json = fastly_api_get(&format!("/service/{service_id}/version"), token)?;
    version_active_verdict(resolve_active_version(&json)?, version, service_id, phase)
}

/// The pure decision behind [`verify_version_active`], split out so the version
/// contract is unit-testable without a live Fastly API.
fn version_active_verdict(
    active: Option<u64>,
    version: u64,
    service_id: &str,
    phase: &str,
) -> Result<(), String> {
    match active {
        Some(active_version) if active_version == version => Ok(()),
        Some(active_version) => Err(format!(
            "production healthcheck version {version} is not active {phase}: service {service_id} currently has version {active_version} active, so the live-domain probe reflects version {active_version}, not {version}"
        )),
        None => Err(format!(
            "production healthcheck version {version} could not be confirmed active {phase}: service {service_id} has no active version"
        )),
    }
}

/// `healthcheck --adapter fastly ...`: probe the domain
/// (production) or the version's staging IP (`--staging`), retrying up
/// to `--retry` times. Emits `status-code` / `healthy` and returns
/// `Err` (non-zero exit) when unhealthy after retries.
///
/// `--domain`, `--service-id` and `--version` are REQUIRED and validated
/// on BOTH the production and the staging path. GitHub Actions' `required:
/// true` does not actually fail a workflow when an input is omitted or
/// empty, so this is the real guard: a production healthcheck must never
/// probe on behalf of an absent/empty version it never verified — the
/// caller chains that same version into rollback.
///
/// On the PRODUCTION path the probe reaches whatever version is live, so when a
/// token is available `version` is verified ACTIVE before and after the probe
/// (see [`verify_version_active`]); without a token the check is service-level.
pub(super) fn healthcheck(args: &[String]) -> Result<(), String> {
    let domain =
        arg_value(args, "--domain").ok_or_else(|| "healthcheck requires --domain".to_owned())?;
    validate_domain(domain)?;
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let version_str =
        arg_value(args, "--version").ok_or_else(|| "healthcheck requires --version".to_owned())?;
    let version = validate_version_str(version_str)?;
    let path = arg_value(args, "--path").unwrap_or("/");
    validate_probe_path(path)?;
    let retry = arg_value(args, "--retry")
        .and_then(|value| value.parse().ok())
        .unwrap_or(3_u32);
    let retry_delay = arg_value(args, "--retry-delay")
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_u64);
    let timeout = arg_value(args, "--timeout")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_u64);
    // curl reads `--max-time 0` as "no limit", so a zero timeout lets a single
    // probe run indefinitely. Require a positive value.
    if timeout == 0 {
        return Err("healthcheck --timeout must be a positive number of seconds".to_owned());
    }

    let is_staging = arg_flag(args, "--staging");
    let staging_ip = if is_staging {
        let token = require_token()?;
        let json = fastly_api_get(
            &format!("/service/{service_id}/version/{version}/domain?include=staging_ips"),
            &token,
        )?;
        let ip = parse_staging_ip(&json).ok_or_else(|| {
            format!("no staging IP found for service {service_id} version {version}")
        })?;
        // `find_staging_ip` searches the response structurally and could surface a
        // non-address string; require a real `IpAddr` before it reaches curl's
        // `--connect-to`, which also settles IPv4-vs-IPv6 formatting.
        ip.parse::<IpAddr>().map_err(|err| {
            format!("resolved staging IP {ip:?} is not a valid IP address: {err}")
        })?;
        Some(ip)
    } else {
        None
    };

    // Production version contract: the probe hits the live domain, which serves
    // whatever version is ACTIVE — not necessarily `version`. When a token is
    // available, require `version` to be active both BEFORE and AFTER the probe, so
    // a version activated concurrently (by another deploy) cannot be reported as a
    // healthy `version`. Without a token the production check is inherently
    // service-level — say so rather than imply a version-specific guarantee. The
    // staging path already targets the specific version's staging IP, so it needs
    // no such check.
    let production_token = if is_staging {
        None
    } else {
        match env::var(FASTLY_API_TOKEN_ENV) {
            Ok(token) if !token.is_empty() => Some(token),
            _ => {
                log::info!(
                    "no {FASTLY_API_TOKEN_ENV} available; production healthcheck is service-level (probes the live domain for service {service_id}, not specifically version {version})"
                );
                None
            }
        }
    };
    if let Some(token) = production_token.as_deref() {
        verify_version_active(&service_id, version, token, "before probing")?;
    }

    let curl_args = build_curl_probe_args(domain, path, staging_ip.as_deref(), timeout);
    let delay = Duration::from_secs(retry_delay);
    let outcome = probe_with_retries(retry, || curl_status(&curl_args), || thread::sleep(delay));
    match outcome {
        Ok(code) => {
            // Confirm `version` is STILL active, so a deploy that activated a newer
            // version during the probe+retries is not reported as a healthy `version`.
            if let Some(token) = production_token.as_deref() {
                verify_version_active(&service_id, version, token, "after probing")?;
            }
            log::info!("status-code={code}");
            log::info!("healthy=true");
            Ok(())
        }
        Err((last_code, msg)) => {
            if let Some(code) = last_code {
                log::info!("status-code={code}");
            }
            log::info!("healthy=false");
            Err(format!(
                "healthcheck for {domain} failed after {} attempt(s): {msg}",
                retry.max(1)
            ))
        }
    }
}

/// Run a single `curl` health probe, returning the HTTP status. A
/// transport failure (timeout, DNS, refused) surfaces as `Err` so the
/// retry loop treats it as an unhealthy attempt.
fn curl_status(args: &[String]) -> Result<u16, String> {
    let output = Command::new("curl").args(args).output().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            "`curl` not found on PATH; install curl and retry".to_owned()
        } else {
            format!("failed to spawn `curl`: {err}")
        }
    })?;
    if !output.status.success() {
        return Err(format!(
            "curl transport failure (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u16>().map_err(|err| {
        format!(
            "could not parse HTTP status from curl output {:?}: {err}",
            stdout.trim()
        )
    })
}

/// `rollback --adapter fastly ...`: production activates the explicit
/// `--rollback-to` version (Fastly cannot infer a previous version);
/// staging deactivates `<version>`.
pub(super) fn rollback(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let version_str =
        arg_value(args, "--version").ok_or_else(|| "rollback requires --version".to_owned())?;
    let version = validate_version_str(version_str)?;
    let token = require_token()?;

    if arg_flag(args, "--staging") {
        // Staging rollback deactivates the STAGED version on the
        // `staging` environment. Fastly's environment-scoped
        // deactivate is `PUT .../deactivate/staging` (a plain
        // `.../deactivate` would target the production activation).
        fastly_api_put(
            &format!("/service/{service_id}/version/{version}/deactivate/staging"),
            &token,
        )?;
        log::info!(
            "[edgezero] deactivated staged version {version} on Fastly service {service_id}"
        );
    } else {
        // Production rollback re-activates an EXPLICIT target. Fastly's version
        // list has no field distinguishing a previously-live version from a
        // staged one (`staging`/`deployed` are documented "Unused"; `locked`
        // only means "not editable"), so the target cannot be inferred — it is
        // captured before the superseding deploy and passed in as --rollback-to.
        let previous = arg_value(args, "--rollback-to")
            .and_then(|raw| validate_version_str(raw).ok())
            .ok_or_else(|| {
                "production rollback requires a valid --rollback-to version".to_owned()
            })?;
        // Best-effort staleness check: the version being rolled back FROM
        // (`--version`) must STILL be the active version. A rollback workflow can
        // run long after its deploy — if a newer version was activated meanwhile,
        // activating the old target would clobber that newer deploy, so refuse.
        //
        // This is NOT atomic: Fastly's activate endpoint has no precondition, so
        // a deploy that lands BETWEEN this read and the activate below can still
        // be clobbered. It narrows the window (catching the common much-later
        // rollback) but does not close it — serialise deploys and rollbacks per
        // SERVICE (a service-scoped concurrency group) to eliminate the race.
        let json = fastly_api_get(&format!("/service/{service_id}/version"), &token)?;
        ensure_rollback_from_is_active(resolve_active_version(&json)?, version, &service_id)?;
        // Fastly's activate endpoint requires `PUT` (not `POST`).
        fastly_api_put(
            &format!("/service/{service_id}/version/{previous}/activate"),
            &token,
        )?;
        log::info!("rolled-back-to={previous}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::cli::path_mutation_guard;
    #[cfg(unix)]
    use edgezero_core::test_env::{EnvOverride, PathPrepend};
    #[cfg(unix)]
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn arg_value_reads_flag_value() {
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--version".to_owned(),
            "42".to_owned(),
        ];
        assert_eq!(arg_value(&args, "--service-id"), Some("SVC1"));
        assert_eq!(arg_value(&args, "--version"), Some("42"));
        assert_eq!(arg_value(&args, "--missing"), None);
    }

    #[test]
    fn arg_value_none_when_flag_is_last() {
        let args = vec!["--version".to_owned()];
        assert_eq!(arg_value(&args, "--version"), None);
    }

    #[test]
    fn arg_flag_detects_presence() {
        let args = vec!["--staging".to_owned()];
        assert!(arg_flag(&args, "--staging"));
        assert!(!arg_flag(&args, "--nope"));
    }

    #[test]
    fn args_without_flag_value_strips_pair() {
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--comment".to_owned(),
            "ci".to_owned(),
        ];
        assert_eq!(
            args_without_flag_value(&args, "--service-id"),
            vec!["--comment".to_owned(), "ci".to_owned()]
        );
    }

    #[test]
    fn resolve_manifest_dir_prefers_manifest_path_flag() {
        // When the CLI threads `--manifest-path <abs fastly.toml>`, the
        // staged deploy must use its parent directory rather than a bare
        // working-directory search (which in a monorepo could pick a
        // different app's fastly.toml).
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            "/repo/apps/edge/fastly.toml".to_owned(),
        ];
        let dir = resolve_manifest_dir(&args).expect("resolves from --manifest-path");
        assert_eq!(dir, PathBuf::from("/repo/apps/edge"));
    }

    #[test]
    fn resolve_service_id_prefers_flag() {
        let args = vec!["--service-id".to_owned(), "SVCFROMARG".to_owned()];
        assert_eq!(resolve_service_id(&args).unwrap(), "SVCFROMARG");
    }

    #[test]
    fn split_staged_passthrough_lifts_comment_out_of_compute_update() {
        // `fastly compute update` has NO `--comment` flag (verified against
        // `fastly compute update --help`, CLI v15) — forwarding it makes the
        // command exit non-zero and fails the whole staged deploy. It must be
        // lifted out and applied via `service-version update` instead.
        for args in [owned(&["--comment", "ci run 12"]), owned(&["--comment=x"])] {
            let split = split_staged_passthrough(&args);
            assert!(
                !split
                    .forwarded
                    .iter()
                    .any(|arg| arg.starts_with("--comment")),
                "--comment must never reach `compute update`: {:?}",
                split.forwarded
            );
            assert!(
                split.comment.is_some(),
                "comment must be captured: {args:?}"
            );
        }
        assert_eq!(
            split_staged_passthrough(&owned(&["--comment", "ci run 12"])).comment,
            Some("ci run 12".to_owned())
        );
        assert_eq!(
            split_staged_passthrough(&owned(&["--comment=x"])).comment,
            Some("x".to_owned())
        );
    }

    #[test]
    fn split_staged_passthrough_forwards_supported_flags_only() {
        let args = owned(&[
            "--package",
            "pkg.tar.gz",
            "--autoclone",
            "--verbose",
            "--comment",
            "note",
            "--env",
            "stage",
            "--status-check-off",
        ]);
        let split = split_staged_passthrough(&args);
        // Supported by `compute update`: kept (value flags keep their value).
        assert_eq!(
            split.forwarded,
            owned(&["--package", "pkg.tar.gz", "--autoclone", "--verbose"])
        );
        // `--env`/`--status-check-off` are `compute deploy` flags, not
        // `compute update` ones: dropped, and `--env`'s detached value
        // `stage` is dropped with it (never left as a bogus positional).
        assert_eq!(split.dropped, owned(&["--env", "--status-check-off"]));
        assert!(!split.forwarded.iter().any(|arg| arg == "stage"));
        assert_eq!(split.comment, Some("note".to_owned()));
    }

    #[test]
    fn has_non_interactive_detects_both_spellings() {
        assert!(has_non_interactive(&owned(&["--non-interactive"])));
        assert!(has_non_interactive(&owned(&["-i"])));
        assert!(!has_non_interactive(&owned(&["--autoclone"])));
    }

    #[test]
    fn healthcheck_rejects_missing_or_empty_required_values_on_production() {
        for (args, needle) in [
            (
                owned(&["--domain", "example.com", "--service-id", "SVC1"]),
                "--version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "SVC1",
                    "--version",
                    "",
                ]),
                "invalid version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "SVC1",
                    "--version",
                    "15.2.0",
                ]),
                "invalid version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "",
                    "--version",
                    "7",
                ]),
                "invalid service id",
            ),
            (
                owned(&["--domain", "", "--service-id", "SVC1", "--version", "7"]),
                "invalid domain",
            ),
            (
                owned(&["--service-id", "SVC1", "--version", "7"]),
                "--domain",
            ),
        ] {
            let err = healthcheck(&args).expect_err("must reject absent/empty required value");
            assert!(
                err.contains(needle),
                "expected {needle:?} in error for {args:?}, got: {err}"
            );
        }
    }

    #[test]
    fn healthcheck_rejects_empty_required_values_on_staging() {
        for args in [
            owned(&[
                "--staging",
                "--domain",
                "example.com",
                "--service-id",
                "",
                "--version",
                "7",
            ]),
            owned(&[
                "--staging",
                "--domain",
                "example.com",
                "--service-id",
                "SVC1",
                "--version",
                "",
            ]),
        ] {
            healthcheck(&args).expect_err("staging must reject empty required values");
        }
    }

    #[test]
    fn rollback_rejects_missing_or_invalid_required_values() {
        for staging in [&[][..], &["--staging".to_owned()][..]] {
            for bad in [
                owned(&["--service-id", "SVC1"]),
                owned(&["--service-id", "SVC1", "--version", ""]),
                owned(&["--service-id", "SVC1", "--version", "12abc"]),
                owned(&["--service-id", "", "--version", "7"]),
            ] {
                let mut args = bad.clone();
                args.extend_from_slice(staging);
                rollback(&args).expect_err("rollback must reject invalid required values");
            }
        }
    }

    #[test]
    fn curl_quote_escapes_quotes_and_backslashes() {
        assert_eq!(curl_quote("plain"), "\"plain\"");
        assert_eq!(curl_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(curl_quote("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn curl_quote_never_emits_raw_control_characters() {
        // A token carrying a `"` and a newline must not be able to
        // terminate its quoted value and inject a second `url = "..."`
        // directive. The `"` is escaped and the newline is folded to a
        // `\n` escape so NO raw newline reaches the curl config file.
        let token = "tok\"en\nurl = \"https://evil.example\"";
        let quoted = curl_quote(token);
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert!(!quoted.contains('\n'), "no raw newline: {quoted}");
        assert!(!quoted.contains('\r'));
        // The only unescaped `"` are the wrapping pair; every interior
        // quote is preceded by a backslash.
        assert_eq!(quoted, "\"tok\\\"en\\nurl = \\\"https://evil.example\\\"\"");
        // A tab folds too.
        assert_eq!(curl_quote("a\tb"), "\"a\\tb\"");
    }

    #[test]
    fn validate_service_id_accepts_fastly_handle() {
        validate_service_id("SU1Z0isxPaozGVKXdv0eY").expect("alphanumeric handle");
    }

    #[test]
    fn validate_service_id_rejects_non_alphanumeric_characters() {
        validate_service_id("SVC1_").expect_err("trailing underscore");
        validate_service_id("SVC-1").expect_err("hyphen");
    }

    #[test]
    fn validate_service_id_rejects_runtime_env_namespace_delimiter() {
        let err = validate_service_id("SVC__OTHER")
            .expect_err("the runtime-env namespace delimiter must be unambiguous");
        assert!(
            err.contains("namespace delimiter"),
            "error explains the reserved delimiter: {err}"
        );
    }

    #[test]
    fn validate_service_id_rejects_injection_and_empty() {
        // The canonical attack: a service id that closes the url value
        // and appends a second url directive.
        validate_service_id("abc\nurl = \"http://evil\"").expect_err("newline injection");
        validate_service_id("abc\"def").expect_err("quote");
        validate_service_id("has space").expect_err("space");
        validate_service_id("has/slash").expect_err("slash");
        validate_service_id("").expect_err("empty");
    }

    #[test]
    fn validate_version_str_accepts_integer_rejects_junk() {
        assert_eq!(validate_version_str("42"), Ok(42));
        assert_eq!(validate_version_str("0"), Ok(0));
        validate_version_str("-1").expect_err("negative");
        validate_version_str("4.2").expect_err("float");
        validate_version_str("42\nurl = \"x\"").expect_err("newline injection");
        validate_version_str("").expect_err("empty");
    }

    #[test]
    fn validate_domain_accepts_hostnames_rejects_injection() {
        validate_domain("example.com").expect("bare hostname");
        validate_domain("staging.example.co.uk").expect("multi-label hostname");
        validate_domain("host-1.example.com").expect("hostname with dash");
        validate_domain("").expect_err("empty");
        validate_domain(".example.com").expect_err("leading dot");
        validate_domain("example.com.").expect_err("trailing dot");
        validate_domain("exa..mple.com").expect_err("empty label");
        validate_domain("example.com/evil").expect_err("slash");
        validate_domain("example.com\nurl = \"x\"").expect_err("newline injection");
        validate_domain("has space.com").expect_err("space");
    }

    #[test]
    fn version_active_verdict_enforces_the_production_version_contract() {
        // The requested version is the active one: healthy.
        version_active_verdict(Some(7), 7, "SVC1", "before probing").expect("match is ok");
        // A different active version (a concurrent deploy) must fail closed and name
        // BOTH versions so the mismatch is diagnosable.
        let err = version_active_verdict(Some(9), 7, "SVC1", "after probing")
            .expect_err("a newer active version must fail the version contract");
        assert!(err.contains('7') && err.contains('9'), "{err}");
        // No active version at all is not a healthy version-7 report either.
        version_active_verdict(None, 7, "SVC1", "before probing")
            .expect_err("no active version must fail the contract");
    }

    #[test]
    fn is_healthy_status_covers_2xx_only() {
        assert!(is_healthy_status(200));
        assert!(is_healthy_status(204));
        assert!(is_healthy_status(299));
        // 3xx is NOT healthy: the probe does not follow redirects, so a 301 to an
        // error page must not pass a gate that suppresses an automatic rollback.
        assert!(!is_healthy_status(301));
        assert!(!is_healthy_status(399));
        assert!(!is_healthy_status(400));
        assert!(!is_healthy_status(500));
        assert!(!is_healthy_status(199));
    }

    #[test]
    fn parse_fastly_version_handles_the_shapes_fastly_emits() {
        // The Fastly CLI's own success lines. Go format strings:
        //   "Updated package (service %s, version %v)"  (compute update)
        //   "Deployed package (service %s, version %v)" (compute deploy)
        assert_eq!(
            parse_fastly_version("SUCCESS: Deployed package (service abc, version 7)"),
            Some(7)
        );
        assert_eq!(
            parse_fastly_version("\nSUCCESS: Updated package (service SU1Z0, version 42)\n"),
            Some(42)
        );
        // Our canonical contract line.
        assert_eq!(parse_fastly_version("version=12"), Some(12));
        // The --autoclone notice, when no success line is present.
        assert_eq!(
            parse_fastly_version(
                "Service version 3 is not editable, so it was automatically cloned because \
                 --autoclone is enabled. Now operating on version 4."
            ),
            Some(4)
        );
        // Full autoclone + success output: the SUCCESS line wins, and the
        // PRE-clone version (3) never does — even though stdout/stderr are
        // concatenated and their relative order is not guaranteed.
        let combined = "SUCCESS: \nUpdated package (service abc, version 4)\n\
             Service version 3 is not editable, so it was automatically cloned. \
             Now operating on version 4.";
        assert_eq!(parse_fastly_version(combined), Some(4));
        assert_eq!(parse_fastly_version("no numbers here"), None);
    }

    #[test]
    fn parse_fastly_version_rejects_confusable_lines() {
        // A lax parser that took ANY digits after the word "version" produced a
        // WRONG service version for each of these. They must all be `None`, which
        // makes `deploy_staged` fail closed.
        assert_eq!(
            parse_fastly_version("Uploaded package to service 12345, version unchanged"),
            None
        );
        // The CLI's own semver must not be mistaken for a service version.
        assert_eq!(parse_fastly_version("Fastly CLI version 15.2.0"), None);
        assert_eq!(
            parse_fastly_version("Checking version compatibility for service 99"),
            None
        );
        // A bare `version <N>` mention with no success-line context is not
        // trusted either.
        assert_eq!(parse_fastly_version("cloning version 3"), None);
        // `--version=active` echoed in a command line is not a contract line.
        assert_eq!(
            parse_fastly_version("running: fastly compute update --version=active"),
            None
        );
    }

    #[test]
    fn parse_active_version_finds_active_entry() {
        let json = r#"[
            {"number": 1, "active": false},
            {"number": 2, "active": true},
            {"number": 3, "active": false}
        ]"#;
        assert_eq!(resolve_active_version(json), Ok(Some(2)));
    }

    #[test]
    fn parse_active_version_none_when_no_active() {
        // A parsed list with no active version is `Ok(None)` — confirmed
        // no active version (first deploy), NOT an operational failure.
        let json = r#"[{"number": 1, "active": false}]"#;
        assert_eq!(resolve_active_version(json), Ok(None));
    }

    #[test]
    fn resolve_active_version_errors_on_unparseable_payload() {
        // A truncated / non-array body is an operational failure, distinct from
        // "no active version" — the caller must fail closed, not record empty.
        resolve_active_version("not json").expect_err("non-JSON must be an operational error");
        resolve_active_version(r#"{"error":"unauthorized"}"#)
            .expect_err("a non-array body must be an operational error");
    }

    #[test]
    fn resolve_active_version_errors_on_malformed_active_entries() {
        // A garbled ACTIVE entry must fail closed, not read as "no active
        // version" — otherwise a production deploy proceeds with no rollback
        // target. Each of these is malformed and must be an operational error.
        resolve_active_version(r#"[{"active":true}]"#)
            .expect_err("active entry with no `number` must error");
        resolve_active_version(r#"[{"active":true,"number":"7"}]"#)
            .expect_err("active entry with a string `number` must error");
        resolve_active_version(r#"[{"active":"true","number":7}]"#)
            .expect_err("a non-boolean `active` must error");
        // A non-boolean `active` ANYWHERE is schema drift — the whole list is
        // scanned, so it is caught even AFTER a valid active entry (a naive
        // first-match parser would miss this one).
        resolve_active_version(r#"[{"active":"false"},{"active":true,"number":9}]"#)
            .expect_err("a non-boolean `active` before the active entry is schema drift");
        resolve_active_version(r#"[{"active":true,"number":9},{"active":"nope"}]"#)
            .expect_err("a non-boolean `active` AFTER the active entry is still schema drift");
        // More than one active version is ambiguous — refuse rather than pick one.
        resolve_active_version(r#"[{"active":true,"number":9},{"active":true,"number":10}]"#)
            .expect_err("two active versions must error as ambiguous");
        // EVERY element must be a version object with a numeric `number` — a
        // garbled entry must fail closed, not be skipped as "not active".
        resolve_active_version("[]").expect_err("an empty version list is an invalid response");
        resolve_active_version("[null]").expect_err("a null element must error");
        resolve_active_version("[{}]").expect_err("an entry with no `number` must error");
        resolve_active_version(r#"[{"number":"invalid"}]"#)
            .expect_err("a non-numeric `number` must error");
        // An omitted `active` field means "not active" (not an error), as long
        // as the entry is otherwise a well-formed version object.
        assert_eq!(resolve_active_version(r#"[{"number":42}]"#), Ok(None));
        // Sanity: a well-formed list still resolves.
        assert_eq!(
            resolve_active_version(r#"[{"active":false,"number":1},{"active":true,"number":2}]"#),
            Ok(Some(2))
        );
    }

    #[test]
    fn ensure_rollback_from_is_active_blocks_racing_deploys() {
        // The version being rolled back FROM is still active → proceed.
        assert_eq!(ensure_rollback_from_is_active(Some(7), 7, "svc"), Ok(()));
        // A NEWER version is active (a deploy raced the rollback) → refuse, so
        // the newer deploy is not clobbered.
        ensure_rollback_from_is_active(Some(9), 7, "svc")
            .expect_err("a newer active version must block the rollback");
        // No active version at all → refuse.
        ensure_rollback_from_is_active(None, 7, "svc")
            .expect_err("no active version must block the rollback");
    }

    #[test]
    fn active_version_or_require_enforces_require_active() {
        let active = r#"[{"active":true,"number":5}]"#;
        let none = r#"[{"active":false,"number":5}]"#;

        // A resolvable active version is returned regardless of the flag.
        assert_eq!(active_version_or_require(active, false, "svc"), Ok(Some(5)));
        assert_eq!(active_version_or_require(active, true, "svc"), Ok(Some(5)));

        // No active version: tolerated for `active-version` (first deploy), but an
        // ERROR for the production-deploy fallback (`--require-active`), which
        // must never report a deploy with no resolvable version.
        assert_eq!(active_version_or_require(none, false, "svc"), Ok(None));
        active_version_or_require(none, true, "svc")
            .expect_err("require-active with no active version must fail closed");

        // A malformed response is an error either way.
        active_version_or_require("not json", false, "svc").expect_err("malformed must error");
    }

    #[test]
    fn parse_staging_ip_reads_the_singular_staging_ip_field() {
        // The REAL Fastly response shape for
        // `GET /service/<id>/version/<n>/domain?include=staging_ips`:
        // an array of domain objects, each with a SINGULAR `staging_ip`
        // STRING. Body copied from go-fastly's recorded API fixture
        // `fastly/fixtures/domains/list_with_staging_ips.yaml`, matching
        // its `StagingIP *string `mapstructure:"staging_ip"`` field.
        // (`staging_ips` is only the `include=` query value, never a
        // field name — a parser looking for it as an array would NEVER
        // find a staging IP.)
        let json = r#"[
            {
                "created_at": "2022-11-04T17:36:56Z",
                "service_id": "kKJb5bOFI47uHeBVluGfX1",
                "name": "integ-test-20221104.go-fastly-1.com",
                "version": 73,
                "comment": "comment",
                "deleted_at": null,
                "staging_ip": "167.82.81.194"
            }
        ]"#;
        assert_eq!(parse_staging_ip(json).as_deref(), Some("167.82.81.194"));
    }

    #[test]
    fn parse_staging_ip_tolerates_a_plural_array_shape() {
        let json = r#"[{"name": "example.com", "staging_ips": ["151.101.2.10"]}]"#;
        assert_eq!(parse_staging_ip(json).as_deref(), Some("151.101.2.10"));
    }

    #[test]
    fn parse_staging_ip_none_when_absent_or_null() {
        assert_eq!(parse_staging_ip(r#"[{"name": "example.com"}]"#), None);
        // `staging_ip` is nullable for services without staging enabled.
        assert_eq!(
            parse_staging_ip(r#"[{"name": "example.com", "staging_ip": null}]"#),
            None
        );
    }

    #[test]
    fn parse_config_store_entries_reads_key_value_pairs() {
        let entries = parse_config_store_entries(
            r#"[{"item_key":"A","item_value":"1"},{"item_key":"B","item_value":"2"}]"#,
        )
        .expect("well-formed listing parses");
        assert_eq!(
            entries,
            vec![
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn parse_config_store_entries_errors_never_leak_the_value() {
        // The listing carries every entry's item_value (possibly a production secret),
        // and CLI status lines are logged verbatim into retained CI logs — so no error
        // path may echo the payload. A sentinel secret must NEVER appear in any error.
        const SECRET: &str = "s3cr3t-sentinel-value";

        // 1. Malformed JSON.
        let malformed_json = parse_config_store_entries(&format!("not json {SECRET}"))
            .expect_err("malformed JSON must error");
        assert!(
            !malformed_json.contains(SECRET),
            "malformed-JSON error leaked the value: {malformed_json}"
        );

        // 2. Schema drift: valid JSON that is neither a bare array nor an `items`
        //    envelope (here an object whose VALUE is the secret).
        let drift = parse_config_store_entries(&format!(r#"{{"unexpected":"{SECRET}"}}"#))
            .expect_err("schema drift must error");
        assert!(
            !drift.contains(SECRET),
            "schema-drift error leaked the value: {drift}"
        );

        // 3. Malformed entry: a valid array where an entry lacks item_key/item_value,
        //    while a SIBLING entry carries the secret in its value.
        let bad_entry = parse_config_store_entries(&format!(
            r#"[{{"item_key":"ok","item_value":"{SECRET}"}},{{"item_key":"bad"}}]"#
        ))
        .expect_err("a malformed entry must error");
        assert!(
            !bad_entry.contains(SECRET),
            "malformed-entry error leaked the value: {bad_entry}"
        );
    }

    #[test]
    fn build_curl_probe_args_production_has_no_connect_to() {
        let args = build_curl_probe_args("example.com", "/", None, 10);
        assert!(!args.iter().any(|arg| arg == "--connect-to"));
        assert!(args.contains(&"https://example.com/".to_owned()));
        assert!(args.contains(&"--max-time".to_owned()));
        assert!(args.contains(&"10".to_owned()));
        // Globbing must be off so bracket/brace characters in a path are literal.
        assert!(args.contains(&"--globoff".to_owned()));
    }

    #[test]
    fn build_curl_probe_args_path_with_glob_chars_is_literal() {
        // A path with `[` `]` would be a curl glob without --globoff; here it must
        // appear verbatim in the single URL argument, with globbing disabled.
        let args = build_curl_probe_args("example.com", "/health?ids[0]=1", None, 10);
        assert!(args.contains(&"--globoff".to_owned()));
        assert!(args.contains(&"https://example.com/health?ids[0]=1".to_owned()));
    }

    #[test]
    fn build_curl_probe_args_staging_reroutes_to_ip() {
        let args = build_curl_probe_args("staging.example.com", "/", Some("151.101.2.10"), 15);
        let idx = args
            .iter()
            .position(|arg| arg == "--connect-to")
            .expect("--connect-to present for staging");
        assert_eq!(args[idx + 1], "::151.101.2.10:443");
        assert!(args.contains(&"https://staging.example.com/".to_owned()));
    }

    #[test]
    fn build_curl_probe_args_leads_with_q_to_ignore_curlrc() {
        // `-q` must be the FIRST argument or curl merges `~/.curlrc` before it.
        let args = build_curl_probe_args("example.com", "/", None, 10);
        assert_eq!(args.first().map(String::as_str), Some("-q"));
    }

    #[test]
    fn build_curl_probe_args_brackets_ipv6_connect_to() {
        let args = build_curl_probe_args("staging.example.com", "/", Some("2001:db8::1"), 10);
        let idx = args
            .iter()
            .position(|arg| arg == "--connect-to")
            .expect("--connect-to present for staging");
        // An IPv6 literal must be bracketed so curl does not misparse the colons.
        assert_eq!(args[idx + 1], "::[2001:db8::1]:443");
    }

    #[test]
    fn build_curl_probe_args_honors_path_on_production_and_staging() {
        // Production: the path is appended to the domain URL.
        let prod = build_curl_probe_args("example.com", "/health", None, 10);
        assert!(prod.contains(&"https://example.com/health".to_owned()));
        // Staging: same URL (with the path), rerouted to the staging IP.
        let staging =
            build_curl_probe_args("staging.example.com", "/health", Some("151.101.2.10"), 10);
        assert!(staging.contains(&"https://staging.example.com/health".to_owned()));
        let idx = staging
            .iter()
            .position(|arg| arg == "--connect-to")
            .expect("--connect-to present for staging");
        assert_eq!(staging[idx + 1], "::151.101.2.10:443");
    }

    #[test]
    fn validate_probe_path_requires_leading_slash_and_no_whitespace() {
        validate_probe_path("/").expect("root");
        validate_probe_path("/health").expect("simple path");
        validate_probe_path("/api/v1/status?ready=1").expect("path with query");
        validate_probe_path("health").expect_err("no leading slash");
        validate_probe_path("").expect_err("empty");
        validate_probe_path("/ with space").expect_err("whitespace");
        validate_probe_path("/inject\nHost: evil").expect_err("newline injection");
    }

    #[test]
    fn healthcheck_rejects_zero_timeout() {
        // A zero timeout becomes curl `--max-time 0` (no limit); reject it before
        // any probe. The other required args are valid so we reach the check.
        let args = [
            "--adapter",
            "fastly",
            "--domain",
            "example.com",
            "--service-id",
            "svc123",
            "--version",
            "1",
            "--timeout",
            "0",
        ]
        .map(str::to_owned)
        .to_vec();
        let err = healthcheck(&args).expect_err("zero timeout must be rejected");
        assert!(err.contains("timeout"), "unexpected error: {err}");
    }

    #[test]
    fn probe_with_retries_returns_first_healthy() {
        let mut calls: i32 = 0;
        let mut between: i32 = 0;
        let result = probe_with_retries(
            5,
            || {
                calls += 1_i32;
                Ok(200)
            },
            || between += 1_i32,
        );
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 1_i32, "should stop after first healthy probe");
        assert_eq!(between, 0_i32, "no delay before the first attempt");
    }

    #[test]
    fn probe_with_retries_succeeds_after_unhealthy_attempts() {
        let mut calls: i32 = 0;
        let mut between: i32 = 0;
        let result = probe_with_retries(
            5,
            || {
                calls += 1_i32;
                if calls < 3_i32 { Ok(503) } else { Ok(200) }
            },
            || between += 1_i32,
        );
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 3_i32);
        assert_eq!(
            between, 2_i32,
            "delay runs between each of the first 3 attempts"
        );
    }

    #[test]
    fn probe_with_retries_exhausts_and_reports_last_code() {
        let mut between: i32 = 0;
        let result = probe_with_retries(3, || Ok(500), || between += 1_i32);
        assert_eq!(
            result,
            Err((Some(500), "unhealthy HTTP status 500".to_owned()))
        );
        assert_eq!(
            between, 2_i32,
            "delay runs between attempts, not after the last"
        );
    }

    #[test]
    fn probe_with_retries_reports_transport_error() {
        let result: Result<u16, (Option<u16>, String)> =
            probe_with_retries(1, || Err("connection refused".to_owned()), || {});
        assert_eq!(result, Err((None, "connection refused".to_owned())));
    }

    #[test]
    fn probe_with_retries_treats_zero_retry_as_one_attempt() {
        let mut calls: i32 = 0;
        let result = probe_with_retries(
            0,
            || {
                calls += 1_i32;
                Ok(500)
            },
            || {},
        );
        assert_eq!(
            result,
            Err((Some(500), "unhealthy HTTP status 500".to_owned()))
        );
        assert_eq!(calls, 1_i32);
    }

    #[test]
    fn runtime_env_key_is_scoped_for_the_runtime_reader() {
        assert_eq!(
            canonical_runtime_env_key_for("app_config"),
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"
        );
        assert_eq!(
            runtime_env_key_for("SVCA", "app_config"),
            "EDGEZERO__SERVICES__SVCA__STORES__CONFIG__APP_CONFIG__KEY"
        );
    }

    #[test]
    fn staging_entries_from_production_mirrors_only_current_service_entries() {
        // Production carries an unscoped legacy override, this service's
        // explicit selector and name mapping, and another service's mapping.
        // The per-service twin keeps only current-service values, replacing
        // every declared selector with its scoped staging value.
        let production = vec![
            (
                "EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL".to_owned(),
                "debug".to_owned(),
            ),
            (
                "EDGEZERO__SERVICES__SVC1__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
                "custom_prod_key".to_owned(),
            ),
            (
                "EDGEZERO__SERVICES__SVC1__STORES__CONFIG__APP_CONFIG__NAME".to_owned(),
                "app_config".to_owned(),
            ),
            (
                "EDGEZERO__SERVICES__SVC2__STORES__SECRETS__DEFAULT__NAME".to_owned(),
                "other_service_secrets".to_owned(),
            ),
        ];
        let out = staging_entries_from_production(
            &production,
            "SVC1",
            &["app_config".to_owned(), "feature_flags".to_owned()],
        );

        assert!(
            !out.iter()
                .any(|(key, _)| key == "EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL"),
            "legacy unscoped entries are not part of a service-owned twin: {out:?}"
        );
        assert!(out.contains(&(
            "EDGEZERO__SERVICES__SVC1__STORES__CONFIG__APP_CONFIG__NAME".to_owned(),
            "app_config".to_owned()
        )));
        assert!(out.contains(&(
            "EDGEZERO__SERVICES__SVC1__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
            "app_config_staging".to_owned()
        )));
        assert!(!out.iter().any(|(_, value)| value == "custom_prod_key"));
        assert!(out.contains(&(
            "EDGEZERO__SERVICES__SVC1__STORES__CONFIG__FEATURE_FLAGS__KEY".to_owned(),
            "feature_flags_staging".to_owned()
        )));
        assert!(
            !out.iter().any(|(key, value)| {
                key.contains("__SVC2__") || value == "other_service_secrets"
            }),
            "another service's scoped entries must not enter this twin: {out:?}"
        );
        assert_eq!(
            out.iter()
                .filter(|(key, _)| {
                    key == "EDGEZERO__SERVICES__SVC1__STORES__CONFIG__APP_CONFIG__KEY"
                })
                .count(),
            1
        );
    }

    #[test]
    fn find_resource_link_id_matches_on_link_name_not_resource_name() {
        // The link's `name` is an alias defaulting to the resource's name. The
        // staging relink depends on that alias: a store named
        // `edgezero_runtime_env_staging` is linked AS `edgezero_runtime_env`.
        let json = r#"[
            {"id":"LINK_KV","name":"sessions"},
            {"id":"LINK_ENV","name":"edgezero_runtime_env"}
        ]"#;
        assert_eq!(
            find_resource_link_id(json, "edgezero_runtime_env").as_deref(),
            Some("LINK_ENV")
        );
        // Absent link -> nothing to delete, not an error.
        assert_eq!(find_resource_link_id(json, "nope"), None);
        // Tolerates the `{"items": [...]}` envelope, like the store lookup.
        let enveloped = r#"{"items":[{"id":"L1","name":"edgezero_runtime_env"}]}"#;
        assert_eq!(
            find_resource_link_id(enveloped, "edgezero_runtime_env").as_deref(),
            Some("L1")
        );
        assert_eq!(find_resource_link_id("not json", "x"), None);
    }

    /// Fake `fastly` on `$PATH` that appends every invocation's argv (one
    /// space-joined line per call) to a record file, and echoes
    /// `update_stdout` for `fastly compute update`. Returns the temp dir
    /// (which must outlive the test) and the record path.
    #[cfg(unix)]
    fn fake_fastly_recorder(update_stdout: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let script_path = dir.path().join("fastly");
        // Answers every call `deploy_staged` makes. The staging relink needs the
        // selector store to resolve and the inherited link to be listed; without
        // these the staged path fails closed (which is correct, but not what
        // these tests are exercising).
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' '{update_stdout}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}},{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"update\" ]; then\n  \
               cat >/dev/null\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               case \"$*\" in\n    \
                 *--store-id=ENVSEL1*) printf '%s\\n' '[{{\"item_key\":\"EDGEZERO__SERVICES__SVC1__LOGGING__LEVEL\",\"item_value\":\"debug\"}}]' ;;\n    \
                 *) printf '%s\\n' '[]' ;;\n  \
               esac\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"LINK1\",\"name\":\"edgezero_runtime_env\"}}]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
        );
        fs::write(&script_path, script).expect("write fake fastly");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        (dir, record)
    }

    /// Run `deploy_staged` against a fake `fastly`, returning the result
    /// and the recorded argv lines.
    #[cfg(unix)]
    fn run_deploy_staged_with_fake(
        update_stdout: &str,
        extra: &[&str],
    ) -> (Result<(), String>, Vec<String>) {
        run_deploy_staged_with_fake_and_env(update_stdout, extra, None)
    }

    #[cfg(unix)]
    fn run_deploy_staged_with_fake_and_env(
        update_stdout: &str,
        extra: &[&str],
        store_name_override: Option<(&str, &str)>,
    ) -> (Result<(), String>, Vec<String>) {
        let _lock = path_mutation_guard().lock().expect("guard");
        let (fake, record) = fake_fastly_recorder(update_stdout);
        let _path = PathPrepend::new(fake.path());
        let app = tempdir().expect("app dir");
        let manifest = app.path().join("fastly.toml");
        fs::write(&manifest, "name = \"app\"\n").expect("write fastly.toml");

        // RAII: set the variables for the call, then restore them on drop. The
        // shared guard serializes every process-environment mutation in tests.
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");
        let _store_name_override =
            store_name_override.map(|(key, value)| EnvOverride::set(key, value));
        let mut args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            manifest.display().to_string(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let result = deploy_staged(&args);

        let recorded = fs::read_to_string(&record).unwrap_or_default();
        let lines = recorded.lines().map(str::to_owned).collect();
        (result, lines)
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_routes_comment_to_service_version_update() {
        // `--comment` is allowlisted for `deploy-args` and recommended by the
        // adoption guide, but `fastly compute update` has no such flag. It
        // must NOT be forwarded there (that would fail the deploy) and must
        // instead land on the version via `service-version update`.
        for comment_args in [vec!["--comment", "ci run 12"], vec!["--comment=ci run 12"]] {
            let (result, argv) = run_deploy_staged_with_fake(
                "SUCCESS: Updated package (service SVC1, version 7)",
                &comment_args,
            );
            result.expect("staged deploy with --comment must succeed");

            let update = argv
                .iter()
                .find(|line| line.starts_with("compute update"))
                .expect("compute update was invoked");
            assert!(
                !update.contains("--comment"),
                "--comment must not be forwarded to `compute update`: {update}"
            );
            assert!(
                update.contains("--non-interactive"),
                "compute update must be non-interactive: {update}"
            );

            let comment_call = argv
                .iter()
                .find(|line| line.starts_with("service-version update"))
                .expect("`service-version update` must apply the version comment");
            assert_eq!(
                comment_call,
                "service-version update --service-id=SVC1 --version=7 --comment ci run 12"
            );

            // The comment lands on the version BEFORE it is staged (while it
            // is still an editable draft).
            let comment_idx = argv
                .iter()
                .position(|line| line.starts_with("service-version update"))
                .expect("comment call");
            let stage_idx = argv
                .iter()
                .position(|line| line.starts_with("service-version stage"))
                .expect("stage call");
            assert!(comment_idx < stage_idx, "comment must precede staging");
            assert_eq!(
                argv[stage_idx],
                "service-version stage --service-id=SVC1 --version=7"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_ignores_ambient_store_name_overrides() {
        let (result, argv) = run_deploy_staged_with_fake_and_env(
            "SUCCESS: Updated package (service SVC1, version 7)",
            &["--edgezero-staging-config=app_config"],
            Some((
                "EDGEZERO__STORES__SECRETS__DEFAULT__NAME",
                "ambient_secrets",
            )),
        );
        result.expect("staged deploy succeeds");

        assert!(
            !argv.iter().any(|line| {
                line.contains("EDGEZERO__STORES__SECRETS__DEFAULT__NAME")
                    || line.contains("ambient_secrets")
            }),
            "staging must mirror persisted production mappings, not ambient process env: {argv:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_points_the_draft_at_the_staging_selector_store() {
        // The defect this closes: a clone inherits the active version's links,
        // so without a relink the staged version opens production's selector
        // store and reads PRODUCTION config -- `config push --staging` would
        // write a key nothing ever reads. The CLI threads the declared config
        // store as `--edgezero-staging-config=<logical>`.
        let (result, argv) = run_deploy_staged_with_fake(
            "SUCCESS: Updated package (service SVC1, version 7)",
            &["--edgezero-staging-config=app_config"],
        );
        result.expect("staged deploy must succeed");

        // The twin MIRRORS production: the non-selector override is copied
        // verbatim, and the config selector is upserted (redirected to
        // `app_config_staging` via stdin) into the staging store.
        assert!(
            argv.iter().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__SERVICES__SVC1__LOGGING__LEVEL"
            )),
            "production's non-config override must be mirrored into the twin: {argv:?}"
        );
        assert!(
            argv.iter().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__SERVICES__SVC1__STORES__CONFIG__APP_CONFIG__KEY"
            )),
            "the config selector must be written into the twin: {argv:?}"
        );
        // The mirror runs while the draft is still editable, before the relink.
        let mirror_idx = argv
            .iter()
            .position(|line| line.starts_with("config-store-entry update --store-id=STAGEID1"))
            .expect("mirror upsert");

        // The inherited production link is dropped: a version cannot hold two
        // links under one name.
        let delete_idx = argv
            .iter()
            .position(|line| line.starts_with("resource-link delete"))
            .expect("the inherited runtime-env link must be deleted");
        assert_eq!(
            argv[delete_idx],
            "resource-link delete --service-id=SVC1 --version=7 --id=LINK1"
        );

        // The staging STORE is linked under the name the runtime opens.
        let create_idx = argv
            .iter()
            .position(|line| line.starts_with("resource-link create"))
            .expect("the staging selector store must be linked");
        assert_eq!(
            argv[create_idx],
            "resource-link create --service-id=SVC1 --version=7 --resource-id=STAGEID1 --name=edgezero_runtime_env"
        );

        // Order matters: delete before create (name collision), and both while
        // the version is still an editable draft -- i.e. before staging.
        assert!(delete_idx < create_idx, "delete must precede create");
        assert!(
            mirror_idx < delete_idx,
            "the twin must be mirrored before the draft is relinked to it"
        );
        let stage_idx = argv
            .iter()
            .position(|line| line.starts_with("service-version stage"))
            .expect("stage call");
        assert!(
            create_idx < stage_idx,
            "the relink must happen while the version is still a draft"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_works_for_an_app_that_selects_no_config() {
        use std::os::unix::fs::PermissionsExt as _;

        // An app declaring no config stores threads no
        // `--edgezero-staging-config`, so there is no selector to isolate:
        // staging is still meaningful (staged CODE, no config), the draft keeps
        // the inherited link, and no config-store lookup happens at all.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        // No config stores at all on the account.
        fs::write(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\nelif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' '[]'\nfi\nexit 0\n",
        )
        .expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
        ])
        .expect("an app with no config selection must still be stageable");
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_auto_creates_the_staging_twin_when_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        // A staged deploy owns the twin end to end: if the account has no
        // staging store yet, the deploy creates it (rather than failing), so a
        // provisioned app can stage without a separate setup step.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let marker = dir.path().join("twin-created");
        let script_path = dir.path().join("fastly");
        // Stateful fake: `config-store list` includes the twin ONLY after a
        // `config-store create` has touched the marker.
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"create\" ]; then\n  \
               : > '{marker}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               if [ -f '{marker}' ]; then\n    \
                 printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}},{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n  \
               else\n    \
                 printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}}]'\n  \
               fi\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"update\" ]; then\n  \
               cat >/dev/null\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"LINK1\",\"name\":\"edgezero_runtime_env\"}}]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
            marker = marker.display(),
        );
        fs::write(&script_path, script).expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect("staged deploy must auto-create the twin and succeed");

        let argv = fs::read_to_string(&record).unwrap_or_default();
        assert!(
            argv.lines()
                .any(|line| line == "config-store create --name=edgezero_runtime_env_staging_SVC1"),
            "the per-service twin must be created on demand: {argv}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_isolates_when_config_declared_but_prod_store_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        // The app DECLARES config but has no `edgezero_runtime_env` store (never
        // provisioned an override store — production reads its default key). A
        // staged deploy must NOT silently inherit production config: it creates
        // the per-service twin, writes the `<logical>_staging` selector, and
        // relinks the draft to it. There is nothing to mirror (no production
        // entries), but staging is still isolated.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let marker = dir.path().join("twin-created");
        let script_path = dir.path().join("fastly");
        // No `edgezero_runtime_env` ever; the twin appears only after create.
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"create\" ]; then\n  \
               : > '{marker}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               if [ -f '{marker}' ]; then\n    \
                 printf '%s\\n' '[{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n  \
               else\n    \
                 printf '%s\\n' '[]'\n  \
               fi\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"update\" ]; then\n  \
               cat >/dev/null\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
            marker = marker.display(),
        );
        fs::write(&script_path, script).expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect("must isolate staging even with no production override store");

        let argv = fs::read_to_string(&record).unwrap_or_default();
        assert!(
            argv.lines().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__SERVICES__SVC1__STORES__CONFIG__APP_CONFIG__KEY"
            )),
            "the staging selector must be written even with no production store: {argv}"
        );
        assert!(
            argv.lines().any(|line| line.starts_with(
                "resource-link create --service-id=SVC1 --version=7 --resource-id=STAGEID1 --name=edgezero_runtime_env"
            )),
            "the draft must be relinked to the staging twin: {argv}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_fails_closed_when_config_store_list_is_unreadable() {
        use std::os::unix::fs::PermissionsExt as _;

        // If the store listing can't be parsed (a CLI schema change), we cannot
        // tell whether production config exists — refuse rather than risk a
        // staged version that silently serves PRODUCTION config.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        fs::write(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\nelif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' 'not json at all'\nfi\nexit 0\n",
        )
        .expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        let err = deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect_err("an unreadable config-store listing must fail closed");
        assert!(
            err.contains("Refusing to stage") || err.contains("could not parse"),
            "the error must explain the refusal: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_without_comment_makes_no_version_comment_call() {
        let (result, argv) =
            run_deploy_staged_with_fake("SUCCESS: Updated package (service SVC1, version 7)", &[]);
        result.expect("staged deploy must succeed");
        assert!(
            !argv
                .iter()
                .any(|line| line.starts_with("service-version update")),
            "no comment => no `service-version update` call: {argv:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_fails_closed_when_version_is_unparseable() {
        // A HIGHEST-version fallback here could silently adopt a version created
        // by a CONCURRENT deploy. We must error out instead of guessing.
        let (result, argv) = run_deploy_staged_with_fake("uploaded, but nothing parseable", &[]);
        let err = result.expect_err("unparseable version must fail closed");
        assert!(
            err.contains("could not determine the staged version"),
            "unexpected error: {err}"
        );
        assert!(
            !argv
                .iter()
                .any(|line| line.starts_with("service-version stage")),
            "must not stage a guessed version: {argv:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_does_not_duplicate_non_interactive_from_passthrough() {
        // `--non-interactive` is an allowlisted `compute update` flag, so a
        // caller-supplied one is FORWARDED. We must not then append our own:
        // passing the switch twice makes the Fastly CLI exit non-zero.
        let (result, argv) = run_deploy_staged_with_fake(
            "SUCCESS: Updated package (service SVC1, version 7)",
            &["--non-interactive"],
        );
        result.expect("staged deploy with a passthrough --non-interactive must succeed");
        let update = argv
            .iter()
            .find(|line| line.starts_with("compute update"))
            .expect("compute update was invoked");
        assert_eq!(
            update.matches("--non-interactive").count(),
            1,
            "the non-interactive switch must appear exactly once: {update}"
        );
    }
}
