use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

const RELEASE_METADATA_NAME: &str = "release.json";
const LIFECYCLE_PROTOCOL: u64 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationReleaseMetadata {
    adapter: String,
    app_cli: ReleaseMember,
    format: u64,
    lifecycle_protocol: u64,
    manifests: ReleaseManifests,
    package: ReleaseMember,
    source_revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseManifests {
    adapter: ReleaseMember,
    edgezero: ReleaseMember,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseMember {
    path: String,
    sha256: String,
}

#[derive(Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "release verification records every member; host-only tests audit their exact bytes"
    )
)]
pub(crate) struct VerifiedApplicationRelease {
    adapter_manifest: PathBuf,
    application_cli: PathBuf,
    application_manifest: PathBuf,
    package: PathBuf,
    package_sha256: String,
    root: PathBuf,
    source_revision: String,
}

impl VerifiedApplicationRelease {
    pub(crate) fn adapter_manifest(&self) -> &Path {
        &self.adapter_manifest
    }

    pub(crate) fn package(&self) -> &Path {
        &self.package
    }

    pub(crate) fn package_sha256(&self) -> &str {
        &self.package_sha256
    }
}

pub(crate) fn verify_application_release(
    root: &Path,
    loaded_application_manifest: &Path,
    referenced_adapter_manifest: &Path,
) -> Result<VerifiedApplicationRelease, String> {
    let canonical_root = root.canonicalize().map_err(|error| {
        format!(
            "could not resolve application release root {}: {error}",
            root.display()
        )
    })?;
    if !canonical_root.is_dir() {
        return Err(format!(
            "application release root {} is not a directory",
            canonical_root.display()
        ));
    }

    let metadata_path = canonical_root.join(RELEASE_METADATA_NAME);
    let metadata_bytes = fs::read(&metadata_path).map_err(|error| {
        format!("application release is missing {RELEASE_METADATA_NAME}: {error}")
    })?;
    let metadata: ApplicationReleaseMetadata = serde_json::from_slice(&metadata_bytes)
        .map_err(|error| format!("invalid application release release.json: {error}"))?;
    validate_metadata(&metadata)?;

    let app_cli_relative = validate_release_path(&metadata.app_cli.path)?;
    let package_relative = validate_release_path(&metadata.package.path)?;
    let application_manifest_relative = validate_release_path(&metadata.manifests.edgezero.path)?;
    let adapter_manifest_relative = validate_release_path(&metadata.manifests.adapter.path)?;
    let mut recorded_relative_paths = BTreeSet::new();
    for relative in [
        &app_cli_relative,
        &package_relative,
        &application_manifest_relative,
        &adapter_manifest_relative,
    ] {
        if !recorded_relative_paths.insert(relative.clone()) {
            return Err(format!(
                "application release contains duplicate recorded path {}",
                relative.display()
            ));
        }
    }
    let application_cli = verify_member(
        &canonical_root,
        &app_cli_relative,
        &metadata.app_cli.sha256,
        "application CLI",
    )?;
    let package = verify_member(
        &canonical_root,
        &package_relative,
        &metadata.package.sha256,
        "package",
    )?;
    let recorded_application_manifest = verify_member(
        &canonical_root,
        &application_manifest_relative,
        &metadata.manifests.edgezero.sha256,
        "application manifest",
    )?;
    let recorded_adapter_manifest = verify_member(
        &canonical_root,
        &adapter_manifest_relative,
        &metadata.manifests.adapter.sha256,
        "adapter manifest",
    )?;

    let canonical_loaded_manifest =
        canonical_regular_file(loaded_application_manifest, "loaded application manifest")?;
    if canonical_loaded_manifest != recorded_application_manifest {
        return Err(format!(
            "loaded application manifest {} is not the application manifest recorded by the immutable release",
            canonical_loaded_manifest.display()
        ));
    }
    let canonical_referenced_adapter_manifest =
        canonical_regular_file(referenced_adapter_manifest, "referenced adapter manifest")?;
    if canonical_referenced_adapter_manifest != recorded_adapter_manifest {
        return Err(format!(
            "referenced adapter manifest {} is not the adapter manifest recorded by the immutable release",
            canonical_referenced_adapter_manifest.display()
        ));
    }

    verify_exact_members(&canonical_root, &recorded_relative_paths)?;

    Ok(VerifiedApplicationRelease {
        adapter_manifest: recorded_adapter_manifest,
        application_cli,
        application_manifest: recorded_application_manifest,
        package,
        package_sha256: metadata.package.sha256,
        root: canonical_root,
        source_revision: metadata.source_revision,
    })
}

