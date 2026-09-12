use crate::{Result, require};
use std::{collections::BTreeMap, ffi::OsString};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    WriteExpected {
        work_root: String,
        app_repo_id: String,
        source_revision: String,
        app_cli_package: String,
        app_cli_bin: String,
        workspace_id: String,
        platform_id: String,
        provenance_protocol: String,
        output: String,
    },
    WriteReleaseRequest {
        work_root: String,
        gate_sha: String,
        provenance_protocol: String,
        release_tag: String,
        output: String,
    },
    Package {
        work_root: String,
        binary: String,
        schema: String,
        expected: String,
        app_cli_version: String,
        archive: String,
    },
    Validate {
        work_root: String,
        archive: String,
        schema: String,
        expected: String,
        output: String,
    },
    SelfTest {
        fixtures: String,
    },
}

pub(crate) fn parse<I, S>(arguments: I) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut arguments = arguments
        .into_iter()
        .map(|argument| {
            argument
                .into()
                .into_string()
                .map_err(|_| "arguments must be UTF-8".to_string())
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter();
    let name = arguments.next().ok_or("missing command")?;
    match name.as_str() {
        "write-expected" => {
            let mut flags = flags(
                arguments,
                &[
                    "--work-root",
                    "--app-repo-id",
                    "--source-revision",
                    "--app-cli-package",
                    "--app-cli-bin",
                    "--workspace-id",
                    "--platform-id",
                    "--provenance-protocol",
                    "--output",
                ],
            )?;
            Ok(Command::WriteExpected {
                work_root: take(&mut flags, "--work-root"),
                app_repo_id: take(&mut flags, "--app-repo-id"),
                source_revision: take(&mut flags, "--source-revision"),
                app_cli_package: take(&mut flags, "--app-cli-package"),
                app_cli_bin: take(&mut flags, "--app-cli-bin"),
                workspace_id: take(&mut flags, "--workspace-id"),
                platform_id: take(&mut flags, "--platform-id"),
                provenance_protocol: take(&mut flags, "--provenance-protocol"),
                output: take(&mut flags, "--output"),
            })
        }
        "write-release-request" => {
            let mut flags = flags(
                arguments,
                &[
                    "--work-root",
                    "--gate-sha",
                    "--provenance-protocol",
                    "--release-tag",
                    "--output",
                ],
            )?;
            Ok(Command::WriteReleaseRequest {
                work_root: take(&mut flags, "--work-root"),
                gate_sha: take(&mut flags, "--gate-sha"),
                provenance_protocol: take(&mut flags, "--provenance-protocol"),
                release_tag: take(&mut flags, "--release-tag"),
                output: take(&mut flags, "--output"),
            })
        }
        "package" => {
            let mut flags = flags(
                arguments,
                &[
                    "--work-root",
                    "--binary",
                    "--schema",
                    "--expected",
                    "--app-cli-version",
                    "--archive",
                ],
            )?;
            Ok(Command::Package {
                work_root: take(&mut flags, "--work-root"),
                binary: take(&mut flags, "--binary"),
                schema: take(&mut flags, "--schema"),
                expected: take(&mut flags, "--expected"),
                app_cli_version: take(&mut flags, "--app-cli-version"),
                archive: take(&mut flags, "--archive"),
            })
        }
        "validate" => {
            let mut flags = flags(
                arguments,
                &[
                    "--work-root",
                    "--archive",
                    "--schema",
                    "--expected",
                    "--output",
                ],
            )?;
            Ok(Command::Validate {
                work_root: take(&mut flags, "--work-root"),
                archive: take(&mut flags, "--archive"),
                schema: take(&mut flags, "--schema"),
                expected: take(&mut flags, "--expected"),
                output: take(&mut flags, "--output"),
            })
        }
        "self-test" => {
            let mut flags = flags(arguments, &["--fixtures"])?;
            Ok(Command::SelfTest {
                fixtures: take(&mut flags, "--fixtures"),
            })
        }
        _ => Err(format!("unknown command: {name}")),
    }
}

fn flags(
    mut arguments: impl Iterator<Item = String>,
    expected: &[&str],
) -> Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    while let Some(flag) = arguments.next() {
        require(
            flag.starts_with("--") && !flag.contains('='),
            "malformed flag",
        )?;
        require(expected.contains(&flag.as_str()), "unknown or mixed flag")?;
        let value = arguments
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        require(
            parsed.insert(flag.clone(), value).is_none(),
            "duplicate flag",
        )?;
    }
    for flag in expected {
        require(parsed.contains_key(*flag), &format!("missing flag: {flag}"))?;
    }
    Ok(parsed)
}

fn take(flags: &mut BTreeMap<String, String>, name: &str) -> String {
    flags.remove(name).expect("required flag was checked")
}
