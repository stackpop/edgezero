use crate::{
    Result,
    archive::{self, checked_seek, copy_exact},
    command::{self, Command},
    elf,
    extract::{atomic_publish, validate_output_parent},
    json_contract::{
        Abi, BINARY_LIMIT, EXPECTED_LIMIT, Expected, Interpreter, METADATA_LIMIT,
        Metadata as ProvenanceMetadata,
    },
    require, self_test,
};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs::{self, File, Metadata},
    io::{Cursor, Read, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

const WORK_ROOT: &str = "/work";
const SCHEMA_PATH: &str = "/usr/local/share/edgezero/provenance.schema.json";
const FIXTURES_PATH: &str = "/usr/local/share/edgezero/provenance-fixtures";
const SCHEMA_LIMIT: usize = 64 * 1024;
const ARCHIVE_LIMIT: u64 = BINARY_LIMIT + METADATA_LIMIT as u64 + 4 * archive::BLOCK_SIZE as u64;

#[derive(Clone, Debug)]
pub struct ExecutionLayout {
    image_root: PathBuf,
}

impl ExecutionLayout {
    pub fn container() -> Self {
        Self {
            image_root: PathBuf::from("/"),
        }
    }

    pub fn rooted_at(image_root: impl AsRef<Path>) -> Self {
        Self {
            image_root: image_root.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn image_root(&self) -> Result<PathBuf> {
        let canonical = self
            .image_root
            .canonicalize()
            .map_err(|error| format!("invalid image root: {error}"))?;
        require(canonical == self.image_root, "image root is not canonical")?;
        require(
            fs::symlink_metadata(&self.image_root)
                .map_err(|error| format!("invalid image root: {error}"))?
                .file_type()
                .is_dir(),
            "image root is not a directory",
        )?;
        Ok(canonical)
    }

    pub(crate) fn mapped(&self, literal: &str) -> Result<PathBuf> {
        let path = Path::new(literal);
        require(path.is_absolute(), "container path is not absolute")?;
        require(
            path.components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
            "container path is not lexically canonical",
        )?;
        let relative = path
            .strip_prefix("/")
            .map_err(|_| "container path is not absolute")?;
        Ok(self.image_root()?.join(relative))
    }

    fn mapped_root_spelling(&self, spelling: &str) -> Result<PathBuf> {
        let path = Path::new(spelling);
        require(path.is_absolute(), "work root is not absolute")?;
        let image_root = self.image_root()?;
        let mut mapped = image_root.clone();
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    if mapped != image_root {
                        mapped.pop();
                    }
                }
                Component::Normal(component) => {
                    mapped.push(component);
                    mapped = mapped
                        .canonicalize()
                        .map_err(|error| format!("invalid work root: {error}"))?;
                    require(
                        mapped.starts_with(&image_root),
                        "work root spelling escapes image root",
                    )?;
                }
                Component::Prefix(_) => return Err("work root is not an absolute Unix path".into()),
            }
        }
        require(
            mapped.starts_with(&image_root),
            "work root spelling escapes image root",
        )?;
        Ok(mapped)
    }

    fn work_root(&self, supplied: &str) -> Result<PathBuf> {
        let required = self.mapped(WORK_ROOT)?;
        let supplied = self.mapped_root_spelling(supplied)?;
        let canonical = supplied
            .canonicalize()
            .map_err(|error| format!("invalid work root: {error}"))?;
        require(
            canonical == required,
            "work root does not resolve to literal /work",
        )?;
        require(
            fs::metadata(&supplied)
                .map_err(|error| format!("invalid work root: {error}"))?
                .is_dir(),
            "work root is not a directory",
        )?;
        Ok(canonical)
    }

    fn work_input(&self, supplied: &str, work: &Path) -> Result<SafeFile> {
        require_work_path(supplied, "input path")?;
        let path = self.mapped(supplied)?;
        require(
            path.starts_with(work),
            "input path escapes work root lexically",
        )?;
        SafeFile::open_path(path, Some(work), None)
    }

    fn schema(&self, supplied: &str) -> Result<SafeFile> {
        require_exact(supplied, SCHEMA_PATH, "schema path")?;
        let root = self.image_root()?;
        let ownership =
            fs::symlink_metadata(&root).map_err(|error| format!("invalid image root: {error}"))?;
        SafeFile::open_path(self.mapped(supplied)?, None, Some(&ownership))
    }

    fn output_target(&self, supplied: &str, work: &Path) -> Result<OutputTarget> {
        require_work_path(supplied, "output path")?;
        let basename = Path::new(supplied)
            .file_name()
            .and_then(|basename| basename.to_str())
            .ok_or("output basename is not normal UTF-8")?;
        require(
            !basename.is_empty() && basename != "." && basename != "..",
            "output basename is not normal UTF-8",
        )?;
        let output = self.mapped(supplied)?;
        require(
            output.starts_with(work),
            "output path escapes work root lexically",
        )?;
        let parent = output.parent().ok_or("output has no parent")?;
        let canonical = parent
            .canonicalize()
            .map_err(|error| format!("invalid output parent: {error}"))?;
        require(canonical == parent, "output parent is not canonical")?;
        require(
            canonical.starts_with(work) && canonical != work,
            "output parent escapes work root canonically",
        )?;
        validate_output_parent(parent)?;
        Ok(OutputTarget {
            parent: parent.to_path_buf(),
            basename: basename.to_owned(),
        })
    }

    pub(crate) fn fixture_root(&self, supplied: &str) -> Result<PathBuf> {
        require_exact(supplied, FIXTURES_PATH, "fixtures path")?;
        let root = self.image_root()?;
        let mapped = self.mapped(supplied)?;
        let canonical = mapped
            .canonicalize()
            .map_err(|error| format!("invalid fixtures directory: {error}"))?;
        require(canonical == mapped, "fixtures directory is not canonical")?;
        let root_metadata = fs::symlink_metadata(root).map_err(|error| error.to_string())?;
        let metadata = fs::symlink_metadata(&mapped).map_err(|error| error.to_string())?;
        require(
            metadata.file_type().is_dir(),
            "fixtures path is not a directory",
        )?;
        require_same_owner(&metadata, &root_metadata, "fixtures directory")?;
        Ok(mapped)
    }

    pub(crate) fn baked_schema(&self) -> Result<SafeFile> {
        self.schema(SCHEMA_PATH)
    }
}

