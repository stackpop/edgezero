use crate::{
    Result, archive, elf,
    json_contract::{EXPECTED_LIMIT, Expected, METADATA_LIMIT, Metadata},
    orchestration::{ExecutionLayout, SafeFile, abi, validate_schema_instance},
    require,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, Metadata as FsMetadata},
    io::Read,
    path::Path,
};

const FIXTURE_LIMIT: u64 = 1024 * 1024;
const AGGREGATE_FIXTURE_LIMIT: u64 = 1024 * 1024;
const MAX_DIRECTORY_DEPTH: usize = 2;
const MAX_FILE_DEPTH: usize = 3;
const DIRECTORIES: &[&str] = &[
    "invalid",
    "invalid/elf-malformed",
    "valid",
    "valid/elf-static",
];

#[derive(Clone, Copy)]
enum Kind {
    Archive(bool),
    Elf(bool),
    Expected(bool),
    Metadata(bool),
}

struct Fixture {
    path: &'static str,
    sha256: &'static str,
    kind: Kind,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        path: "invalid/archive-base256.tar",
        sha256: "75493ee2dbbbc8039bd24eb9d251dbbf551f7e8dbbb8804c5a96cdff40cb8ad2",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-checksum.tar",
        sha256: "68de4b0bf30ca7ffc20d528beeccf494891601b93ab515aad4083225d7546b85",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-duplicate.tar",
        sha256: "c10edd5f77f7d1111646e4aa150f2d91b747a9cf5c0647a480156e770fd93e98",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-embedded-nul.tar",
        sha256: "ceb3eca4167017a6e0e670b6f4fa0cf6b2efaca6aee44e26859942690629b049",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-end-block.tar",
        sha256: "daed89daf33ed29d1cb37eb7ab954dfadcad0137c951a4577ced9c2261254592",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-extra-end-block.tar",
        sha256: "46f48046763a3eb258d066056419f63d9be9278929608d1422fe296bd2d1c634",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-extra.tar",
        sha256: "3bce4f6a7369bad91cb786e3d5d84fe2bba5064d4a317f0830ac699b372101f7",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-gnu.tar",
        sha256: "443d5adbb05f81532e307a62ed24dbe148ff520d7ab5dba9e3da139c91fd95bb",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-header.tar",
        sha256: "e758f98e82e509d5d879b7b537cef6b0ef4f5c42774da2974cf4028519af963f",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-link.tar",
        sha256: "4ba5272123152f0d4623f41afee17f329992ca742bb5913439053e12fb208c94",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-max-size.tar",
        sha256: "1a364c6ce7037a527e6519d25aa1fc4088843029689bfb065b3f57e267c6e93e",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-octal-digit.tar",
        sha256: "494ca5688ca6a228ff8019b7eaf7f9df5b48a09787de6909a25179116368ecca",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-octal-padding.tar",
        sha256: "6b3268c71672d4e7e8ebb7a9eee70fe766f08c980371e9846c7b59fda0c4d3fb",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-order.tar",
        sha256: "60ae90ff78d80daef9b1f2bd80a48dde9ee807121624d2e237eea9bc00e22944",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-padding.tar",
        sha256: "e778e0fed834ecfa56621aa4e3c9a4ab45040807053e2157bf48b31750f013c6",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-pax.tar",
        sha256: "205e55d4f39e5cd505515b35b8338ee19625506b1571a4e1fcdc568127596b55",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-size-limit.tar",
        sha256: "2d7e850dc5bf999314cc538825b40a303249b3469cb1ffb043e06c383733dbf4",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-size.tar",
        sha256: "f06217b9f69801d5e78e3f88b4f42fa69a69ab8d688899cc384bb4eecbab974e",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-sparse.tar",
        sha256: "0a4e7d7079fb2b74ad994a9a69013b049222acdec6c408523c74264f8f157bd5",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-special.tar",
        sha256: "43f1840667cd1e06166ff9a28af39baa5c94a4db60ed6fa821111cd794cf9def",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-trailing.tar",
        sha256: "c2f1705cbad092ee21caf2dab7adec106a262596308182eb34e161d8bb2a1090",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/archive-traversal.tar",
        sha256: "11b921b9b519ea14eaa6ca164766096c652924bc6b7fee672375c581dfd62cc4",
        kind: Kind::Archive(false),
    },
    Fixture {
        path: "invalid/elf-malformed/app-cli",
        sha256: "b768ce3772e64cd00641976694c5dac01326e322fb5cc7bebf81ac4960298ab3",
        kind: Kind::Elf(false),
    },
    Fixture {
        path: "invalid/expected-duplicate.json",
        sha256: "cc629e4bbc2f37d35127cc5fa53414e00968f9679f3f16941fab389e384ca2cb",
        kind: Kind::Expected(false),
    },
    Fixture {
        path: "invalid/meta-missing-interpreter.json",
        sha256: "c6299c11b02f864f65bc5feb3ec58a6b37ad8336ae5c8ee77c4a7bf0c3ac4606",
        kind: Kind::Metadata(false),
    },
    Fixture {
        path: "valid/archive.tar",
        sha256: "bb041c59d4c24ecc41f3f329040aef224a8afd5477ee515b59c6ae94d5c258f1",
        kind: Kind::Archive(true),
    },
    Fixture {
        path: "valid/dynamic-meta.json",
        sha256: "f30b752dc40f104311c42a8275d5c9e46a65336ce788a776b7a8cc06edaf4aee",
        kind: Kind::Metadata(true),
    },
    Fixture {
        path: "valid/elf-static/app-cli",
        sha256: "6b25433eed518a44b19e8c821749f4dd07156d0962727ed27c15f8816c3c1c96",
        kind: Kind::Elf(true),
    },
    Fixture {
        path: "valid/expected.json",
        sha256: "67faafc666f0f3ac2ff3528aa27befa4f747fa39efbcbe2c9543cec793f2179f",
        kind: Kind::Expected(true),
    },
    Fixture {
        path: "valid/static-meta.json",
        sha256: "8ad2590f982dacf2272e8afb1f88b9e910349f43aab38675bac9cd43eb2e47d2",
        kind: Kind::Metadata(true),
    },
];

