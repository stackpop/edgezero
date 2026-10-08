//! SDK-free native Cargo selection and exact generated Axum command matching.

use std::collections::BTreeSet;

/// Normalized Cargo feature and target inputs shared by native CLI paths.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[must_use]
pub struct NativeCargoSelection {
    /// Whether Cargo should enable every package feature.
    pub all_features: bool,
    /// Non-feature Cargo args before `--`.
    pub cargo_args: Vec<String>,
    /// Whether Cargo should suppress package defaults.
    pub no_default_features: bool,
    /// Explicit additions, including `[adapters.axum.build].features`.
    pub requested_features: Vec<String>,
    /// Arguments after Cargo's `--`, for `cargo run` only.
    pub run_args: Vec<String>,
    /// Cargo target triple. `None` means Cargo's host target.
    pub target: Option<String>,
}

impl NativeCargoSelection {
    /// Cargo arguments in normalized form; runtime args are intentionally omitted.
    #[inline]
    #[must_use]
    pub fn cargo_args(&self) -> Vec<String> {
        let mut args = self.cargo_args.clone();
        if !self.requested_features.is_empty() {
            args.extend(["--features".to_owned(), self.requested_features.join(",")]);
        }
        args
    }

    /// Merge manifest features with Cargo feature controls, preserving unrelated
    /// arguments and separating runtime args after `--`.
    ///
    /// # Errors
    /// Returns an error for missing/empty feature lists, missing or conflicting
    /// targets, or the non-Cargo `native` target sentinel.
    #[inline]
    pub fn normalize_args(manifest_features: &[String], args: &[String]) -> Result<Self, String> {
        let mut selection = Self::default();
        let mut feature_values = Vec::new();
        let mut pending_value: Option<&str> = None;
        let mut after_separator = false;

        for arg in args {
            if let Some(option) = pending_value.take() {
                if option == "--target" {
                    selection.set_target(arg)?;
                    selection
                        .cargo_args
                        .extend([option.to_owned(), arg.clone()]);
                } else {
                    feature_values.extend(split_features(arg)?);
                }
                continue;
            }
            if after_separator {
                selection.run_args.push(arg.clone());
                continue;
            }
            match arg.as_str() {
                "--" => after_separator = true,
                "--features" | "-F" | "--target" => pending_value = Some(arg),
                "--all-features" => {
                    selection.all_features = true;
                    selection.cargo_args.push(arg.clone());
                }
                "--no-default-features" => {
                    selection.no_default_features = true;
                    selection.cargo_args.push(arg.clone());
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--features=") {
                        feature_values.extend(split_features(value)?);
                    } else if let Some(value) = arg.strip_prefix("-F") {
                        if value.is_empty() {
                            selection.cargo_args.push(arg.clone());
                        } else {
                            feature_values.extend(split_features(value)?);
                        }
                    } else if let Some(value) = arg.strip_prefix("--target=") {
                        selection.set_target(value)?;
                        selection.cargo_args.push(arg.clone());
                    } else {
                        selection.cargo_args.push(arg.clone());
                    }
                }
            }
        }
        if let Some(option) = pending_value {
            return Err(if option == "--target" {
                "--target requires a triple".to_owned()
            } else {
                format!("{option} requires a feature list")
            });
        }

        let mut seen = BTreeSet::new();
        for feature in manifest_features.iter().chain(feature_values.iter()) {
            for normalized in split_features(feature)? {
                if seen.insert(normalized.clone()) {
                    selection.requested_features.push(normalized);
                }
            }
        }
        Ok(selection)
    }

    fn set_target(&mut self, target: &str) -> Result<(), String> {
        if target.trim().is_empty() {
            return Err("--target requires a non-empty target triple".to_owned());
        }
        if target == "native" {
            return Err("`native` selects Cargo's host target; omit `--target`".to_owned());
        }
        match &self.target {
            Some(previous) if previous != target => {
                Err("conflicting --target selections; refusing to silently choose one".to_owned())
            }
            _ => {
                self.target = Some(target.to_owned());
                Ok(())
            }
        }
    }
}