fn validate_metadata(metadata: &ApplicationReleaseMetadata) -> Result<(), String> {
    if metadata.format != 1 {
        return Err(format!(
            "unsupported application release format {}; expected format 1",
            metadata.format
        ));
    }
    if metadata.lifecycle_protocol != LIFECYCLE_PROTOCOL {
        return Err(format!(
            "unsupported application release lifecycle protocol {}; expected {}",
            metadata.lifecycle_protocol, LIFECYCLE_PROTOCOL
        ));
    }
    if metadata.adapter != "fastly" {
        return Err(format!(
            "application release adapter {:?} is unsupported; expected `fastly`",
            metadata.adapter
        ));
    }
    if !matches!(metadata.source_revision.len(), 40 | 64)
        || !is_lower_hex(&metadata.source_revision)
    {
        return Err(
            "application release source_revision must be exactly 40 or 64 lowercase hexadecimal characters"
                .to_owned(),
        );
    }
    for (label, member) in [
        ("app_cli", &metadata.app_cli),
        ("package", &metadata.package),
        ("manifests.edgezero", &metadata.manifests.edgezero),
        ("manifests.adapter", &metadata.manifests.adapter),
    ] {
        if member.sha256.len() != 64 || !is_lower_hex(&member.sha256) {
            return Err(format!(
                "application release {label}.sha256 must be exactly 64 lowercase hexadecimal characters"
            ));
        }
    }
    Ok(())
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_release_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() || raw.contains('\\') {
        return Err(format!(
            "application release path {raw:?} must be a non-empty normalized `/`-separated relative path"
        ));
    }
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "application release path {raw:?} must be normalized and relative to the release root"
        ));
    }
    let normalized = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => segment.to_str(),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if normalized != raw {
        return Err(format!(
            "application release path {raw:?} is not normalized"
        ));
    }
    Ok(path.to_path_buf())
}

fn verify_member(
    root: &Path,
    relative: &Path,
    expected_digest: &str,
    label: &str,
) -> Result<PathBuf, String> {
    let candidate = root.join(relative);
    let metadata = fs::symlink_metadata(&candidate).map_err(|error| {
        format!(
            "application release is missing recorded {label} {}: {error}",
            relative.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "application release recorded {label} {} is a symlink",
            relative.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "application release recorded {label} {} is not a regular file",
            relative.display()
        ));
    }
    let canonical = candidate.canonicalize().map_err(|error| {
        format!(
            "could not resolve application release {label} {}: {error}",
            relative.display()
        )
    })?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "application release recorded {label} {} resolves outside the release root",
            relative.display()
        ));
    }
    let bytes = fs::read(&canonical).map_err(|error| {
        format!(
            "could not read application release {label} {}: {error}",
            relative.display()
        )
    })?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected_digest {
        return Err(format!(
            "application release {label} {} digest does not match release.json",
            relative.display()
        ));
    }
    Ok(canonical)
}

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not resolve {label} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} {} is not a regular file", path.display()));
    }
    path.canonicalize()
        .map_err(|error| format!("could not resolve {label} {}: {error}", path.display()))
}