pub(crate) fn run(layout: &ExecutionLayout, supplied: &str) -> Result<()> {
    let fixture_root = layout.fixture_root(supplied)?;
    let image_root = layout.image_root()?;
    let image_owner = fs::symlink_metadata(&image_root).map_err(|error| error.to_string())?;
    let schema = layout.baked_schema()?;
    let schema_bytes = schema.read_bounded(METADATA_LIMIT)?;
    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    let mut aggregate_bytes = 0u64;
    collect(
        &fixture_root,
        &fixture_root,
        &image_owner,
        &mut files,
        &mut directories,
        &mut aggregate_bytes,
    )?;

    let expected_directories = DIRECTORIES.iter().copied().collect::<BTreeSet<_>>();
    require(
        directories
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == expected_directories,
        "fixture directory manifest mismatch",
    )?;
    require(
        files.len() == FIXTURES.len(),
        "fixture file manifest mismatch",
    )?;
    require(
        files
            .keys()
            .map(String::as_str)
            .eq(FIXTURES.iter().map(|fixture| fixture.path)),
        "fixture path manifest mismatch",
    )?;

    for fixture in FIXTURES {
        let file = files
            .get(fixture.path)
            .ok_or_else(|| format!("missing fixture: {}", fixture.path))?;
        require(
            sha256(file)? == fixture.sha256,
            &format!("fixture digest mismatch: {}", fixture.path),
        )?;
        let valid = evaluate(fixture.kind, file, &schema_bytes, &image_root).is_ok();
        let expected_valid = match fixture.kind {
            Kind::Archive(valid)
            | Kind::Elf(valid)
            | Kind::Expected(valid)
            | Kind::Metadata(valid) => valid,
        };
        require(
            valid == expected_valid,
            &format!("fixture semantic outcome mismatch: {}", fixture.path),
        )?;
        file.verify()?;
    }
    schema.verify()
}

