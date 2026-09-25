//! `--format` output: stream routing, the JSON envelope, and the wire schema.
//!
//! The contract (documented in `docs/guide/cli-reference.md`, "Machine-readable
//! output"):
//!
//! - Every command emits its human-readable output through the logger, in
//!   both formats. Under `--format json`, [`OutputScope`] routes that output
//!   and every child process's stdout to stderr.
//! - stdout then carries exactly one [`Envelope`], written by [`finish`] when
//!   the command ends, on success AND failure.
//!
//! The wire structs below ARE the JSON schema (versioned by
//! [`SCHEMA_VERSION`]). They are deliberately separate from the adapter's
//! domain types, so refactoring those cannot change the public JSON by
//! accident.

use std::io::{self, Write as _};
use std::path::Path;

use edgezero_adapter::process;
use edgezero_adapter::registry::{
    AuthState, GcReport, HealthcheckOutcome, ProvisionAction, ProvisionReport, ProvisionStoreRef,
    ResolvedStoreId, RollbackOutcome, StoreKind,
};
use serde::Serialize;

use crate::args::OutputFormat;

/// Version of the whole JSON surface (envelope + every command result). Bump
/// only for a breaking change (removing / renaming / retyping a key, making a
/// non-null key nullable); additive changes keep it.
const SCHEMA_VERSION: u32 = 1;

/// The command an envelope reports on, spelled as the user types it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CommandName {
    ActiveVersion,
    AuthStatus,
    Build,
    ConfigGc,
    ConfigValidate,
    Deploy,
    Healthcheck,
    Provision,
    Rollback,
}

impl CommandName {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveVersion => "active-version",
            Self::AuthStatus => "auth status",
            Self::Build => "build",
            Self::ConfigGc => "config gc",
            Self::ConfigValidate => "config validate",
            Self::Deploy => "deploy",
            Self::Healthcheck => "healthcheck",
            Self::Provision => "provision",
            Self::Rollback => "rollback",
        }
    }
}

/// Routes stdout for one command run: under `--format json`, logger `info`
/// output and inheriting children's stdout go to stderr, and the previous
/// routing is restored on drop. A `text` scope changes nothing.
#[must_use = "the routing lasts only while the scope is alive"]
pub(crate) struct OutputScope {
    /// The routing to restore (`(child_stdout, info)`); `None` for `text`.
    previous: Option<(bool, bool)>,
}

impl OutputScope {
    pub(crate) fn enter(format: OutputFormat) -> Self {
        let previous = (format == OutputFormat::Json).then(|| {
            (
                process::set_child_stdout_to_stderr(true),
                crate::set_info_to_stderr(true),
            )
        });
        Self { previous }
    }
}

impl Drop for OutputScope {
    fn drop(&mut self) {
        if let Some((child_stdout, info)) = self.previous {
            process::set_child_stdout_to_stderr(child_stdout);
            crate::set_info_to_stderr(info);
        }
    }
}

/// A command failure: the message the binary prints, plus the partial result
/// when the command still measured something (an unhealthy probe, a partly
/// failed gc, an unauthenticated session).
#[derive(Debug)]
pub(crate) struct Failure<R> {
    message: String,
    /// Boxed so `Outcome<R>`'s error variant stays small.
    partial: Option<Box<R>>,
}

impl<R> Failure<R> {
    pub(crate) fn with_result(message: String, result: R) -> Self {
        Self {
            message,
            partial: Some(Box::new(result)),
        }
    }
}

impl<R> From<String> for Failure<R> {
    fn from(message: String) -> Self {
        Self {
            message,
            partial: None,
        }
    }
}

/// What a command produced: its result, or a failure.
pub(crate) type Outcome<R> = Result<R, Failure<R>>;

#[derive(Serialize)]
struct Envelope<'outcome, R> {
    command: &'static str,
    error: Option<ErrorBody<'outcome>>,
    ok: bool,
    result: Option<&'outcome R>,
    schema_version: u32,
}

#[derive(Serialize)]
struct ErrorBody<'msg> {
    message: &'msg str,
}

// ---------------------------------------------------------------- wire schema

/// `active-version` result.
#[derive(Debug, Serialize)]
pub(crate) struct ActiveVersionResult {
    adapter: String,
    service_id: String,
    version: Option<u64>,
}

impl ActiveVersionResult {
    pub(crate) fn new(adapter: &str, service_id: String, version: Option<u64>) -> Self {
        Self {
            adapter: adapter.to_owned(),
            service_id,
            version,
        }
    }
}

