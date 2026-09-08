use edgezero_provenance_validator::{ExecutionLayout, archive, execute, json_contract::Expected};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::Cursor,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

const SCHEMA: &[u8] = include_bytes!("../../../docker/build-app-cli/provenance.schema.json");
const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docker/build-app-cli/fixtures/provenance"
);
const SOURCE: &str = "1111111111111111111111111111111111111111";
const GATE: &str = "2222222222222222222222222222222222222222";
const WORKSPACE: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
const PLATFORM: &str = "sha256:4444444444444444444444444444444444444444444444444444444444444444";

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "edgezero-provenance-cli-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.0.exists() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

struct Harness {
    temp: TempDir,
    layout: ExecutionLayout,
}

impl Harness {
    fn new() -> Self {
        let temp = TempDir::new();
        fs::create_dir(temp.path().join("work")).unwrap();
        let schema = temp
            .path()
            .join("usr/local/share/edgezero/provenance.schema.json");
        fs::create_dir_all(schema.parent().unwrap()).unwrap();
        fs::write(&schema, SCHEMA).unwrap();
        let layout = ExecutionLayout::rooted_at(temp.path());
        Self { temp, layout }
    }

    fn path(&self, container_path: &str) -> PathBuf {
        assert!(container_path.starts_with('/'));
        self.temp
            .path()
            .join(container_path.trim_start_matches('/'))
    }

    fn mkdir(&self, container_path: &str) {
        fs::create_dir_all(self.path(container_path)).unwrap();
    }