fn collect(
    root: &Path,
    directory: &Path,
    image_owner: &FsMetadata,
    files: &mut BTreeMap<String, SafeFile>,
    directories: &mut BTreeSet<String>,
    aggregate_bytes: &mut u64,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "fixture path escapes fixture root")?;
        let relative = relative
            .to_str()
            .ok_or("fixture path is not UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        require_same_owner(&metadata, image_owner)?;
        if metadata.file_type().is_dir() {
            require(
                relative.split('/').count() <= MAX_DIRECTORY_DEPTH,
                "fixture depth limit exceeded",
            )?;
            require(
                DIRECTORIES.contains(&relative.as_str()),
                "unexpected fixture directory",
            )?;
            require(
                directories.len() < DIRECTORIES.len(),
                "fixture directory count limit exceeded",
            )?;
            require(
                path.canonicalize().map_err(|error| error.to_string())? == path,
                "fixture directory is not canonical",
            )?;
            require(directories.insert(relative), "duplicate fixture directory")?;
            collect(
                root,
                &path,
                image_owner,
                files,
                directories,
                aggregate_bytes,
            )?;
        } else if metadata.file_type().is_file() {
            require(
                relative.split('/').count() <= MAX_FILE_DEPTH,
                "fixture depth limit exceeded",
            )?;
            require(
                files.len() < FIXTURES.len(),
                "fixture file count limit exceeded",
            )?;
            require(
                metadata.len() <= FIXTURE_LIMIT,
                "fixture exceeds byte limit",
            )?;
            *aggregate_bytes = aggregate_bytes
                .checked_add(metadata.len())
                .ok_or("fixture aggregate size overflow")?;
            require(
                *aggregate_bytes <= AGGREGATE_FIXTURE_LIMIT,
                "fixture aggregate byte limit exceeded",
            )?;
            let file = SafeFile::open_path(path, Some(root), Some(image_owner))?;
            require(
                files.insert(relative, file).is_none(),
                "duplicate fixture path",
            )?;
        } else {
            return Err("fixture entry is not a regular file or directory".into());
        }
    }
    Ok(())
}

fn evaluate(kind: Kind, file: &SafeFile, schema: &[u8], image_root: &Path) -> Result<()> {
    match kind {
        Kind::Archive(valid) => {
            let mut input = file.open()?;
            let parsed = archive::parse(&mut input)?;
            let metadata = Metadata::parse(&parsed.metadata)?;
            validate_schema_instance(schema, &parsed.metadata)?;
            metadata.canonical_bytes()?;
            if valid {
                evaluate_integrated_archive(
                    file, &mut input, &parsed, &metadata, schema, image_root,
                )?;
            }
            Ok(())
        }
        Kind::Elf(_) => elf::inspect(file.path(), image_root).map(|_| ()),
        Kind::Expected(_) => {
            let bytes = file.read_bounded(EXPECTED_LIMIT)?;
            let expected = Expected::parse(&bytes)?;
            validate_schema_instance(schema, &bytes)?;
            expected.canonical_bytes().map(|_| ())
        }
        Kind::Metadata(_) => {
            let bytes = file.read_bounded(METADATA_LIMIT)?;
            let metadata = Metadata::parse(&bytes)?;
            validate_schema_instance(schema, &bytes)?;
            metadata.canonical_bytes().map(|_| ())
        }
    }
}