/// Recognize only the exact generated Axum command base, never arbitrary shell text.
///
/// # Examples
/// ```
/// assert_eq!(
///     edgezero_adapter_axum::native_build::generated_axum_cargo_action("cargo build -p app"),
///     Some(("build", "app")),
/// );
/// ```
#[inline]
#[must_use]
pub fn generated_axum_cargo_action(command: &str) -> Option<(&'static str, &str)> {
    let mut words = command.split_ascii_whitespace();
    let (Some("cargo"), Some(action_word), Some("-p"), Some(package), None) = (
        words.next(),
        words.next(),
        words.next(),
        words.next(),
        words.next(),
    ) else {
        return None;
    };
    let action = match action_word {
        "build" => "build",
        "run" => "run",
        _ => return None,
    };
    if !valid_package_token(package) || command != format!("cargo {action} -p {package}") {
        return None;
    }
    Some((action, package))
}

fn split_features(value: &str) -> Result<Vec<String>, String> {
    let values: Vec<_> = value
        .split(|ch: char| ch == ',' || ch.is_ascii_whitespace())
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
        .collect();
    if values.is_empty() {
        return Err("Cargo feature list must not be empty".to_owned());
    }
    Ok(values)
}

fn valid_package_token(package: &str) -> bool {
    let mut bytes = package.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::{NativeCargoSelection, generated_axum_cargo_action};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn controls_after_separator_are_runtime_args() {
        let selection = NativeCargoSelection::normalize_args(
            &[],
            &strings(&["--features", "compile", "--", "--features", "runtime"]),
        )
        .expect("feature controls should parse");
        assert_eq!(selection.requested_features, strings(&["compile"]));
        assert_eq!(selection.run_args, strings(&["--features", "runtime"]));
    }

    #[test]
    fn normalizes_repeated_features_and_preserves_cargo_controls() {
        let selection = NativeCargoSelection::normalize_args(
            &strings(&["aws-appconfig-agent", "shared"]),
            &strings(&[
                "--features",
                "shared,aws-secrets-manager",
                "-F",
                "logging extra\ttabbed",
                "--features=metrics",
                "-Fcache",
                "--all-features",
                "--no-default-features",
                "--target",
                "aarch64-unknown-linux-gnu",
                "--locked",
            ]),
        )
        .expect("Cargo controls should normalize");
        assert_eq!(
            selection.requested_features,
            strings(&[
                "aws-appconfig-agent",
                "shared",
                "aws-secrets-manager",
                "logging",
                "extra",
                "tabbed",
                "metrics",
                "cache",
            ])
        );
        assert!(selection.all_features);
        assert!(selection.no_default_features);
        assert_eq!(
            selection.target.as_deref(),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            selection.cargo_args(),
            strings(&[
                "--all-features",
                "--no-default-features",
                "--target",
                "aarch64-unknown-linux-gnu",
                "--locked",
                "--features",
                "aws-appconfig-agent,shared,aws-secrets-manager,logging,extra,tabbed,metrics,cache",
            ])
        );
    }

    #[test]
    fn recognizes_only_exact_generated_package_commands() {
        assert_eq!(
            generated_axum_cargo_action("cargo build -p app_cli"),
            Some(("build", "app_cli"))
        );
        assert_eq!(
            generated_axum_cargo_action("cargo run -p app-cli"),
            Some(("run", "app-cli"))
        );
        for command in [
            "cargo build -p app; touch /tmp/x",
            "cargo build -p app | cat",
            "cargo build -p 'app'",
            "cargo build -p caf\u{e9}",
            "cargo build -p app.extra",
            "CARGO build -p app",
            "cargo run -p app -- --help",
            "cargo\tbuild -p app",
        ] {
            assert_eq!(generated_axum_cargo_action(command), None, "{command}");
        }
    }

    #[test]
    fn rejects_missing_features_and_invalid_or_conflicting_targets() {
        assert_eq!(
            NativeCargoSelection::normalize_args(&[], &strings(&["--features"]))
                .expect_err("missing --features value must fail"),
            "--features requires a feature list"
        );
        assert_eq!(
            NativeCargoSelection::normalize_args(&[], &strings(&["-F"]))
                .expect_err("missing -F value must fail"),
            "-F requires a feature list"
        );
        assert_eq!(
            NativeCargoSelection::normalize_args(
                &[],
                &strings(&[
                    "--target",
                    "x86_64-unknown-linux-gnu",
                    "--target=aarch64-unknown-linux-gnu",
                ]),
            )
            .expect_err("conflicting targets must fail"),
            "conflicting --target selections; refusing to silently choose one"
        );
        assert_eq!(
            NativeCargoSelection::normalize_args(&[], &strings(&["--target=native"]))
                .expect_err("native is not a Cargo target triple"),
            "`native` selects Cargo's host target; omit `--target`"
        );
    }
}
