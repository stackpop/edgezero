//! Shell out to `spin cloud key-value set` to seed Fermyon Cloud KV
//! stores during `config push --adapter spin`.
//!
//! Fermyon Cloud is the only Spin deployment target with a first-class
//! external KV write API, and it's already gated by `spin cloud login`
//! (the operator authenticates the platform CLI separately, same
//! pattern as `wrangler` for Cloudflare and `fastly` for Fastly).
//! We shell out per entry; the Spin Cloud plugin handles
//! authentication, retry, and rate limiting.
//!
//! Auto-detection: the dispatcher activates this writer when the
//! manifest's `[adapters.spin.commands].deploy` shells to `spin deploy`
//! or `spin cloud deploy` (operator intent: this app deploys to
//! Fermyon Cloud), AND the operator did NOT pass `--local`.

use std::io;
use std::process::Command;

/// Detect whether the spin adapter's deploy command targets Fermyon
/// Cloud. Looks for `spin deploy` or `spin cloud deploy` as a substring
/// of the configured command. Substring match (not equality) so a
/// pre-deploy hook like `cd dist && spin deploy --provider …` still
/// trips it.
#[must_use]
pub(crate) fn deploy_command_targets_fermyon_cloud(deploy_cmd: Option<&str>) -> bool {
    let Some(cmd) = deploy_cmd else {
        return false;
    };
    cmd.contains("spin cloud deploy") || cmd.contains("spin deploy")
}

/// Shell out `spin cloud key-value set --store <label> <key> <value>`
/// for each entry. Captures stderr on non-zero exit and surfaces it
/// in the error string. Detects "not logged in" specifically and
/// points the operator at `spin cloud login`.
///
/// # Errors
/// Returns a human-readable error string on:
/// - failure to spawn the `spin` binary (typically "command not
///   found" — the install hint points at the Spin install URL);
/// - non-zero exit from `spin cloud key-value set` (captured stderr
///   included);
/// - "not logged in" stderr (suggests `spin cloud login`).
pub(crate) fn write_batch(store_label: &str, entries: &[(String, String)]) -> Result<(), String> {
    for (key, value) in entries {
        let output = Command::new("spin")
            .args([
                "cloud",
                "key-value",
                "set",
                "--store",
                store_label,
                key,
                value,
            ])
            .output()
            .map_err(|err| {
                if err.kind() == io::ErrorKind::NotFound {
                    "`spin` is not on PATH. Install the Spin CLI (https://spinframework.dev/) and re-run.".to_owned()
                } else {
                    format!("failed to invoke `spin cloud key-value set`: {err}")
                }
            })?;

        if output.status.success() {
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("not logged in")
            || stderr.to_ascii_lowercase().contains("authentication")
        {
            return Err(format!(
                "`spin cloud key-value set` reports not authenticated. Run `spin cloud login` and retry. Stderr: {stderr}"
            ));
        }
        return Err(format!(
            "`spin cloud key-value set --store {store_label} {key} …` failed (status {}): {stderr}",
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_fermyon_cloud_from_spin_deploy() {
        assert!(deploy_command_targets_fermyon_cloud(Some("spin deploy")));
        assert!(deploy_command_targets_fermyon_cloud(Some(
            "spin deploy --from crates/foo"
        )));
        assert!(deploy_command_targets_fermyon_cloud(Some(
            "cd dist && spin deploy"
        )));
    }

    #[test]
    fn detect_fermyon_cloud_from_spin_cloud_deploy() {
        assert!(deploy_command_targets_fermyon_cloud(Some(
            "spin cloud deploy"
        )));
    }

    #[test]
    fn non_cloud_deploy_commands_are_not_detected() {
        assert!(!deploy_command_targets_fermyon_cloud(Some("echo no-op")));
        assert!(!deploy_command_targets_fermyon_cloud(Some(
            "kubectl apply -f spin.yaml"
        )));
        // Sanity: just having "spin" or "deploy" alone doesn't count.
        assert!(!deploy_command_targets_fermyon_cloud(Some("spin build")));
        assert!(!deploy_command_targets_fermyon_cloud(Some("./deploy.sh")));
    }

    #[test]
    fn missing_deploy_command_returns_false() {
        assert!(!deploy_command_targets_fermyon_cloud(None));
    }
}