fn evaluate_integrated_archive(
    archive_file: &SafeFile,
    input: &mut (impl Read + std::io::Seek),
    parsed: &archive::ParsedArchive,
    metadata: &Metadata,
    schema: &[u8],
    image_root: &Path,
) -> Result<()> {
    let valid = archive_file
        .path()
        .parent()
        .ok_or("valid archive fixture has no parent")?;
    let fixture_root = valid
        .parent()
        .ok_or("valid archive fixture escapes fixture root")?;
    let expected = SafeFile::open_path(valid.join("expected.json"), Some(fixture_root), None)?;
    let expected_bytes = expected.read_bounded(EXPECTED_LIMIT)?;
    let identity = Expected::parse(&expected_bytes)?;
    validate_schema_instance(schema, &expected_bytes)?;
    metadata.matches(&identity)?;

    let binary = SafeFile::open_path(valid.join("elf-static/app-cli"), Some(fixture_root), None)?;
    let inspection = elf::inspect(binary.path(), image_root)?;
    let observed_abi = abi(&inspection)?;
    metadata.matches_observation(
        &observed_abi,
        &inspection.binary_sha256,
        inspection.binary_size,
    )?;
    require(
        parsed.binary_size == inspection.binary_size,
        "valid archive binary size differs from ELF fixture",
    )?;
    archive::verify_binary_sha256(input, parsed, &inspection.binary_sha256)?;
    binary.verify()?;
    expected.verify()
}

fn sha256(file: &SafeFile) -> Result<String> {
    let mut input = file.open()?.take(file.size() + 1);
    let mut digest = Sha256::new();
    let mut buffer = [0; 8 * 1024];
    let mut size = 0u64;
    loop {
        let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or("fixture size overflow")?;
        digest.update(&buffer[..read]);
    }
    require(size == file.size(), "fixture size changed while hashing")?;
    file.verify()?;
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
fn require_same_owner(actual: &FsMetadata, expected: &FsMetadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    require(
        actual.uid() == expected.uid() && actual.gid() == expected.gid(),
        "fixture entry is not image-owned",
    )
}

#[cfg(not(unix))]
fn require_same_owner(_actual: &FsMetadata, _expected: &FsMetadata) -> Result<()> {
    Err("fixture ownership validation requires Unix".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Cursor,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    const SCHEMA: &[u8] = include_bytes!("../../../docker/build-app-cli/provenance.schema.json");
    const EXPECTED: &[u8] =
        include_bytes!("../../../docker/build-app-cli/fixtures/provenance/valid/expected.json");
    const ELF: &[u8] = include_bytes!(
        "../../../docker/build-app-cli/fixtures/provenance/valid/elf-static/app-cli"
    );
    const ARCHIVE: &[u8] =
        include_bytes!("../../../docker/build-app-cli/fixtures/provenance/valid/archive.tar");

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "edgezero-provenance-self-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    #[test]
    fn integrated_valid_archive_rejects_observation_mismatch_after_layer_validation() {
        let image_root = TempDir::new();
        let fixture_root = image_root
            .0
            .join("usr/local/share/edgezero/provenance-fixtures");
        let valid = fixture_root.join("valid");
        fs::create_dir_all(valid.join("elf-static")).unwrap();
        fs::write(valid.join("expected.json"), EXPECTED).unwrap();
        fs::write(valid.join("elf-static/app-cli"), ELF).unwrap();

        let mut bytes = ARCHIVE.to_vec();
        let parsed = archive::parse(&mut Cursor::new(&bytes)).unwrap();
        bytes[usize::try_from(parsed.binary_offset).unwrap()] ^= 1;
        let parsed = archive::parse(&mut Cursor::new(&bytes)).unwrap();
        Metadata::parse(&parsed.metadata).unwrap();
        validate_schema_instance(SCHEMA, &parsed.metadata).unwrap();

        let archive_path = valid.join("archive.tar");
        fs::write(&archive_path, bytes).unwrap();
        let archive = SafeFile::open_path(archive_path, Some(&fixture_root), None).unwrap();

        let error = evaluate(Kind::Archive(true), &archive, SCHEMA, &image_root.0).unwrap_err();
        assert!(
            error.contains("binary metadata") || error.contains("binary digest"),
            "unexpected error: {error}"
        );
    }
}