/// `auth status` result.
#[derive(Debug, Serialize)]
pub(crate) struct AuthStatusResult {
    adapter: String,
    state: WireAuthState,
}

impl AuthStatusResult {
    pub(crate) fn new(adapter: &str, state: AuthState) -> Self {
        Self {
            adapter: adapter.to_owned(),
            state: match state {
                AuthState::Authenticated => WireAuthState::Authenticated,
                AuthState::NotApplicable => WireAuthState::NotApplicable,
                AuthState::Unauthenticated => WireAuthState::Unauthenticated,
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum WireAuthState {
    Authenticated,
    NotApplicable,
    Unauthenticated,
}

/// `build` result.
#[derive(Debug, Serialize)]
pub(crate) struct BuildResult {
    adapter: String,
    artifact: Option<String>,
}

impl BuildResult {
    pub(crate) fn new(adapter: &str, artifact: Option<&Path>) -> Self {
        Self {
            adapter: adapter.to_owned(),
            artifact: artifact.map(path_string),
        }
    }
}

/// `deploy` result.
#[derive(Debug, Serialize)]
pub(crate) struct DeployResult {
    adapter: String,
    service_id: Option<String>,
    staging: bool,
    version: Option<u64>,
}

impl DeployResult {
    pub(crate) fn new(
        adapter: &str,
        service_id: Option<String>,
        staging: bool,
        version: Option<u64>,
    ) -> Self {
        Self {
            adapter: adapter.to_owned(),
            service_id,
            staging,
            version,
        }
    }
}

/// `healthcheck` result.
#[derive(Debug, Serialize)]
pub(crate) struct HealthcheckResult {
    adapter: String,
    attempts: u32,
    domain: String,
    healthy: bool,
    path: String,
    service_id: String,
    staging: bool,
    staging_ip: Option<String>,
    status_code: Option<u16>,
    version: u64,
    version_verified: bool,
}

impl HealthcheckResult {
    pub(crate) fn new(adapter: &str, outcome: &HealthcheckOutcome) -> Self {
        Self {
            adapter: adapter.to_owned(),
            attempts: outcome.attempts,
            domain: outcome.domain.clone(),
            healthy: outcome.healthy,
            path: outcome.path.clone(),
            service_id: outcome.service_id.clone(),
            staging: outcome.staging,
            staging_ip: outcome.staging_ip.clone(),
            status_code: outcome.status_code,
            version: outcome.version,
            version_verified: outcome.version_verified,
        }
    }
}

/// `rollback` result.
#[derive(Debug, Serialize)]
pub(crate) struct RollbackResult {
    adapter: String,
    rolled_back_to: Option<u64>,
    service_id: String,
    staging: bool,
    version: u64,
}

impl RollbackResult {
    pub(crate) fn new(adapter: &str, outcome: &RollbackOutcome) -> Self {
        Self {
            adapter: adapter.to_owned(),
            rolled_back_to: outcome.rolled_back_to,
            service_id: outcome.service_id.clone(),
            staging: outcome.staging,
            version: outcome.version,
        }
    }
}

/// `provision` result.
#[derive(Debug, Serialize)]
pub(crate) struct ProvisionResult {
    adapter: String,
    dry_run: bool,
    entries: Vec<ProvisionEntryResult>,
}

impl ProvisionResult {
    pub(crate) fn new(adapter: &str, dry_run: bool, report: &ProvisionReport) -> Self {
        Self {
            adapter: adapter.to_owned(),
            dry_run,
            entries: report
                .entries
                .iter()
                .map(|entry| ProvisionEntryResult {
                    action: WireProvisionAction::from(entry.action),
                    message: entry.message.clone(),
                    store: entry.store.as_ref().map(StoreResult::from),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ProvisionEntryResult {
    action: WireProvisionAction,
    message: String,
    store: Option<StoreResult>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum WireProvisionAction {
    AlreadyPresent,
    Created,
    NotApplicable,
    Note,
    Updated,
    WouldCreate,
    WouldUpdate,
}

impl From<ProvisionAction> for WireProvisionAction {
    fn from(action: ProvisionAction) -> Self {
        match action {
            ProvisionAction::AlreadyPresent => Self::AlreadyPresent,
            ProvisionAction::Created => Self::Created,
            ProvisionAction::NotApplicable => Self::NotApplicable,
            ProvisionAction::Note => Self::Note,
            ProvisionAction::Updated => Self::Updated,
            ProvisionAction::WouldCreate => Self::WouldCreate,
            ProvisionAction::WouldUpdate => Self::WouldUpdate,
        }
    }
}

#[derive(Debug, Serialize)]
struct StoreResult {
    kind: WireStoreKind,
    logical: Option<String>,
    platform: String,
}

impl From<&ProvisionStoreRef> for StoreResult {
    fn from(store: &ProvisionStoreRef) -> Self {
        Self {
            kind: match store.kind {
                StoreKind::Config => WireStoreKind::Config,
                StoreKind::Kv => WireStoreKind::Kv,
                StoreKind::Secrets => WireStoreKind::Secrets,
            },
            logical: store.logical.clone(),
            platform: store.platform.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum WireStoreKind {
    Config,
    Kv,
    Secrets,
}

/// `config gc` result.
#[derive(Debug, Serialize)]
pub(crate) struct GcResult {
    adapter: String,
    deleted: Option<usize>,
    dry_run: bool,
    failed: Vec<String>,
    kept_roots: Vec<String>,
    older_than_secs: Option<u64>,
    planned_deletions: Vec<GcCandidateResult>,
    store: GcStoreResult,
    stranded: Vec<String>,
    summary: GcSummaryResult,
    uncertain: Vec<String>,
    warnings: Vec<String>,
}

impl GcResult {
    pub(crate) fn new(
        adapter: &str,
        store: &ResolvedStoreId,
        dry_run: bool,
        older_than_secs: Option<u64>,
        report: &GcReport,
    ) -> Self {
        Self {
            adapter: adapter.to_owned(),
            deleted: report.deleted,
            dry_run,
            failed: report.failed.clone(),
            kept_roots: report.kept_roots.clone(),
            older_than_secs,
            planned_deletions: report
                .planned
                .iter()
                .map(|candidate| GcCandidateResult {
                    age_secs: candidate.age_secs,
                    key: candidate.key.clone(),
                })
                .collect(),
            store: GcStoreResult {
                id: report.store_id.clone(),
                logical: store.logical.clone(),
                platform: store.platform.clone(),
            },
            stranded: report.stranded.clone(),
            summary: GcSummaryResult {
                entries: report.entries,
                generations_planned: report.generations_planned,
                orphans_planned: report.planned.len(),
                orphans_too_recent: report.retained_recent,
                referenced_chunks: report.referenced_chunks,
                roots: report.roots,
                unprovable: report.unprovable,
            },
            uncertain: report.uncertain.clone(),
            warnings: report.warnings.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct GcCandidateResult {
    age_secs: u64,
    key: String,
}

#[derive(Debug, Serialize)]
struct GcStoreResult {
    id: Option<String>,
    logical: String,
    platform: String,
}

#[derive(Debug, Serialize)]
struct GcSummaryResult {
    entries: usize,
    generations_planned: usize,
    orphans_planned: usize,
    orphans_too_recent: usize,
    referenced_chunks: usize,
    roots: usize,
    unprovable: usize,
}

/// `config validate` result. Present only when validation passed.
#[derive(Debug, Serialize)]
pub(crate) struct ValidateResult {
    app_config: Option<String>,
    app_name: String,
    manifest: String,
    mode: ValidateMode,
    strict: bool,
}

impl ValidateResult {
    /// `app_config` is `None` for the raw (untyped) flow.
    pub(crate) fn new(
        manifest: &Path,
        app_config: Option<&Path>,
        app_name: &str,
        strict: bool,
    ) -> Self {
        Self {
            app_config: app_config.map(path_string),
            app_name: app_name.to_owned(),
            manifest: path_string(manifest),
            mode: if app_config.is_some() {
                ValidateMode::Typed
            } else {
                ValidateMode::Raw
            },
            strict,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ValidateMode {
    Raw,
    Typed,
}

// ---------------------------------------------------------------- rendering

/// Render `outcome` as the pretty-printed JSON envelope.
fn render_envelope<R: Serialize>(
    command: CommandName,
    outcome: &Outcome<R>,
) -> Result<String, String> {
    let envelope = match outcome {
        Ok(result) => Envelope {
            command: command.as_str(),
            error: None,
            ok: true,
            result: Some(result),
            schema_version: SCHEMA_VERSION,
        },
        Err(failure) => Envelope {
            command: command.as_str(),
            error: Some(ErrorBody {
                message: &failure.message,
            }),
            ok: false,
            result: failure.partial.as_deref(),
            schema_version: SCHEMA_VERSION,
        },
    };
    serde_json::to_string_pretty(&envelope)
        .map_err(|err| format!("failed to render the JSON result: {err}"))
}

/// End a command: under `--format json` write the envelope to stdout, then
/// return the `Result` the binary's `main` turns into an exit code.
///
/// The envelope is written here, inside the library, so downstream CLIs whose
/// `main` predates `--format` still emit failure envelopes.
pub(crate) fn finish<R: Serialize>(
    command: CommandName,
    format: OutputFormat,
    outcome: Outcome<R>,
) -> Result<(), String> {
    if format == OutputFormat::Json {
        let document = render_envelope(command, &outcome)?;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{document}")
            .and_then(|()| stdout.flush())
            .map_err(|err| format!("failed to write the JSON result to stdout: {err}"))?;
    }
    outcome.map(|_| ()).map_err(|failure| failure.message)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
#[expect(
    clippy::default_numeric_fallback,
    reason = "integer literals in the `json!` expectations compare by value; their Rust type is irrelevant"
)]
mod tests {
    use super::*;
    use crate::test_support::manifest_guard;
    use edgezero_adapter::registry::{GcCandidate, ProvisionEntry};
    use serde_json::{Value, json};

    fn envelope_of<R: Serialize>(command: CommandName, outcome: &Outcome<R>) -> Value {
        let rendered = render_envelope(command, outcome).expect("renders");
        serde_json::from_str(&rendered).expect("valid JSON")
    }

    /// The §6.1 invariants every envelope must satisfy.
    fn assert_envelope_invariants(envelope: &Value) {
        let object = envelope.as_object().expect("envelope is an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["command", "error", "ok", "result", "schema_version"]);
        assert_eq!(envelope["schema_version"], json!(1));
        let ok = envelope["ok"].as_bool().expect("ok is a bool");
        assert_eq!(
            ok,
            envelope["error"].is_null(),
            "error is null exactly when ok"
        );
        if ok {
            assert!(!envelope["result"].is_null(), "a success carries a result");
        } else {
            assert!(envelope["error"]["message"].is_string());
        }
    }

    #[test]
    fn success_envelope_carries_the_result() {
        let outcome: Outcome<ActiveVersionResult> = Ok(ActiveVersionResult::new(
            "fastly",
            "SVC1".to_owned(),
            Some(42),
        ));
        let envelope = envelope_of(CommandName::ActiveVersion, &outcome);
        assert_envelope_invariants(&envelope);
        assert_eq!(envelope["command"], json!("active-version"));
        assert_eq!(
            envelope["result"],
            json!({"adapter": "fastly", "service_id": "SVC1", "version": 42})
        );
    }

    #[test]
    fn failure_envelope_without_partial_result_has_null_result() {
        let outcome: Outcome<ActiveVersionResult> = Err(Failure::from("boom".to_owned()));
        let envelope = envelope_of(CommandName::ActiveVersion, &outcome);
        assert_envelope_invariants(&envelope);
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["error"], json!({"message": "boom"}));
        assert!(envelope["result"].is_null());
    }

    #[test]
    fn failure_envelope_keeps_the_partial_result() {
        let outcome = HealthcheckOutcome {
            attempts: 3,
            domain: "app.example.com".to_owned(),
            failure: Some("healthcheck failed".to_owned()),
            healthy: false,
            path: "/".to_owned(),
            service_id: "SVC1".to_owned(),
            staging: true,
            staging_ip: Some("151.101.2.10".to_owned()),
            status_code: Some(503),
            version: 7,
            version_verified: false,
        };
        let failed: Outcome<HealthcheckResult> = Err(Failure::with_result(
            "healthcheck failed".to_owned(),
            HealthcheckResult::new("fastly", &outcome),
        ));
        let envelope = envelope_of(CommandName::Healthcheck, &failed);
        assert_envelope_invariants(&envelope);
        assert_eq!(
            envelope["result"],
            json!({
                "adapter": "fastly", "attempts": 3, "domain": "app.example.com",
                "healthy": false, "path": "/", "service_id": "SVC1", "staging": true,
                "staging_ip": "151.101.2.10", "status_code": 503, "version": 7,
                "version_verified": false
            })
        );
    }

    #[test]
    fn every_command_name_is_spelled_as_typed() {
        let names: Vec<&str> = [
            CommandName::ActiveVersion,
            CommandName::AuthStatus,
            CommandName::Build,
            CommandName::ConfigGc,
            CommandName::ConfigValidate,
            CommandName::Deploy,
            CommandName::Healthcheck,
            CommandName::Provision,
            CommandName::Rollback,
        ]
        .into_iter()
        .map(CommandName::as_str)
        .collect();
        assert_eq!(
            names,
            [
                "active-version",
                "auth status",
                "build",
                "config gc",
                "config validate",
                "deploy",
                "healthcheck",
                "provision",
                "rollback"
            ]
        );
    }

    #[test]
    fn auth_build_deploy_rollback_results_serialize_every_key() {
        let auth = serde_json::to_value(AuthStatusResult::new("axum", AuthState::NotApplicable))
            .expect("serialize");
        assert_eq!(auth, json!({"adapter": "axum", "state": "not_applicable"}));

        let build = serde_json::to_value(BuildResult::new("cloudflare", None)).expect("serialize");
        assert_eq!(build, json!({"adapter": "cloudflare", "artifact": null}));

        let deploy = serde_json::to_value(DeployResult::new("fastly", None, true, Some(43)))
            .expect("serialize");
        assert_eq!(
            deploy,
            json!({"adapter": "fastly", "service_id": null, "staging": true, "version": 43})
        );

        let rollback = serde_json::to_value(RollbackResult::new(
            "fastly",
            &RollbackOutcome {
                rolled_back_to: None,
                service_id: "SVC1".to_owned(),
                staging: true,
                version: 43,
            },
        ))
        .expect("serialize");
        assert_eq!(
            rollback,
            json!({"adapter": "fastly", "rolled_back_to": null, "service_id": "SVC1", "staging": true, "version": 43})
        );
    }

    #[test]
    fn provision_result_maps_actions_and_stores() {
        let store = ResolvedStoreId::new("sessions", "prod_sessions");
        let report = ProvisionReport {
            entries: vec![
                ProvisionEntry::for_store(
                    ProvisionAction::WouldCreate,
                    StoreKind::Kv,
                    &store,
                    "would create".to_owned(),
                ),
                ProvisionEntry::note("nothing else".to_owned()),
            ],
        };
        let result =
            serde_json::to_value(ProvisionResult::new("fastly", true, &report)).expect("serialize");
        assert_eq!(
            result,
            json!({
                "adapter": "fastly",
                "dry_run": true,
                "entries": [
                    {"action": "would_create", "message": "would create",
                     "store": {"kind": "kv", "logical": "sessions", "platform": "prod_sessions"}},
                    {"action": "note", "message": "nothing else", "store": null}
                ]
            })
        );
    }

    #[test]
    fn gc_result_summarises_the_report() {
        let report = GcReport {
            deleted: None,
            entries: 40,
            generations_planned: 1,
            kept_roots: vec!["app_config".to_owned()],
            planned: vec![GcCandidate {
                age_secs: 90_211,
                key: "app_config.__chunk.a".to_owned(),
            }],
            referenced_chunks: 12,
            retained_recent: 2,
            roots: 3,
            store_id: Some("7Ab".to_owned()),
            text_lines: vec!["not on the wire".to_owned()],
            ..GcReport::default()
        };
        let store = ResolvedStoreId::from_logical("app_config");
        let result = serde_json::to_value(GcResult::new("fastly", &store, true, None, &report))
            .expect("serialize");
        assert_eq!(
            result,
            json!({
                "adapter": "fastly", "deleted": null, "dry_run": true, "failed": [],
                "kept_roots": ["app_config"], "older_than_secs": null,
                "planned_deletions": [{"age_secs": 90_211, "key": "app_config.__chunk.a"}],
                "store": {"id": "7Ab", "logical": "app_config", "platform": "app_config"},
                "stranded": [],
                "summary": {"entries": 40, "generations_planned": 1, "orphans_planned": 1,
                            "orphans_too_recent": 2, "referenced_chunks": 12, "roots": 3,
                            "unprovable": 0},
                "uncertain": [], "warnings": []
            })
        );
    }

    #[test]
    fn validate_result_reports_mode_from_the_app_config() {
        let raw = serde_json::to_value(ValidateResult::new(
            Path::new("edgezero.toml"),
            None,
            "demo",
            true,
        ))
        .expect("serialize");
        assert_eq!(
            raw,
            json!({"app_config": null, "app_name": "demo", "manifest": "edgezero.toml", "mode": "raw", "strict": true})
        );
        let typed = serde_json::to_value(ValidateResult::new(
            Path::new("edgezero.toml"),
            Some(Path::new("demo.toml")),
            "demo",
            false,
        ))
        .expect("serialize");
        assert_eq!(typed["mode"], json!("typed"));
        assert_eq!(typed["app_config"], json!("demo.toml"));
    }

    #[test]
    fn output_scope_routes_only_while_alive() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let text = OutputScope::enter(OutputFormat::Text);
        assert!(!process::child_stdout_to_stderr(), "text changes nothing");
        drop(text);
        let json = OutputScope::enter(OutputFormat::Json);
        assert!(process::child_stdout_to_stderr());
        drop(json);
        assert!(!process::child_stdout_to_stderr(), "restored on drop");
    }
}