    fn write(&self, container_path: &str, bytes: &[u8]) {
        let path = self.path(container_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn run(&self, arguments: &[String]) -> edgezero_provenance_validator::Result<()> {
        execute(&self.layout, arguments.iter().cloned())
    }

    fn install_fixtures(&self) {
        copy_tree(
            Path::new(FIXTURES),
            &self.path("/usr/local/share/edgezero/provenance-fixtures"),
        );
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn expected(repo: &str) -> Vec<u8> {
    Expected::new(
        repo,
        SOURCE,
        "edgezero-cli",
        "edgezero",
        WORKSPACE,
        PLATFORM,
    )
    .unwrap()
    .canonical_bytes()
    .unwrap()
}

fn write_expected_args() -> Vec<String> {
    strings(&[
        "write-expected",
        "--work-root",
        "/work",
        "--app-repo-id",
        "123456",
        "--source-revision",
        SOURCE,
        "--app-cli-package",
        "edgezero-cli",
        "--app-cli-bin",
        "edgezero",
        "--workspace-id",
        WORKSPACE,
        "--platform-id",
        PLATFORM,
        "--provenance-protocol",
        "1",
        "--output",
        "/work/expected/expected.json",
    ])
}

fn write_release_args() -> Vec<String> {
    strings(&[
        "write-release-request",
        "--work-root",
        "/work",
        "--gate-sha",
        GATE,
        "--provenance-protocol",
        "1",
        "--release-tag",
        "build-container-v7",
        "--output",
        "/work/release/release-request.json",
    ])
}

fn package_args() -> Vec<String> {
    strings(&[
        "package",
        "--work-root",
        "/work",
        "--binary",
        "/work/input/app-cli",
        "--schema",
        "/usr/local/share/edgezero/provenance.schema.json",
        "--expected",
        "/work/input/expected.json",
        "--app-cli-version",
        "0.1.0",
        "--archive",
        "/work/packaged/artifact.tar",
    ])
}

fn validate_args() -> Vec<String> {
    strings(&[
        "validate",
        "--work-root",
        "/work",
        "--archive",
        "/work/input/artifact.tar",
        "--schema",
        "/usr/local/share/edgezero/provenance.schema.json",
        "--expected",
        "/work/input/expected.json",
        "--output",
        "/work/validated/app-cli",
    ])
}

fn self_test_args() -> Vec<String> {
    strings(&[
        "self-test",
        "--fixtures",
        "/usr/local/share/edgezero/provenance-fixtures",
    ])
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn replace_flag(arguments: &mut [String], flag: &str, value: &str) {
    let index = arguments.iter().position(|item| item == flag).unwrap();
    arguments[index + 1] = value.into();
}

fn prepare_package(harness: &Harness, repo: &str) {
    harness.mkdir("/work/input");
    harness.mkdir("/work/packaged");
    harness.write("/work/input/expected.json", &expected(repo));
    harness.write("/work/input/app-cli", &static_elf(0x800));
}

fn package_once(repo: &str) -> (Harness, Vec<u8>) {
    let harness = Harness::new();
    prepare_package(&harness, repo);
    harness.run(&package_args()).unwrap();
    let bytes = fs::read(harness.path("/work/packaged/artifact.tar")).unwrap();
    (harness, bytes)
}

fn prepare_validate(harness: &Harness, repo: &str, archive: &[u8]) {
    harness.mkdir("/work/input");
    harness.mkdir("/work/validated");
    harness.write("/work/input/expected.json", &expected(repo));
    harness.write("/work/input/artifact.tar", archive);
}

fn rewrite_metadata(archive_bytes: &[u8], change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut input = Cursor::new(archive_bytes);
    let parsed = archive::parse(&mut input).unwrap();
    let start = usize::try_from(parsed.binary_offset).unwrap();
    let end = start + usize::try_from(parsed.binary_size).unwrap();
    let mut metadata: Value = serde_json::from_slice(&parsed.metadata).unwrap();
    change(&mut metadata);
    let metadata = serde_json::to_vec(&metadata).unwrap();
    let mut output = Vec::new();
    archive::encode(
        &mut output,
        &mut Cursor::new(&metadata),
        metadata.len() as u64,
        &mut Cursor::new(&archive_bytes[start..end]),
        parsed.binary_size,
    )
    .unwrap();
    output
}

#[test]
fn command_grammar_rejects_missing_duplicate_unknown_malformed_mixed_and_extra_arguments() {
    let harness = Harness::new();
    harness.mkdir("/work/expected");
    let valid = write_expected_args();
    let mut cases = vec![
        Vec::new(),
        strings(&["unknown-command"]),
        strings(&["write_expected"]),
    ];

    let mut missing = valid.clone();
    missing.truncate(missing.len() - 2);
    cases.push(missing);

    let mut duplicate = valid.clone();
    duplicate.extend(strings(&["--output", "/work/expected/expected.json"]));
    cases.push(duplicate);

    let mut unknown = valid.clone();
    unknown.extend(strings(&["--unknown", "value"]));
    cases.push(unknown);

    let mut joined = valid.clone();
    joined.push("--output=/work/expected/expected.json".into());
    cases.push(joined);

    let mut positional = valid.clone();
    positional.push("extra".into());
    cases.push(positional);

    let mut mixed = valid;
    mixed.extend(strings(&["--archive", "/work/packaged/artifact.tar"]));
    cases.push(mixed);

    for arguments in cases {
        assert!(harness.run(&arguments).is_err(), "accepted {arguments:?}");
    }
    assert!(
        fs::read_dir(harness.path("/work/expected"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn host_process_rejects_every_nonliteral_work_root_before_work() {
    let executable = env!("CARGO_BIN_EXE_edgezero-provenance-validator");
    let non_work_root = std::env::temp_dir().canonicalize().unwrap();
    for mut arguments in [
        write_expected_args(),
        write_release_args(),
        package_args(),
        validate_args(),
    ] {
        replace_flag(
            &mut arguments,
            "--work-root",
            non_work_root.to_str().unwrap(),
        );
        let output = Command::new(executable).args(&arguments).output().unwrap();
        assert!(!output.status.success(), "accepted {arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("work root does not resolve to literal /work"),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn host_process_self_test_rejects_every_nonliteral_fixture_path() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docker/build-app-cli/fixtures/provenance")
        .canonicalize()
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_edgezero-provenance-validator"))
        .args(["self-test", "--fixtures"])
        .arg(fixtures)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("fixtures path is not the required literal")
    );
}

#[test]
fn writers_emit_exact_canonical_bytes_and_safe_modes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let harness = Harness::new();
    harness.mkdir("/work/expected");
    harness.run(&write_expected_args()).unwrap();
    let expected_path = harness.path("/work/expected/expected.json");
    assert_eq!(fs::read(&expected_path).unwrap(), expected("123456"));
    let metadata = fs::symlink_metadata(expected_path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o644);
    assert_eq!(metadata.nlink(), 1);

    harness.mkdir("/work/release");
    let mut release = write_release_args();
    replace_flag(&mut release, "--release-tag", "build-container-v7");
    harness.run(&release).unwrap();
    let release_path = harness.path("/work/release/release-request.json");
    assert_eq!(
        fs::read(release_path).unwrap(),
        format!(
            "{{\"gate-sha\":\"{GATE}\",\"provenance-protocol\":1,\"release-tag\":\"build-container-v7\"}}"
        )
        .as_bytes()
    );
}

#[test]
fn release_request_rejects_noncanonical_values_without_output() {
    for (flag, value) in [
        ("--gate-sha", "0000000000000000000000000000000000000000"),
        ("--gate-sha", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        ("--provenance-protocol", "01"),
        ("--release-tag", "build-container-v0"),
        ("--release-tag", "build-container-v01"),
        ("--release-tag", "v1"),
    ] {
        let harness = Harness::new();
        harness.mkdir("/work/release");
        let mut arguments = write_release_args();
        replace_flag(&mut arguments, flag, value);
        assert!(harness.run(&arguments).is_err(), "accepted {flag}={value}");
        assert!(
            fs::read_dir(harness.path("/work/release"))
                .unwrap()
                .next()
                .is_none()
        );
    }
}

#[test]
fn release_request_accepts_canonical_positive_decimal_beyond_u64() {
    let harness = Harness::new();
    harness.mkdir("/work/release");
    let mut arguments = write_release_args();
    replace_flag(
        &mut arguments,
        "--release-tag",
        "build-container-v18446744073709551616",
    );

    harness.run(&arguments).unwrap();
    assert!(
        String::from_utf8(fs::read(harness.path("/work/release/release-request.json")).unwrap())
            .unwrap()
            .contains("build-container-v18446744073709551616")
    );
}

#[test]
fn package_is_deterministic_and_validate_round_trips_without_execution() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (first_harness, first) = package_once("123456");
    let (_second_harness, second) = package_once("123456");
    assert_eq!(first, second);

    let archive_metadata =
        fs::symlink_metadata(first_harness.path("/work/packaged/artifact.tar")).unwrap();
    assert_eq!(archive_metadata.permissions().mode() & 0o7777, 0o644);
    assert_eq!(archive_metadata.nlink(), 1);

    let validator = Harness::new();
    prepare_validate(&validator, "123456", &first);
    validator.run(&validate_args()).unwrap();
    let output = validator.path("/work/validated/app-cli");
    assert_eq!(fs::read(&output).unwrap(), static_elf(0x800));
    let metadata = fs::symlink_metadata(output).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o755);
    assert_eq!(metadata.nlink(), 1);
}

#[test]
fn validate_rejects_identity_digest_size_and_abi_mismatches_before_publication() {
    let (_packager, archive) = package_once("123456");
    let cases = [
        archive.clone(),
        rewrite_metadata(&archive, |metadata| {
            metadata["binary-sha256"] = Value::String(format!("sha256:{}", "5".repeat(64)));
        }),
        rewrite_metadata(&archive, |metadata| {
            metadata["binary-size"] = Value::from(2049);
        }),
        rewrite_metadata(&archive, |metadata| {
            metadata["abi"]["interpreter"] = Value::String("/lib64/ld-linux-x86-64.so.2".into());
        }),
    ];

    for (index, candidate) in cases.into_iter().enumerate() {
        let harness = Harness::new();
        let repo = if index == 0 { "654321" } else { "123456" };
        prepare_validate(&harness, repo, &candidate);
        assert!(
            harness.run(&validate_args()).is_err(),
            "accepted case {index}"
        );
        assert!(
            fs::read_dir(harness.path("/work/validated"))
                .unwrap()
                .next()
                .is_none()
        );
    }
}

#[test]
fn malformed_schema_expected_archive_and_elf_inputs_leave_fresh_parents_empty() {
    let malformed_expected = Harness::new();
    malformed_expected.mkdir("/work/input");
    malformed_expected.mkdir("/work/packaged");
    malformed_expected.write("/work/input/expected.json", b"{}");
    malformed_expected.write("/work/input/app-cli", &static_elf(0x800));
    assert!(malformed_expected.run(&package_args()).is_err());
    assert_empty(&malformed_expected, "/work/packaged");

    let bad_elf = Harness::new();
    bad_elf.mkdir("/work/input");
    bad_elf.mkdir("/work/packaged");
    bad_elf.write("/work/input/expected.json", &expected("123456"));
    bad_elf.write("/work/input/app-cli", b"not ELF");
    assert!(bad_elf.run(&package_args()).is_err());
    assert_empty(&bad_elf, "/work/packaged");

    let malformed_archive = Harness::new();
    prepare_validate(&malformed_archive, "123456", b"not an archive");
    assert!(malformed_archive.run(&validate_args()).is_err());
    assert_empty(&malformed_archive, "/work/validated");

    let restrictive_schema = Harness::new();
    prepare_package(&restrictive_schema, "123456");
    restrictive_schema.write(
        "/usr/local/share/edgezero/provenance.schema.json",
        br#"{"type":"null"}"#,
    );
    assert!(restrictive_schema.run(&package_args()).is_err());
    assert_empty(&restrictive_schema, "/work/packaged");

    let invalid_schema = Harness::new();
    prepare_package(&invalid_schema, "123456");
    invalid_schema.write("/usr/local/share/edgezero/provenance.schema.json", b"{");
    assert!(invalid_schema.run(&package_args()).is_err());
    assert_empty(&invalid_schema, "/work/packaged");
}

#[test]
fn review_general_work_paths_accept_confined_alternates() {
    let expected_writer = Harness::new();
    expected_writer.mkdir("/work/alternate-expected");
    let mut arguments = write_expected_args();
    replace_flag(
        &mut arguments,
        "--output",
        "/work/alternate-expected/identity.json",
    );
    expected_writer.run(&arguments).unwrap();
    assert_eq!(
        fs::read(expected_writer.path("/work/alternate-expected/identity.json")).unwrap(),
        expected("123456")
    );

    let packager = Harness::new();
    packager.mkdir("/work/source");
    packager.mkdir("/work/identity");
    packager.mkdir("/work/alternate-package");
    packager.write("/work/source/tool", &static_elf(0x800));
    packager.write("/work/identity/build.json", &expected("123456"));
    let mut package = package_args();
    replace_flag(&mut package, "--binary", "/work/source/tool");
    replace_flag(&mut package, "--expected", "/work/identity/build.json");
    replace_flag(
        &mut package,
        "--archive",
        "/work/alternate-package/bundle.tar",
    );
    packager.run(&package).unwrap();
    let archive = fs::read(packager.path("/work/alternate-package/bundle.tar")).unwrap();

    let validator = Harness::new();
    validator.mkdir("/work/incoming");
    validator.mkdir("/work/identity");
    validator.mkdir("/work/alternate-install");
    validator.write("/work/incoming/bundle.tar", &archive);
    validator.write("/work/identity/build.json", &expected("123456"));
    let mut validate = validate_args();
    replace_flag(&mut validate, "--archive", "/work/incoming/bundle.tar");
    replace_flag(&mut validate, "--expected", "/work/identity/build.json");
    replace_flag(&mut validate, "--output", "/work/alternate-install/tool");
    validator.run(&validate).unwrap();
    assert_eq!(
        fs::read(validator.path("/work/alternate-install/tool")).unwrap(),
        static_elf(0x800)
    );
}

#[cfg(unix)]
#[test]
fn review_work_root_aliases_must_resolve_to_the_image_work_directory() {
    use std::os::unix::fs::symlink;

    let spelling = Harness::new();
    spelling.mkdir("/work/spelling-output");
    let mut spelling_args = write_expected_args();
    replace_flag(&mut spelling_args, "--work-root", "/work/.");
    replace_flag(
        &mut spelling_args,
        "--output",
        "/work/spelling-output/identity.json",
    );
    spelling.run(&spelling_args).unwrap();

    let parent_spelling = Harness::new();
    parent_spelling.mkdir("/work/parent-spelling-output");
    let mut parent_spelling_args = write_expected_args();
    replace_flag(&mut parent_spelling_args, "--work-root", "/work/../work");
    replace_flag(
        &mut parent_spelling_args,
        "--output",
        "/work/parent-spelling-output/identity.json",
    );
    parent_spelling.run(&parent_spelling_args).unwrap();

    let alias = Harness::new();
    alias.mkdir("/work/alias-output");
    symlink(alias.path("/work"), alias.path("/work-alias")).unwrap();
    let mut alias_args = write_expected_args();
    replace_flag(&mut alias_args, "--work-root", "/work-alias");
    replace_flag(
        &mut alias_args,
        "--output",
        "/work/alias-output/identity.json",
    );
    alias.run(&alias_args).unwrap();

    let wrong = Harness::new();
    wrong.mkdir("/elsewhere");
    wrong.mkdir("/work/rejected-output");
    symlink(wrong.path("/elsewhere"), wrong.path("/work-alias")).unwrap();
    let mut wrong_args = write_expected_args();
    replace_flag(&mut wrong_args, "--work-root", "/work-alias");
    replace_flag(
        &mut wrong_args,
        "--output",
        "/work/rejected-output/identity.json",
    );
    assert!(wrong.run(&wrong_args).is_err());
    assert_empty(&wrong, "/work/rejected-output");

    let outside = TempDir::new();
    fs::create_dir(outside.path().join("escaped-work")).unwrap();
    let escaped = Harness::new();
    escaped.mkdir("/work/rejected-output");
    symlink(
        outside.path().join("escaped-work"),
        escaped.path("/outside-alias"),
    )
    .unwrap();
    let mut escaped_args = write_expected_args();
    replace_flag(&mut escaped_args, "--work-root", "/outside-alias");
    replace_flag(
        &mut escaped_args,
        "--output",
        "/work/rejected-output/identity.json",
    );
    assert!(escaped.run(&escaped_args).is_err());
    assert_empty(&escaped, "/work/rejected-output");

    let escaped_then_parent = Harness::new();
    escaped_then_parent.mkdir("/work/rejected-output");
    symlink(
        outside.path().join("escaped-work"),
        escaped_then_parent.path("/outside-alias"),
    )
    .unwrap();
    let mut escaped_then_parent_args = write_expected_args();
    replace_flag(
        &mut escaped_then_parent_args,
        "--work-root",
        "/outside-alias/../work",
    );
    replace_flag(
        &mut escaped_then_parent_args,
        "--output",
        "/work/rejected-output/identity.json",
    );
    assert!(escaped_then_parent.run(&escaped_then_parent_args).is_err());
    assert_empty(&escaped_then_parent, "/work/rejected-output");
}

#[test]
fn review_work_paths_reject_lexical_escapes_and_baked_paths_remain_exact() {
    let cases = [
        (
            "write-expected",
            "--output",
            "/work/../outside/expected.json",
        ),
        (
            "write-release-request",
            "--output",
            "/work/release/other.json",
        ),
        ("package", "--binary", "/work/input/../../outside-app"),
        (
            "package",
            "--expected",
            "/work/input/../../outside-expected.json",
        ),
        (
            "package",
            "--archive",
            "/work/packaged/../../outside-artifact.tar",
        ),
        ("package", "--schema", "/work/input/schema.json"),
        (
            "validate",
            "--archive",
            "/work/input/../../outside-artifact.tar",
        ),
        (
            "validate",
            "--expected",
            "/work/input/../../outside-expected.json",
        ),
        ("validate", "--output", "/work/validated/../../outside-app"),
        ("validate", "--schema", "/work/input/schema.json"),
    ];

    for (command, flag, value) in cases {
        let harness = Harness::new();
        let mut arguments = match command {
            "write-expected" => {
                harness.mkdir("/work/expected");
                write_expected_args()
            }
            "write-release-request" => {
                harness.mkdir("/work/release");
                write_release_args()
            }
            "package" => {
                prepare_package(&harness, "123456");
                package_args()
            }
            "validate" => {
                harness.mkdir("/work/input");
                harness.mkdir("/work/validated");
                harness.write("/work/input/expected.json", &expected("123456"));
                harness.write("/work/input/artifact.tar", b"invalid");
                validate_args()
            }
            _ => unreachable!(),
        };
        replace_flag(&mut arguments, flag, value);
        assert!(
            harness.run(&arguments).is_err(),
            "accepted {command} {flag}={value}"
        );
    }
}

#[cfg(unix)]
#[test]
fn canonical_symlink_and_hardlink_escapes_fail_for_every_path_category() {
    use std::os::unix::fs::{MetadataExt, symlink};

    let binary = Harness::new();
    prepare_package(&binary, "123456");
    let outside_binary = binary.path("/outside-app-cli");
    fs::write(&outside_binary, static_elf(0x800)).unwrap();
    fs::remove_file(binary.path("/work/input/app-cli")).unwrap();
    symlink(&outside_binary, binary.path("/work/input/app-cli")).unwrap();
    assert!(binary.run(&package_args()).is_err());

    let expected_link = Harness::new();
    prepare_package(&expected_link, "123456");
    let outside_expected = expected_link.path("/outside-expected.json");
    fs::write(&outside_expected, expected("123456")).unwrap();
    fs::remove_file(expected_link.path("/work/input/expected.json")).unwrap();
    symlink(
        &outside_expected,
        expected_link.path("/work/input/expected.json"),
    )
    .unwrap();
    assert!(expected_link.run(&package_args()).is_err());

    let (_packager, archive_bytes) = package_once("123456");
    let archive_link = Harness::new();
    prepare_validate(&archive_link, "123456", &archive_bytes);
    let outside_archive = archive_link.path("/outside-artifact.tar");
    fs::write(&outside_archive, &archive_bytes).unwrap();
    fs::remove_file(archive_link.path("/work/input/artifact.tar")).unwrap();
    symlink(
        &outside_archive,
        archive_link.path("/work/input/artifact.tar"),
    )
    .unwrap();
    assert!(archive_link.run(&validate_args()).is_err());

    let schema_link = Harness::new();
    prepare_package(&schema_link, "123456");
    let schema = schema_link.path("/usr/local/share/edgezero/provenance.schema.json");
    let alternate = schema_link.path("/alternate-schema.json");
    fs::write(&alternate, SCHEMA).unwrap();
    fs::remove_file(&schema).unwrap();
    symlink(alternate, schema).unwrap();
    assert!(schema_link.run(&package_args()).is_err());

    let output_link = Harness::new();
    output_link.mkdir("/work/real-packaged");
    symlink(
        output_link.path("/work/real-packaged"),
        output_link.path("/work/packaged"),
    )
    .unwrap();
    output_link.mkdir("/work/input");
    output_link.write("/work/input/expected.json", &expected("123456"));
    output_link.write("/work/input/app-cli", &static_elf(0x800));
    assert!(output_link.run(&package_args()).is_err());

    let expected_output_link = Harness::new();
    expected_output_link.mkdir("/outside-expected-output");
    symlink(
        expected_output_link.path("/outside-expected-output"),
        expected_output_link.path("/work/expected-link"),
    )
    .unwrap();
    let mut expected_output_args = write_expected_args();
    replace_flag(
        &mut expected_output_args,
        "--output",
        "/work/expected-link/identity.json",
    );
    assert!(expected_output_link.run(&expected_output_args).is_err());

    let validate_expected_link = Harness::new();
    prepare_validate(&validate_expected_link, "123456", &archive_bytes);
    let outside_validate_expected = validate_expected_link.path("/outside-validate-expected.json");
    fs::write(&outside_validate_expected, expected("123456")).unwrap();
    symlink(
        outside_validate_expected,
        validate_expected_link.path("/work/input/expected-link.json"),
    )
    .unwrap();
    let mut validate_expected_args = validate_args();
    replace_flag(
        &mut validate_expected_args,
        "--expected",
        "/work/input/expected-link.json",
    );
    assert!(validate_expected_link.run(&validate_expected_args).is_err());

    let validate_output_link = Harness::new();
    prepare_validate(&validate_output_link, "123456", &archive_bytes);
    validate_output_link.mkdir("/outside-validated-output");
    symlink(
        validate_output_link.path("/outside-validated-output"),
        validate_output_link.path("/work/validated-link"),
    )
    .unwrap();
    let mut validate_output_args = validate_args();
    replace_flag(
        &mut validate_output_args,
        "--output",
        "/work/validated-link/tool",
    );
    assert!(validate_output_link.run(&validate_output_args).is_err());

    let hardlink = Harness::new();
    prepare_package(&hardlink, "123456");
    fs::hard_link(
        hardlink.path("/work/input/app-cli"),
        hardlink.path("/work/input/app-cli-alias"),
    )
    .unwrap();
    assert!(
        fs::metadata(hardlink.path("/work/input/app-cli"))
            .unwrap()
            .nlink()
            > 1
    );
    assert!(hardlink.run(&package_args()).is_err());
}

#[cfg(unix)]
#[test]
fn output_parents_must_be_empty_real_canonical_writable_directories() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let nonempty = Harness::new();
    prepare_package(&nonempty, "123456");
    nonempty.write("/work/packaged/sentinel", b"sentinel");
    assert!(nonempty.run(&package_args()).is_err());
    assert_eq!(
        fs::read(nonempty.path("/work/packaged/sentinel")).unwrap(),
        b"sentinel"
    );

    let symlinked = Harness::new();
    symlinked.mkdir("/work/input");
    symlinked.write("/work/input/expected.json", &expected("123456"));
    symlinked.write("/work/input/app-cli", &static_elf(0x800));
    symlinked.mkdir("/work/elsewhere");
    symlink(
        symlinked.path("/work/elsewhere"),
        symlinked.path("/work/packaged"),
    )
    .unwrap();
    assert!(symlinked.run(&package_args()).is_err());

    let regular = Harness::new();
    regular.mkdir("/work/input");
    regular.write("/work/input/expected.json", &expected("123456"));
    regular.write("/work/input/app-cli", &static_elf(0x800));
    regular.write("/work/packaged", b"not a directory");
    assert!(regular.run(&package_args()).is_err());

    let readonly = Harness::new();
    prepare_package(&readonly, "123456");
    fs::set_permissions(
        readonly.path("/work/packaged"),
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    assert!(readonly.run(&package_args()).is_err());
}

#[test]
fn final_name_collision_is_no_replace_and_cleans_only_the_owned_temporary_file() {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    let harness = Harness::new();
    prepare_package(&harness, "123456");
    harness.write("/work/input/app-cli", &static_elf(16 * 1024 * 1024));
    let parent = harness.path("/work/packaged");
    let temporary = parent.join(".artifact.tar.tmp");
    let final_path = parent.join("artifact.tar");
    let (cancel, cancelled) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if temporary.exists() {
                let mut collision = File::options()
                    .write(true)
                    .create_new(true)
                    .open(final_path)
                    .map_err(|error| error.to_string())?;
                std::io::Write::write_all(&mut collision, b"sentinel")
                    .map_err(|error| error.to_string())?;
                return Ok::<(), String>(());
            }
            match cancelled.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("package completed before temporary output appeared".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for temporary output".into());
            }
            std::thread::yield_now();
        }
    });

    let package_result = harness.run(&package_args());
    let _ = cancel.send(());
    let watcher_result = watcher.join().expect("collision watcher panicked");

    assert!(package_result.is_err());
    watcher_result.unwrap();
    assert_eq!(
        fs::read(harness.path("/work/packaged/artifact.tar")).unwrap(),
        b"sentinel"
    );
    assert!(!harness.path("/work/packaged/.artifact.tar.tmp").exists());
}

#[test]
fn host_deletion_recovery_removes_the_whole_parent_before_retry() {
    let harness = Harness::new();
    prepare_package(&harness, "123456");
    harness.write(
        "/work/packaged/.artifact.tar.tmp",
        b"simulated interruption",
    );
    assert!(harness.run(&package_args()).is_err());

    fs::remove_dir_all(harness.path("/work/packaged")).unwrap();
    harness.mkdir("/work/packaged");
    harness.run(&package_args()).unwrap();
    assert_eq!(
        fs::read_dir(harness.path("/work/packaged"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn self_test_accepts_only_the_complete_unchanged_compiled_fixture_manifest() {
    let valid = Harness::new();
    valid.install_fixtures();
    valid.run(&self_test_args()).unwrap();

    let missing = Harness::new();
    missing.install_fixtures();
    fs::remove_file(
        missing.path("/usr/local/share/edgezero/provenance-fixtures/valid/expected.json"),
    )
    .unwrap();
    assert!(missing.run(&self_test_args()).is_err());

    let extra = Harness::new();
    extra.install_fixtures();
    extra.write(
        "/usr/local/share/edgezero/provenance-fixtures/extra.json",
        b"{}",
    );
    assert!(extra.run(&self_test_args()).is_err());

    let changed = Harness::new();
    changed.install_fixtures();
    changed.write(
        "/usr/local/share/edgezero/provenance-fixtures/valid/expected.json",
        b"{}",
    );
    assert!(changed.run(&self_test_args()).is_err());
}

#[cfg(unix)]
#[test]
fn self_test_rejects_symlinked_fixture_entries_and_fixture_root() {
    use std::os::unix::fs::symlink;

    let entry = Harness::new();
    entry.install_fixtures();
    let expected_path =
        entry.path("/usr/local/share/edgezero/provenance-fixtures/valid/expected.json");
    fs::remove_file(&expected_path).unwrap();
    symlink(
        entry.path("/usr/local/share/edgezero/provenance.schema.json"),
        expected_path,
    )
    .unwrap();
    assert!(entry.run(&self_test_args()).is_err());

    let root = Harness::new();
    let alternate = root.path("/usr/local/share/edgezero/alternate-fixtures");
    copy_tree(Path::new(FIXTURES), &alternate);
    symlink(
        alternate,
        root.path("/usr/local/share/edgezero/provenance-fixtures"),
    )
    .unwrap();
    assert!(root.run(&self_test_args()).is_err());
}

#[test]
fn review_self_test_stops_at_file_count_and_aggregate_limits() {
    let over_count = Harness::new();
    over_count.install_fixtures();
    over_count.write(
        "/usr/local/share/edgezero/provenance-fixtures/valid/extra.json",
        b"{}",
    );
    let error = over_count.run(&self_test_args()).unwrap_err();
    assert!(
        error.contains("file count limit"),
        "unexpected error: {error}"
    );

    let aggregate = Harness::new();
    aggregate.install_fixtures();
    File::options()
        .write(true)
        .open(
            aggregate
                .path("/usr/local/share/edgezero/provenance-fixtures/valid/elf-static/app-cli"),
        )
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    let error = aggregate.run(&self_test_args()).unwrap_err();
    assert!(
        error.contains("aggregate byte limit"),
        "unexpected error: {error}"
    );
}

#[test]
fn review_self_test_rejects_unexpected_and_over_depth_directories_before_recursing() {
    let unexpected = Harness::new();
    unexpected.install_fixtures();
    unexpected.mkdir("/usr/local/share/edgezero/provenance-fixtures/unexpected/do-not-scan");
    let error = unexpected.run(&self_test_args()).unwrap_err();
    assert!(
        error.contains("unexpected fixture directory"),
        "unexpected error: {error}"
    );

    let over_depth = Harness::new();
    over_depth.install_fixtures();
    over_depth
        .mkdir("/usr/local/share/edgezero/provenance-fixtures/invalid/elf-malformed/too-deep");
    let error = over_depth.run(&self_test_args()).unwrap_err();
    assert!(
        error.contains("fixture depth limit"),
        "unexpected error: {error}"
    );
}

fn assert_empty(harness: &Harness, container_path: &str) {
    assert!(
        fs::read_dir(harness.path(container_path))
            .unwrap()
            .next()
            .is_none()
    );
}

fn static_elf(size: usize) -> Vec<u8> {
    const ELF_HEADER_SIZE: usize = 64;
    const PROGRAM_HEADER_SIZE: usize = 56;
    let mut bytes = vec![0; size];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    put_u16(&mut bytes, 16, 2);
    put_u16(&mut bytes, 18, 62);
    put_u32(&mut bytes, 20, 1);
    put_u64(&mut bytes, 32, ELF_HEADER_SIZE as u64);
    put_u16(&mut bytes, 52, ELF_HEADER_SIZE as u16);
    put_u16(&mut bytes, 54, PROGRAM_HEADER_SIZE as u16);
    put_u16(&mut bytes, 56, 1);
    let ph = ELF_HEADER_SIZE;
    put_u32(&mut bytes, ph, 1);
    put_u32(&mut bytes, ph + 4, 4);
    put_u64(&mut bytes, ph + 16, 0x400000);
    put_u64(&mut bytes, ph + 32, size as u64);
    put_u64(&mut bytes, ph + 40, size as u64);
    put_u64(&mut bytes, ph + 48, 8);
    bytes
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn input_reads_do_not_require_loading_the_binary_into_memory() {
    let harness = Harness::new();
    prepare_package(&harness, "123456");
    let binary = File::options()
        .write(true)
        .open(harness.path("/work/input/app-cli"))
        .unwrap();
    binary.set_len(32 * 1024 * 1024).unwrap();
    drop(binary);

    harness.run(&package_args()).unwrap();
    assert!(harness.path("/work/packaged/artifact.tar").is_file());
}