#[expect(
    clippy::filetype_is_file,
    reason = "the verifier must reject every non-directory, non-symlink, non-regular release member"
)]
fn verify_exact_members(root: &Path, recorded: &BTreeSet<PathBuf>) -> Result<(), String> {
    let mut expected = recorded.clone();
    expected.insert(PathBuf::from(RELEASE_METADATA_NAME));
    let mut expected_directories = BTreeSet::new();
    for member in &expected {
        let mut parent = member.parent();
        while let Some(directory) = parent.filter(|directory| !directory.as_os_str().is_empty()) {
            expected_directories.insert(directory.to_path_buf());
            parent = directory.parent();
        }
    }
    let mut actual = BTreeSet::new();
    for candidate in WalkDir::new(root).follow_links(false) {
        let entry = candidate
            .map_err(|error| format!("could not inspect extracted application release: {error}"))?;
        if entry.path() == root {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| format!("application release member escaped its root: {error}"))?;
        if entry.file_type().is_dir() {
            if !expected_directories.contains(relative) {
                return Err(format!(
                    "application release contains extra member {}",
                    relative.display()
                ));
            }
            continue;
        }
        if entry.file_type().is_symlink() {
            return Err(format!(
                "application release member {} is a symlink",
                relative.display()
            ));
        }
        if !entry.file_type().is_file() {
            return Err(format!(
                "application release member {} is not a regular file",
                relative.display()
            ));
        }
        actual.insert(relative.to_path_buf());
    }
    if actual != expected {
        let extra = actual.difference(&expected).next();
        let missing = expected.difference(&actual).next();
        if let Some(path) = extra {
            return Err(format!(
                "application release contains extra member {}",
                path.display()
            ));
        }
        if let Some(path) = missing {
            return Err(format!(
                "application release is missing recorded member {}",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::arbitrary_source_item_ordering,
        reason = "the release fixture keeps construction helpers in lifecycle order for readable adversarial tests"
    )]

    use super::*;
    use sha2::Sha256;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct ReleaseFixture {
        root: TempDir,
        application_manifest: PathBuf,
        adapter_manifest: PathBuf,
    }

    impl ReleaseFixture {
        fn new() -> Self {
            let root = TempDir::new().expect("release root");
            for directory in ["cli", "pkg", "adapter"] {
                fs::create_dir_all(root.path().join(directory)).expect("release directory");
            }
            let application_manifest = root.path().join("edgezero.toml");
            let adapter_manifest = root.path().join("adapter/fastly.toml");
            fs::write(root.path().join("cli/app-cli.tar.gz"), b"immutable cli")
                .expect("application cli");
            fs::write(root.path().join("pkg/app.tar.gz"), b"immutable package").expect("package");
            fs::write(
                &application_manifest,
                b"[app]\nname = \"demo\"\n[adapters.fastly.adapter]\nmanifest = \"adapter/fastly.toml\"\n",
            )
            .expect("application manifest");
            fs::write(
                &adapter_manifest,
                b"manifest_version = 3\nname = \"demo\"\n",
            )
            .expect("adapter manifest");
            let fixture = Self {
                root,
                application_manifest,
                adapter_manifest,
            };
            fixture.write_metadata_with(|_| {});
            fixture
        }

        fn digest(path: &Path) -> String {
            let bytes = fs::read(path).expect("fixture member");
            format!("{:x}", Sha256::digest(bytes))
        }

        fn metadata(&self) -> serde_json::Value {
            serde_json::json!({
                "format": 1,
                "lifecycle_protocol": 1,
                "source_revision": "a".repeat(40),
                "adapter": "fastly",
                "app_cli": {
                    "path": "cli/app-cli.tar.gz",
                    "sha256": Self::digest(&self.root.path().join("cli/app-cli.tar.gz")),
                },
                "package": {
                    "path": "pkg/app.tar.gz",
                    "sha256": Self::digest(&self.root.path().join("pkg/app.tar.gz")),
                },
                "manifests": {
                    "edgezero": {
                        "path": "edgezero.toml",
                        "sha256": Self::digest(&self.application_manifest),
                    },
                    "adapter": {
                        "path": "adapter/fastly.toml",
                        "sha256": Self::digest(&self.adapter_manifest),
                    }
                }
            })
        }

        fn write_metadata_with(&self, mutate: impl FnOnce(&mut serde_json::Value)) {
            let mut metadata = self.metadata();
            mutate(&mut metadata);
            fs::write(
                self.root.path().join("release.json"),
                serde_json::to_vec(&metadata).expect("metadata json"),
            )
            .expect("release metadata");
        }

        fn verify(&self) -> Result<VerifiedApplicationRelease, String> {
            verify_application_release(
                self.root.path(),
                &self.application_manifest,
                &self.adapter_manifest,
            )
        }
    }

    #[test]
    fn application_release_verifies_exact_members_and_returns_confined_paths() {
        let fixture = ReleaseFixture::new();
        let verified = fixture.verify().expect("valid release");

        assert_eq!(verified.root, fixture.root.path().canonicalize().unwrap());
        assert_eq!(
            verified.application_cli,
            fixture
                .root
                .path()
                .join("cli/app-cli.tar.gz")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            verified.application_manifest,
            fixture.application_manifest.canonicalize().unwrap()
        );
        assert_eq!(
            verified.adapter_manifest,
            fixture.adapter_manifest.canonicalize().unwrap()
        );
        assert_eq!(
            verified.package,
            fixture
                .root
                .path()
                .join("pkg/app.tar.gz")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(verified.package_sha256.len(), 64);
        assert_eq!(verified.source_revision, "a".repeat(40));
    }

    #[test]
    fn application_release_allows_required_parents_and_rejects_extra_empty_directories() {
        let fixture = ReleaseFixture::new();
        fixture
            .verify()
            .expect("directories required by recorded members are allowed");

        fs::create_dir_all(fixture.root.path().join("unexpected-empty"))
            .expect("extra empty directory");
        let error = fixture
            .verify()
            .expect_err("an extra empty directory is an extra release member");
        assert!(error.contains("extra member unexpected-empty"), "{error}");
    }

    #[test]
    fn application_release_rejects_duplicate_unknown_and_unsupported_metadata() {
        let fixture = ReleaseFixture::new();
        let valid = fs::read_to_string(fixture.root.path().join("release.json")).unwrap();
        let duplicate = valid.replacen("\"format\":1", "\"format\":1,\"format\":1", 1);
        fs::write(fixture.root.path().join("release.json"), duplicate).unwrap();
        assert!(fixture.verify().unwrap_err().contains("release.json"));

        fixture.write_metadata_with(|metadata| {
            metadata["unexpected"] = serde_json::json!(true);
        });
        assert!(fixture.verify().unwrap_err().contains("unknown"));

        fixture.write_metadata_with(|metadata| metadata["format"] = serde_json::json!(2_u64));
        assert!(fixture.verify().unwrap_err().contains("format"));

        fixture.write_metadata_with(|metadata| {
            metadata["app_cli"]["unexpected"] = serde_json::json!(true);
        });
        assert!(fixture.verify().unwrap_err().contains("unknown"));

        fixture.write_metadata_with(|metadata| {
            metadata
                .as_object_mut()
                .expect("release metadata object")
                .remove("package");
        });
        assert!(fixture.verify().unwrap_err().contains("missing field"));
    }

    #[test]
    fn application_release_requires_supported_lifecycle_protocol() {
        let missing = ReleaseFixture::new();
        missing.write_metadata_with(|metadata| {
            metadata
                .as_object_mut()
                .expect("release metadata object")
                .remove("lifecycle_protocol");
        });
        assert!(missing.verify().unwrap_err().contains("lifecycle_protocol"));

        let wrong_type = ReleaseFixture::new();
        wrong_type.write_metadata_with(|metadata| {
            metadata["lifecycle_protocol"] = serde_json::json!("1");
        });
        assert!(
            wrong_type
                .verify()
                .unwrap_err()
                .contains("invalid application release release.json")
        );

        let unsupported = ReleaseFixture::new();
        unsupported.write_metadata_with(|metadata| {
            metadata["lifecycle_protocol"] = serde_json::json!(2_u64);
        });
        assert!(
            unsupported
                .verify()
                .unwrap_err()
                .contains("lifecycle protocol")
        );
    }

    #[test]
    fn application_release_rejects_invalid_revision_adapter_and_digest_syntax() {
        for revision in ["A".repeat(40), "a".repeat(39), "z".repeat(40)] {
            let revision_fixture = ReleaseFixture::new();
            revision_fixture.write_metadata_with(|metadata| {
                metadata["source_revision"] = serde_json::json!(revision);
            });
            assert!(
                revision_fixture
                    .verify()
                    .unwrap_err()
                    .contains("source_revision")
            );
        }

        let adapter_fixture = ReleaseFixture::new();
        adapter_fixture.write_metadata_with(|metadata| {
            metadata["adapter"] = serde_json::json!("cloudflare");
        });
        assert!(adapter_fixture.verify().unwrap_err().contains("adapter"));

        for digest in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            let digest_fixture = ReleaseFixture::new();
            digest_fixture.write_metadata_with(|metadata| {
                metadata["package"]["sha256"] = serde_json::json!(digest);
            });
            assert!(digest_fixture.verify().unwrap_err().contains("sha256"));
        }
    }

    #[test]
    fn application_release_rejects_unconfined_or_non_normalized_paths() {
        for path in [
            "/tmp/package.tar.gz",
            "../package.tar.gz",
            "pkg/../pkg/app.tar.gz",
            "pkg//app.tar.gz",
            "pkg\\app.tar.gz",
            "./pkg/app.tar.gz",
        ] {
            let fixture = ReleaseFixture::new();
            fixture.write_metadata_with(|metadata| {
                metadata["package"]["path"] = serde_json::json!(path);
            });
            assert!(fixture.verify().is_err(), "path {path:?} must be rejected");
        }

        let fixture = ReleaseFixture::new();
        fixture.write_metadata_with(|metadata| {
            metadata["package"]["path"] = serde_json::json!("cli/app-cli.tar.gz");
            metadata["package"]["sha256"] = metadata["app_cli"]["sha256"].clone();
        });
        assert!(fixture.verify().unwrap_err().contains("duplicate"));
    }

    #[cfg(unix)]
    #[test]
    fn application_release_rejects_symlinks_and_canonical_root_escapes() {
        use std::os::unix::fs::symlink;

        let file_symlink_fixture = ReleaseFixture::new();
        fs::remove_file(file_symlink_fixture.root.path().join("pkg/app.tar.gz")).unwrap();
        symlink(
            file_symlink_fixture.root.path().join("cli/app-cli.tar.gz"),
            file_symlink_fixture.root.path().join("pkg/app.tar.gz"),
        )
        .unwrap();
        assert!(
            file_symlink_fixture
                .verify()
                .unwrap_err()
                .contains("symlink")
        );

        let directory_symlink_fixture = ReleaseFixture::new();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("app.tar.gz"), b"immutable package").unwrap();
        fs::remove_dir_all(directory_symlink_fixture.root.path().join("pkg")).unwrap();
        symlink(
            outside.path(),
            directory_symlink_fixture.root.path().join("pkg"),
        )
        .unwrap();
        let error = directory_symlink_fixture.verify().unwrap_err();
        assert!(
            error.contains("outside") || error.contains("symlink"),
            "{error}"
        );
    }

    #[test]
    fn application_release_rejects_non_files_missing_and_extra_members() {
        let directory_fixture = ReleaseFixture::new();
        fs::remove_file(directory_fixture.root.path().join("pkg/app.tar.gz")).unwrap();
        fs::create_dir_all(directory_fixture.root.path().join("pkg/app.tar.gz")).unwrap();
        assert!(
            directory_fixture
                .verify()
                .unwrap_err()
                .contains("regular file")
        );

        let missing_fixture = ReleaseFixture::new();
        fs::remove_file(missing_fixture.root.path().join("pkg/app.tar.gz")).unwrap();
        assert!(missing_fixture.verify().unwrap_err().contains("missing"));

        let extra_fixture = ReleaseFixture::new();
        fs::write(extra_fixture.root.path().join("extra.txt"), b"extra").unwrap();
        assert!(extra_fixture.verify().unwrap_err().contains("extra"));
    }

    #[test]
    fn application_release_rejects_each_digest_mismatch() {
        for member in [
            "cli/app-cli.tar.gz",
            "pkg/app.tar.gz",
            "edgezero.toml",
            "adapter/fastly.toml",
        ] {
            let fixture = ReleaseFixture::new();
            fs::write(fixture.root.path().join(member), b"tampered").unwrap();
            let error = fixture.verify().unwrap_err();
            assert!(error.contains("digest"), "{member}: {error}");
            assert!(!error.contains("tampered"), "file contents leaked: {error}");
        }
    }

    #[test]
    fn application_release_requires_exact_loaded_manifests() {
        let fixture = ReleaseFixture::new();
        let other_application = fixture.root.path().join("other-edgezero.toml");
        fs::write(&other_application, b"[app]\nname = \"other\"\n").unwrap();
        let application_error = verify_application_release(
            fixture.root.path(),
            &other_application,
            &fixture.adapter_manifest,
        )
        .unwrap_err();
        assert!(
            application_error.contains("application manifest"),
            "{application_error}"
        );

        let other_adapter = fixture.root.path().join("adapter/other-fastly.toml");
        fs::write(&other_adapter, b"manifest_version = 3\n").unwrap();
        let adapter_error = verify_application_release(
            fixture.root.path(),
            &fixture.application_manifest,
            &other_adapter,
        )
        .unwrap_err();
        assert!(
            adapter_error.contains("adapter manifest"),
            "{adapter_error}"
        );
    }
}