struct OutputTarget {
    parent: PathBuf,
    basename: String,
}

pub fn execute<I, S>(layout: &ExecutionLayout, arguments: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    match command::parse(arguments)? {
        Command::WriteExpected {
            work_root,
            app_repo_id,
            source_revision,
            app_cli_package,
            app_cli_bin,
            workspace_id,
            platform_id,
            provenance_protocol,
            output,
        } => write_expected(
            layout,
            &work_root,
            &app_repo_id,
            &source_revision,
            &app_cli_package,
            &app_cli_bin,
            &workspace_id,
            &platform_id,
            &provenance_protocol,
            &output,
        ),
        Command::WriteReleaseRequest {
            work_root,
            gate_sha,
            provenance_protocol,
            release_tag,
            output,
        } => write_release_request(
            layout,
            &work_root,
            &gate_sha,
            &provenance_protocol,
            &release_tag,
            &output,
        ),
        Command::Package {
            work_root,
            binary,
            schema,
            expected,
            app_cli_version,
            archive,
        } => package(
            layout,
            &work_root,
            &binary,
            &schema,
            &expected,
            &app_cli_version,
            &archive,
        ),
        Command::Validate {
            work_root,
            archive,
            schema,
            expected,
            output,
        } => validate(layout, &work_root, &archive, &schema, &expected, &output),
        Command::SelfTest { fixtures } => self_test::run(layout, &fixtures),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_expected(
    layout: &ExecutionLayout,
    work_root: &str,
    app_repo_id: &str,
    source_revision: &str,
    app_cli_package: &str,
    app_cli_bin: &str,
    workspace_id: &str,
    platform_id: &str,
    provenance_protocol: &str,
    output: &str,
) -> Result<()> {
    let work = layout.work_root(work_root)?;
    require_protocol(provenance_protocol)?;
    let document = Expected::new(
        app_repo_id,
        source_revision,
        app_cli_package,
        app_cli_bin,
        workspace_id,
        platform_id,
    )?;
    let bytes = document.canonical_bytes()?;
    let target = layout.output_target(output, &work)?;
    publish_bytes(&target.parent, &target.basename, &bytes, |path| {
        require(
            read_bounded_path(path, EXPECTED_LIMIT)? == bytes,
            "staged expected bytes changed",
        )?;
        Expected::parse(&bytes).map(|_| ())
    })
}

fn write_release_request(
    layout: &ExecutionLayout,
    work_root: &str,
    gate_sha: &str,
    provenance_protocol: &str,
    release_tag: &str,
    output: &str,
) -> Result<()> {
    let work = layout.work_root(work_root)?;
    require_protocol(provenance_protocol)?;
    require(nonzero_hex(gate_sha, 40), "invalid gate SHA")?;
    validate_release_tag(release_tag)?;
    let bytes = format!(
        "{{\"gate-sha\":\"{gate_sha}\",\"provenance-protocol\":1,\"release-tag\":\"{release_tag}\"}}"
    )
    .into_bytes();
    require_exact(output, "/work/release/release-request.json", "output path")?;
    let target = layout.output_target(output, &work)?;
    publish_bytes(&target.parent, &target.basename, &bytes, |path| {
        require(
            read_bounded_path(path, EXPECTED_LIMIT)? == bytes,
            "staged release request bytes changed",
        )
    })
}

fn package(
    layout: &ExecutionLayout,
    work_root: &str,
    binary: &str,
    schema: &str,
    expected: &str,
    app_cli_version: &str,
    archive_path: &str,
) -> Result<()> {
    let work = layout.work_root(work_root)?;
    let binary = layout.work_input(binary, &work)?;
    let expected = layout.work_input(expected, &work)?;
    let schema = layout.schema(schema)?;
    let target = layout.output_target(archive_path, &work)?;

    let schema_bytes = schema.read_bounded(SCHEMA_LIMIT)?;
    let expected_bytes = expected.read_bounded(EXPECTED_LIMIT)?;
    let identity = Expected::parse(&expected_bytes)?;
    validate_schema_instance(&schema_bytes, &expected_bytes)?;
    let inspection = elf::inspect(binary.path(), &layout.image_root()?)?;
    let abi = abi(&inspection)?;
    let metadata = ProvenanceMetadata::new(
        &identity,
        abi,
        app_cli_version,
        &inspection.binary_sha256,
        inspection.binary_size,
    )?;
    let metadata_bytes = metadata.canonical_bytes()?;
    validate_schema_instance(&schema_bytes, &metadata_bytes)?;
    let mut binary_file = binary.open()?;

    atomic_publish(
        &target.parent,
        &target.basename,
        0o644,
        |output| {
            archive::encode(
                output,
                &mut Cursor::new(&metadata_bytes),
                metadata_bytes.len() as u64,
                &mut binary_file,
                inspection.binary_size,
            )
        },
        |path| {
            let mut staged = File::open(path).map_err(|error| error.to_string())?;
            let parsed = archive::parse(&mut staged)?;
            require(parsed.metadata == metadata_bytes, "staged metadata changed")?;
            let parsed_metadata = ProvenanceMetadata::parse(&parsed.metadata)?;
            parsed_metadata.matches(&identity)?;
            validate_schema_instance(&schema_bytes, &parsed.metadata)?;
            require(
                parsed.binary_size == inspection.binary_size,
                "staged binary size changed",
            )?;
            archive::verify_binary_sha256(&mut staged, &parsed, &inspection.binary_sha256)?;
            binary.verify()?;
            expected.verify()?;
            schema.verify()
        },
    )?;
    Ok(())
}

fn validate(
    layout: &ExecutionLayout,
    work_root: &str,
    archive_path: &str,
    schema: &str,
    expected: &str,
    output: &str,
) -> Result<()> {
    let work = layout.work_root(work_root)?;
    let archive_input = layout.work_input(archive_path, &work)?;
    require(
        archive_input.size() <= ARCHIVE_LIMIT,
        "archive exceeds protocol bounds",
    )?;
    let expected = layout.work_input(expected, &work)?;
    let schema = layout.schema(schema)?;
    let target = layout.output_target(output, &work)?;

    let schema_bytes = schema.read_bounded(SCHEMA_LIMIT)?;
    let expected_bytes = expected.read_bounded(EXPECTED_LIMIT)?;
    let identity = Expected::parse(&expected_bytes)?;
    validate_schema_instance(&schema_bytes, &expected_bytes)?;
    let mut archive_file = archive_input.open()?;
    let parsed = archive::parse(&mut archive_file)?;
    let metadata = ProvenanceMetadata::parse(&parsed.metadata)?;
    metadata.matches(&identity)?;
    validate_schema_instance(&schema_bytes, &parsed.metadata)?;
    checked_seek(&mut archive_file, SeekFrom::Start(parsed.binary_offset))?;

    atomic_publish(
        &target.parent,
        &target.basename,
        0o755,
        |output| copy_exact(&mut archive_file, output, parsed.binary_size),
        |path| {
            let inspection = elf::inspect_staged(path, &layout.image_root()?)?;
            let observed_abi = abi(&inspection)?;
            metadata.matches_observation(
                &observed_abi,
                &inspection.binary_sha256,
                inspection.binary_size,
            )?;
            archive_input.verify()?;
            expected.verify()?;
            schema.verify()
        },
    )?;
    Ok(())
}

fn publish_bytes(
    parent: &Path,
    basename: &str,
    bytes: &[u8],
    validate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    atomic_publish(
        parent,
        basename,
        0o644,
        |output| output.write_all(bytes).map_err(|error| error.to_string()),
        validate,
    )?;
    Ok(())
}

pub(crate) fn abi(inspection: &elf::ElfInspection) -> Result<Abi> {
    let interpreter = match inspection.interpreter.as_deref() {
        None => Interpreter::Static,
        Some(crate::json_contract::INTERPRETER) => Interpreter::Dynamic,
        Some(_) => return Err("unsupported ELF interpreter".into()),
    };
    Abi::new(interpreter, inspection.needed.clone())
}

pub(crate) fn validate_schema_instance(schema: &[u8], instance: &[u8]) -> Result<()> {
    let schema: Value = serde_json::from_slice(schema)
        .map_err(|error| format!("invalid provenance schema: {error}"))?;
    require(
        jsonschema::draft202012::meta::is_valid(&schema),
        "invalid provenance schema",
    )?;
    let validator = jsonschema::draft202012::new(&schema)
        .map_err(|error| format!("invalid provenance schema: {error}"))?;
    let instance: Value = serde_json::from_slice(instance)
        .map_err(|error| format!("invalid provenance JSON: {error}"))?;
    require(
        validator.is_valid(&instance),
        "provenance JSON does not match schema",
    )
}

fn require_protocol(value: &str) -> Result<()> {
    require(value == "1", "unsupported provenance protocol")
}

fn validate_release_tag(value: &str) -> Result<()> {
    let decimal = value
        .strip_prefix("build-container-v")
        .ok_or("invalid release tag")?;
    require(
        decimal
            .as_bytes()
            .first()
            .is_some_and(|digit| (b'1'..=b'9').contains(digit))
            && decimal.as_bytes()[1..].iter().all(u8::is_ascii_digit),
        "noncanonical release tag",
    )
}

fn nonzero_hex(value: &str, width: usize) -> bool {
    value.len() == width
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && value.bytes().any(|byte| byte != b'0')
}

fn require_exact(actual: &str, expected: &str, label: &str) -> Result<()> {
    require(
        actual == expected,
        &format!("{label} is not the required literal"),
    )
}

fn require_work_path(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    require(path.is_absolute(), &format!("{label} is not absolute"))?;
    require(
        path.components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        &format!("{label} is not lexically canonical"),
    )?;
    let root = Path::new(WORK_ROOT);
    require(
        path.starts_with(root) && path != root,
        &format!("{label} escapes work root lexically"),
    )
}

fn read_bounded_path(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    require(metadata.len() <= limit as u64, "input exceeds byte limit")?;
    let file = File::open(path).map_err(|error| error.to_string())?;
    let bytes = read_bounded(file, metadata.len() as usize, limit)?;
    require(
        bytes.len() as u64 == metadata.len(),
        "input size changed while reading",
    )?;
    Ok(bytes)
}

fn read_bounded(reader: impl Read, capacity: usize, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(capacity.min(limit));
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    require(bytes.len() <= limit, "input exceeds byte limit")?;
    Ok(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    links: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    #[cfg(unix)]
    fn from(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    #[cfg(not(unix))]
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: 0,
            inode: 0,
            size: metadata.len(),
            mode: 0,
            links: 1,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SafeFile {
    path: PathBuf,
    identity: FileIdentity,
}

impl SafeFile {
    pub(crate) fn open_path(
        path: PathBuf,
        confinement: Option<&Path>,
        image_owner: Option<&Metadata>,
    ) -> Result<Self> {
        let canonical = path.canonicalize().map_err(|error| error.to_string())?;
        require(canonical == path, "input path is not canonical")?;
        if let Some(root) = confinement {
            require(
                canonical.starts_with(root) && canonical != root,
                "input path escapes work root canonically",
            )?;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        require(
            metadata.file_type().is_file(),
            "input is not a regular file",
        )?;
        let identity = FileIdentity::from(&metadata);
        require(identity.links == 1, "input has multiple links")?;
        if let Some(owner) = image_owner {
            require_same_owner(&metadata, owner, "image file")?;
        }
        let safe = Self { path, identity };
        drop(safe.open()?);
        Ok(safe)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn size(&self) -> u64 {
        self.identity.size
    }

    pub(crate) fn open(&self) -> Result<File> {
        let file = File::open(&self.path).map_err(|error| error.to_string())?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        require(
            metadata.file_type().is_file(),
            "input is not a regular file",
        )?;
        require(
            FileIdentity::from(&metadata) == self.identity,
            "input changed while opening",
        )?;
        Ok(file)
    }

    pub(crate) fn read_bounded(&self, limit: usize) -> Result<Vec<u8>> {
        require(
            self.identity.size <= limit as u64,
            "input exceeds byte limit",
        )?;
        let bytes = read_bounded(self.open()?, self.identity.size as usize, limit)?;
        require(
            bytes.len() as u64 == self.identity.size,
            "input size changed while reading",
        )?;
        self.verify()?;
        Ok(bytes)
    }

    pub(crate) fn verify(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.path).map_err(|error| error.to_string())?;
        require(
            metadata.file_type().is_file() && FileIdentity::from(&metadata) == self.identity,
            "input changed during validation",
        )
    }
}

#[cfg(unix)]
fn require_same_owner(actual: &Metadata, expected: &Metadata, label: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    require(
        actual.uid() == expected.uid() && actual.gid() == expected.gid(),
        &format!("{label} is not image-owned"),
    )
}

#[cfg(not(unix))]
fn require_same_owner(_actual: &Metadata, _expected: &Metadata, _label: &str) -> Result<()> {
    Err("image ownership validation requires Unix".into())
}
