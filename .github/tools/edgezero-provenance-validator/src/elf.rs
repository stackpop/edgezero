use crate::{
    Result,
    json_contract::{BINARY_LIMIT, INTERPRETER, METADATA_LIMIT},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const DYNAMIC_ENTRY_SIZE: u64 = 16;
const IO_CHUNK_SIZE: usize = 8 * 1024;
// A nonempty needed name occupies at least two quotes and one payload byte in
// canonical JSON, so a larger list cannot fit in protocol metadata.
const MIN_CANONICAL_NEEDED_ITEM_BYTES: usize = 3;
const MAX_NEEDED_ENTRIES: usize = METADATA_LIMIT / MIN_CANONICAL_NEEDED_ITEM_BYTES;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u32 = 1;
const ELFOSABI_SYSV: u8 = 0;
const ELFOSABI_GNU: u8 = 3;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PN_XNUM: u16 = 0xffff;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PF_R: u32 = 4;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
const DT_SONAME: u64 = 14;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;
const DT_FLAGS: u64 = 30;
const DT_POSFLAG_1: u64 = 0x6ffffdfd;
const DT_CONFIG: u64 = 0x6ffffefa;
const DT_DEPAUDIT: u64 = 0x6ffffefb;
const DT_AUDIT: u64 = 0x6ffffefc;
const DT_FLAGS_1: u64 = 0x6ffffffb;
const DT_AUXILIARY: u64 = 0x7ffffffd;
const DT_FILTER: u64 = 0x7fffffff;
const FLAGS_MASK: u64 = 0x0000001e;
const FLAGS_1_MASK: u64 = 0x5eff976f;
const RUNTIME_LIB: &str = "/opt/edgezero/runtime-lib";
const APP_PATH: &str = "/work/bin/app-cli";
const APP_NAME: &str = "app-cli";
const LOADER_NAME: &str = "ld-linux-x86-64.so.2";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosureClaim {
    StartupOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectRole {
    Primary,
    Interpreter,
    Library,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisitedObject {
    pub image_path: String,
    pub role: ObjectRole,
    pub device: u64,
    pub inode: u64,
    pub needed: Vec<String>,
    pub soname: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfInspection {
    pub machine: String,
    pub interpreter: Option<String>,
    pub needed: Vec<String>,
    pub binary_sha256: String,
    pub binary_size: u64,
    pub visited: Vec<VisitedObject>,
    pub claim: ClosureClaim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchCommand {
    pub program: String,
    pub args: Vec<String>,
}

pub fn direct_launch(
    interpreter: Option<&str>,
    operation_args: &[String],
) -> Result<LaunchCommand> {
    let mut args = match interpreter {
        None => vec![APP_PATH.into()],
        Some(INTERPRETER) => vec![
            INTERPRETER.into(),
            "--inhibit-cache".into(),
            "--glibc-hwcaps-mask".into(),
            String::new(),
            "--library-path".into(),
            RUNTIME_LIB.into(),
            APP_PATH.into(),
        ],
        Some(_) => return Err("unsupported ELF interpreter".into()),
    };
    args.extend_from_slice(operation_args);
    Ok(LaunchCommand {
        program: "/usr/bin/env".into(),
        args,
    })
}

#[cfg(unix)]
pub fn inspect(primary_path: &Path, image_root: &Path) -> Result<ElfInspection> {
    validate_root(image_root)?;
    require_no_preload(image_root)?;
    require(
        primary_path.file_name().and_then(|name| name.to_str()) == Some(APP_NAME),
        "primary filename is not app-cli",
    )?;

    let (primary, mut primary_file) = open_object(primary_path, APP_PATH, ObjectRole::Primary)?;
    let binary_size = primary_file.metadata().map_err(io_error)?.len();
    let binary_sha256 = hash_file(&mut primary_file, binary_size)?;
    let mut direct_needed = primary.parsed.needed.clone();
    direct_needed.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    let Some(interpreter) = primary.parsed.interpreter.as_deref() else {
        return Ok(ElfInspection {
            machine: "x86_64".into(),
            interpreter: None,
            needed: direct_needed,
            binary_sha256,
            binary_size,
            visited: vec![primary.visited()],
            claim: ClosureClaim::StartupOnly,
        });
    };
    require(interpreter == INTERPRETER, "unsupported ELF interpreter")?;

    validate_relative_directory(image_root, &["lib64"])?;
    let loader_path = image_root.join("lib64").join(LOADER_NAME);
    let (loader, _) = open_object(&loader_path, INTERPRETER, ObjectRole::Interpreter)?;

    validate_relative_directory(image_root, &["opt", "edgezero", "runtime-lib"])?;
    let runtime_path = image_root.join("opt/edgezero/runtime-lib");
    let libraries = scan_runtime_libraries(&runtime_path)?;
    validate_aliases(&primary, &loader, &libraries)?;

    let mut visited_identities = HashSet::new();
    let mut visited = Vec::new();
    add_visited(&primary, &mut visited_identities, &mut visited);
    add_visited(&loader, &mut visited_identities, &mut visited);
    visit_dependencies(
        &loader.parsed.needed,
        &loader,
        &libraries,
        &mut visited_identities,
        &mut visited,
    )?;
    visit_dependencies(
        &primary.parsed.needed,
        &loader,
        &libraries,
        &mut visited_identities,
        &mut visited,
    )?;

    Ok(ElfInspection {
        machine: "x86_64".into(),
        interpreter: Some(INTERPRETER.into()),
        needed: direct_needed,
        binary_sha256,
        binary_size,
        visited,
        claim: ClosureClaim::StartupOnly,
    })
}

#[cfg(not(unix))]
pub fn inspect(_primary_path: &Path, _image_root: &Path) -> Result<ElfInspection> {
    Err("Protocol-1 ELF inspection requires Unix device and inode metadata".into())
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug)]
struct Object {
    image_path: String,
    role: ObjectRole,
    identity: Identity,
    parsed: ParsedElf,
}

impl Object {
    fn visited(&self) -> VisitedObject {
        VisitedObject {
            image_path: self.image_path.clone(),
            role: self.role,
            device: self.identity.device,
            inode: self.identity.inode,
            needed: self.parsed.needed.clone(),
            soname: self.parsed.soname.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct ParsedElf {
    interpreter: Option<String>,
    needed: Vec<String>,
    soname: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

#[derive(Clone, Copy)]
enum StringKind {
    Needed,
    Soname,
}

#[cfg(unix)]
fn open_object(path: &Path, image_path: &str, role: ObjectRole) -> Result<(Object, File)> {
    use std::os::unix::fs::MetadataExt;

    let path_metadata = fs::symlink_metadata(path).map_err(io_error)?;
    require(
        path_metadata.file_type().is_file(),
        "ELF object is not a regular file",
    )?;
    require(path_metadata.nlink() == 1, "ELF object has multiple links")?;
    let mut file = File::open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    require(
        metadata.file_type().is_file(),
        "ELF object is not a regular file",
    )?;
    require(metadata.nlink() == 1, "ELF object has multiple links")?;
    require(
        path_metadata.dev() == metadata.dev() && path_metadata.ino() == metadata.ino(),
        "ELF object changed while opening",
    )?;
    require(
        (1..=BINARY_LIMIT).contains(&metadata.len()),
        "ELF object size is outside protocol bounds",
    )?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("ELF filename is not UTF-8")?;
    let parsed = parse_elf(&mut file, metadata.len(), role, filename)?;
    Ok((
        Object {
            image_path: image_path.into(),
            role,
            identity: Identity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            parsed,
        },
        file,
    ))
}

fn parse_elf<R: Read + Seek>(
    reader: &mut R,
    file_size: u64,
    role: ObjectRole,
    filename: &str,
) -> Result<ParsedElf> {
    let header = read_range(reader, 0, ELF_HEADER_SIZE, file_size)?;
    require(&header[..4] == b"\x7fELF", "invalid ELF magic")?;
    require(header[4] == ELFCLASS64, "ELF is not 64-bit")?;
    require(header[5] == ELFDATA2LSB, "ELF is not little-endian")?;
    require(
        header[6] == EV_CURRENT as u8,
        "unsupported ELF ident version",
    )?;
    require(
        matches!(header[7], ELFOSABI_SYSV | ELFOSABI_GNU),
        "unsupported ELF OSABI",
    )?;
    require(header[8] == 0, "unsupported ELF ABI version")?;
    require(
        header[9..16].iter().all(|byte| *byte == 0),
        "nonzero ELF ident padding",
    )?;

    let elf_type = u16_at(&header, 16);
    let valid_type = match role {
        ObjectRole::Primary => matches!(elf_type, ET_EXEC | ET_DYN),
        ObjectRole::Interpreter | ObjectRole::Library => elf_type == ET_DYN,
    };
    require(valid_type, "unsupported ELF object type")?;
    require(u16_at(&header, 18) == EM_X86_64, "unsupported ELF machine")?;
    require(u32_at(&header, 20) == EV_CURRENT, "unsupported ELF version")?;
    require(u32_at(&header, 48) == 0, "unsupported ELF flags")?;
    require(
        usize::from(u16_at(&header, 52)) == ELF_HEADER_SIZE,
        "invalid ELF header size",
    )?;
    require(
        usize::from(u16_at(&header, 54)) == PROGRAM_HEADER_SIZE,
        "invalid program header size",
    )?;
    let phnum = u16_at(&header, 56);
    require(phnum != 0, "ELF has no program headers")?;
    require(
        phnum != PN_XNUM,
        "extended program header numbering is unsupported",
    )?;
    let phoff = u64_at(&header, 32);
    let table_size = u64::from(phnum)
        .checked_mul(PROGRAM_HEADER_SIZE as u64)
        .ok_or("program header table size overflow")?;
    checked_range(phoff, table_size, file_size, "program header table")?;

    let mut programs = Vec::with_capacity(usize::from(phnum));
    for index in 0..u64::from(phnum) {
        let offset = phoff
            .checked_add(
                index
                    .checked_mul(PROGRAM_HEADER_SIZE as u64)
                    .ok_or("program header offset overflow")?,
            )
            .ok_or("program header offset overflow")?;
        let bytes = read_range(reader, offset, PROGRAM_HEADER_SIZE, file_size)?;
        let program = ProgramHeader {
            kind: u32_at(&bytes, 0),
            flags: u32_at(&bytes, 4),
            offset: u64_at(&bytes, 8),
            vaddr: u64_at(&bytes, 16),
            filesz: u64_at(&bytes, 32),
            memsz: u64_at(&bytes, 40),
        };
        if program.kind == PT_LOAD {
            require(
                program.filesz <= program.memsz,
                "PT_LOAD file size exceeds memory size",
            )?;
        }
        checked_range(program.offset, program.filesz, file_size, "program segment")?;
        program
            .vaddr
            .checked_add(program.filesz)
            .ok_or("program file-backed address overflow")?;
        program
            .vaddr
            .checked_add(program.memsz)
            .ok_or("program memory address overflow")?;
        programs.push(program);
    }

    let interps: Vec<_> = programs
        .iter()
        .filter(|program| program.kind == PT_INTERP)
        .collect();
    require(interps.len() <= 1, "multiple PT_INTERP segments")?;
    let interpreter = interps
        .first()
        .map(|program| parse_interpreter(reader, file_size, &programs, program))
        .transpose()?;

    let dynamics: Vec<_> = programs
        .iter()
        .filter(|program| program.kind == PT_DYNAMIC)
        .collect();
    require(dynamics.len() <= 1, "multiple PT_DYNAMIC segments")?;
    let dynamic = dynamics
        .first()
        .map(|program| parse_dynamic(reader, file_size, &programs, program))
        .transpose()?;
    let parsed = dynamic.unwrap_or(ParsedElf {
        interpreter: None,
        needed: Vec::new(),
        soname: None,
    });
    let is_dynamic = interpreter.is_some() || !parsed.needed.is_empty();

    match role {
        ObjectRole::Primary => {
            require(parsed.soname.is_none(), "primary ELF has DT_SONAME")?;
            require(
                if is_dynamic {
                    dynamics.len() == 1 && interpreter.as_deref() == Some(INTERPRETER)
                } else {
                    dynamics.is_empty() && interpreter.is_none()
                },
                "primary static/dynamic profile is inconsistent",
            )?;
        }
        ObjectRole::Library => {
            require(dynamics.len() == 1, "library has no PT_DYNAMIC")?;
            require(
                interpreter
                    .as_deref()
                    .is_none_or(|value| value == INTERPRETER),
                "library has unsupported PT_INTERP",
            )?;
            validate_name(filename, "library filename")?;
            require(
                !matches!(filename, APP_NAME | LOADER_NAME),
                "reserved runtime library filename",
            )?;
            if let Some(soname) = &parsed.soname {
                validate_name(soname, "library SONAME")?;
                require(soname == filename, "library SONAME differs from filename")?;
            }
        }
        ObjectRole::Interpreter => {
            require(dynamics.len() == 1, "interpreter has no PT_DYNAMIC")?;
            require(interpreter.is_none(), "interpreter has PT_INTERP")?;
            if let Some(soname) = &parsed.soname {
                require(soname == LOADER_NAME, "interpreter has unsupported SONAME")?;
            }
        }
    }

    Ok(ParsedElf {
        interpreter,
        ..parsed
    })
}

fn parse_interpreter<R: Read + Seek>(
    reader: &mut R,
    file_size: u64,
    programs: &[ProgramHeader],
    program: &ProgramHeader,
) -> Result<String> {
    require(
        program.filesz == program.memsz,
        "PT_INTERP file and memory sizes differ",
    )?;
    require(
        program.filesz == (INTERPRETER.len() + 1) as u64,
        "unsupported PT_INTERP size",
    )?;
    let mapped = map_virtual(programs, program.vaddr, program.filesz)?;
    require(mapped == program.offset, "contradictory PT_INTERP mapping")?;
    let bytes = read_range(reader, program.offset, INTERPRETER.len() + 1, file_size)?;
    require(bytes.last() == Some(&0), "PT_INTERP is not NUL terminated")?;
    require(
        !bytes[..bytes.len() - 1].contains(&0),
        "PT_INTERP contains interior NUL",
    )?;
    let value =
        std::str::from_utf8(&bytes[..bytes.len() - 1]).map_err(|_| "PT_INTERP is not UTF-8")?;
    require(
        !value.chars().any(char::is_control),
        "PT_INTERP contains a control character",
    )?;
    require(value == INTERPRETER, "unsupported ELF interpreter")?;
    Ok(value.into())
}

fn parse_dynamic<R: Read + Seek>(
    reader: &mut R,
    file_size: u64,
    programs: &[ProgramHeader],
    dynamic: &ProgramHeader,
) -> Result<ParsedElf> {
    require(dynamic.filesz != 0, "empty PT_DYNAMIC")?;
    require(
        dynamic.filesz == dynamic.memsz,
        "PT_DYNAMIC file and memory sizes differ",
    )?;
    require(
        dynamic.filesz.is_multiple_of(DYNAMIC_ENTRY_SIZE),
        "malformed PT_DYNAMIC entry width",
    )?;
    let mapped = map_virtual(programs, dynamic.vaddr, dynamic.filesz)?;
    require(mapped == dynamic.offset, "contradictory PT_DYNAMIC mapping")?;

    let mut seen = HashSet::new();
    let mut string_refs = Vec::new();
    let mut strtab = None;
    let mut strsz = None;
    let mut terminated_at = None;
    let mut needed_count = 0;
    let entries = dynamic.filesz / DYNAMIC_ENTRY_SIZE;
    let entries_per_chunk = IO_CHUNK_SIZE as u64 / DYNAMIC_ENTRY_SIZE;
    let mut index = 0;
    'dynamic: while index < entries {
        let chunk_entries = (entries - index).min(entries_per_chunk);
        let chunk_size = chunk_entries
            .checked_mul(DYNAMIC_ENTRY_SIZE)
            .ok_or("dynamic chunk size overflow")?;
        let chunk_offset = dynamic
            .offset
            .checked_add(
                index
                    .checked_mul(DYNAMIC_ENTRY_SIZE)
                    .ok_or("dynamic entry offset overflow")?,
            )
            .ok_or("dynamic entry offset overflow")?;
        let chunk = read_range(
            reader,
            chunk_offset,
            usize::try_from(chunk_size).map_err(|_| "dynamic chunk size overflow")?,
            file_size,
        )?;
        for (chunk_index, entry) in chunk.chunks_exact(DYNAMIC_ENTRY_SIZE as usize).enumerate() {
            let entry_offset = chunk_offset
                .checked_add(
                    (chunk_index as u64)
                        .checked_mul(DYNAMIC_ENTRY_SIZE)
                        .ok_or("dynamic entry offset overflow")?,
                )
                .ok_or("dynamic entry offset overflow")?;
            let tag = u64_at(entry, 0);
            let value = u64_at(entry, 8);
            if tag == DT_NULL {
                require(value == 0, "nonzero DT_NULL value")?;
                terminated_at = Some(
                    entry_offset
                        .checked_add(DYNAMIC_ENTRY_SIZE)
                        .ok_or("dynamic terminator overflow")?,
                );
                break 'dynamic;
            }
            require(!is_forbidden_tag(tag), "forbidden dynamic tag")?;
            require(is_allowed_tag(tag), "unknown dynamic tag")?;
            if tag != DT_NEEDED {
                require(seen.insert(tag), "duplicate singleton dynamic tag")?;
            }
            match tag {
                DT_NEEDED => {
                    require(
                        needed_count < MAX_NEEDED_ENTRIES,
                        "too many DT_NEEDED entries",
                    )?;
                    needed_count += 1;
                    string_refs.push((StringKind::Needed, value));
                }
                DT_SONAME => string_refs.push((StringKind::Soname, value)),
                DT_STRTAB => strtab = Some(value),
                DT_STRSZ => strsz = Some(value),
                DT_FLAGS => require(value & !FLAGS_MASK == 0, "unsupported DT_FLAGS bits")?,
                DT_FLAGS_1 => require(value & !FLAGS_1_MASK == 0, "unsupported DT_FLAGS_1 bits")?,
                _ => {}
            }
        }
        index = index
            .checked_add(chunk_entries)
            .ok_or("dynamic entry index overflow")?;
    }

    let trailing_start = terminated_at.ok_or("PT_DYNAMIC has no DT_NULL")?;
    let dynamic_end = dynamic
        .offset
        .checked_add(dynamic.filesz)
        .ok_or("dynamic range overflow")?;
    require_zero_range(reader, trailing_start, dynamic_end, file_size)?;
    require(
        strtab.is_some() == strsz.is_some(),
        "incomplete dynamic string table",
    )?;
    require(
        string_refs.is_empty() || strtab.is_some(),
        "dynamic string tag has no string table",
    )?;

    let mut needed = Vec::new();
    let mut soname = None;
    if let (Some(vaddr), Some(size)) = (strtab, strsz) {
        require(size != 0, "empty dynamic string table")?;
        let table_offset = map_virtual(programs, vaddr, size)?;
        for (kind, offset) in string_refs {
            let value = read_dynamic_string(reader, file_size, table_offset, size, offset)?;
            match kind {
                StringKind::Needed => {
                    validate_name(&value, "DT_NEEDED")?;
                    needed.push(value);
                }
                StringKind::Soname => {
                    validate_name(&value, "DT_SONAME")?;
                    soname = Some(value);
                }
            }
        }
    }

    Ok(ParsedElf {
        interpreter: None,
        needed,
        soname,
    })
}

fn map_virtual(programs: &[ProgramHeader], vaddr: u64, size: u64) -> Result<u64> {
    require(size != 0, "empty mapped range")?;
    let end = vaddr.checked_add(size).ok_or("virtual range overflow")?;
    let mut intersections = 0;
    let mut mapping = None;
    for load in programs.iter().filter(|program| program.kind == PT_LOAD) {
        let memory_end = load
            .vaddr
            .checked_add(load.memsz)
            .ok_or("PT_LOAD range overflow")?;
        if vaddr < memory_end && load.vaddr < end {
            intersections += 1;
        }

        let file_backed_end = load
            .vaddr
            .checked_add(load.filesz)
            .ok_or("PT_LOAD file-backed range overflow")?;
        if load.flags & PF_R != 0
            && vaddr >= load.vaddr
            && end <= file_backed_end
            && end <= memory_end
        {
            let delta = vaddr
                .checked_sub(load.vaddr)
                .ok_or("virtual mapping underflow")?;
            let offset = load
                .offset
                .checked_add(delta)
                .ok_or("file mapping overflow")?;
            let offset_end = offset.checked_add(size).ok_or("file mapping overflow")?;
            let load_file_end = load
                .offset
                .checked_add(load.filesz)
                .ok_or("PT_LOAD file range overflow")?;
            if offset_end <= load_file_end {
                require(mapping.is_none(), "multiple loader-visible mappings")?;
                mapping = Some(offset);
            }
        }
    }
    require(
        intersections == 1,
        "mapped range is not uniquely readable and file-backed",
    )?;
    mapping.ok_or_else(|| "mapped range is not readable and file-backed".into())
}

fn read_dynamic_string<R: Read + Seek>(
    reader: &mut R,
    file_size: u64,
    table_offset: u64,
    table_size: u64,
    string_offset: u64,
) -> Result<String> {
    require(
        string_offset < table_size,
        "dynamic string offset is out of range",
    )?;
    let remaining = table_size
        .checked_sub(string_offset)
        .ok_or("dynamic string range underflow")?;
    let read_size = remaining.min(256);
    let offset = table_offset
        .checked_add(string_offset)
        .ok_or("dynamic string offset overflow")?;
    let bytes = read_range(
        reader,
        offset,
        usize::try_from(read_size).map_err(|_| "dynamic string size overflow")?,
        file_size,
    )?;
    let terminator = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("dynamic string is unterminated or oversized")?;
    let value =
        std::str::from_utf8(&bytes[..terminator]).map_err(|_| "dynamic string is not UTF-8")?;
    require(
        !value.chars().any(char::is_control),
        "dynamic string contains a control character",
    )?;
    Ok(value.into())
}

fn is_forbidden_tag(tag: u64) -> bool {
    matches!(
        tag,
        DT_RPATH
            | DT_RUNPATH
            | DT_AUDIT
            | DT_DEPAUDIT
            | DT_CONFIG
            | DT_AUXILIARY
            | DT_FILTER
            | DT_POSFLAG_1
    )
}

fn is_allowed_tag(tag: u64) -> bool {
    (tag <= 14)
        || (16..=28).contains(&tag)
        || tag == 30
        || (32..=37).contains(&tag)
        || matches!(tag, 0x6ffffef5..=0x6ffffef7 | 0x6ffffff0 | 0x6ffffff9..=0x6fffffff)
        || matches!(tag, 0x70000000 | 0x70000001 | 0x70000003)
}

#[cfg(unix)]
fn scan_runtime_libraries(runtime_path: &Path) -> Result<BTreeMap<String, Object>> {
    let mut libraries = BTreeMap::new();
    for entry in fs::read_dir(runtime_path).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "runtime library filename is not UTF-8")?;
        validate_name(&name, "runtime library filename")?;
        require(
            !matches!(name.as_str(), APP_NAME | LOADER_NAME),
            "reserved runtime library filename",
        )?;
        require(
            !libraries.contains_key(&name),
            "duplicate runtime library basename",
        )?;
        let image_path = format!("{RUNTIME_LIB}/{name}");
        let (object, _) = open_object(&entry.path(), &image_path, ObjectRole::Library)?;
        libraries.insert(name, object);
    }
    Ok(libraries)
}

fn validate_aliases(
    primary: &Object,
    loader: &Object,
    libraries: &BTreeMap<String, Object>,
) -> Result<()> {
    let mut aliases: HashMap<String, Identity> = HashMap::new();
    let mut identities: HashMap<Identity, String> = HashMap::new();
    register_object_aliases(primary, APP_NAME, &mut aliases, &mut identities)?;
    register_object_aliases(loader, LOADER_NAME, &mut aliases, &mut identities)?;
    for (filename, object) in libraries {
        register_object_aliases(object, filename, &mut aliases, &mut identities)?;
    }
    Ok(())
}

fn register_object_aliases(
    object: &Object,
    filename: &str,
    aliases: &mut HashMap<String, Identity>,
    identities: &mut HashMap<Identity, String>,
) -> Result<()> {
    if let Some(existing) = identities.insert(object.identity, object.image_path.clone()) {
        require(
            existing == object.image_path,
            "distinct ELF paths share device and inode",
        )?;
    }
    for alias in [Some(filename), object.parsed.soname.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(existing) = aliases.insert(alias.into(), object.identity) {
            require(
                existing == object.identity,
                "cross-object ELF alias collision",
            )?;
        }
    }
    Ok(())
}

fn visit_dependencies(
    dependencies: &[String],
    loader: &Object,
    libraries: &BTreeMap<String, Object>,
    identities: &mut HashSet<Identity>,
    visited: &mut Vec<VisitedObject>,
) -> Result<()> {
    for dependency in dependencies {
        let object = if dependency == LOADER_NAME {
            loader
        } else {
            libraries
                .get(dependency)
                .ok_or_else(|| format!("missing runtime dependency: {dependency}"))?
        };
        if identities.insert(object.identity) {
            visited.push(object.visited());
            visit_dependencies(
                &object.parsed.needed,
                loader,
                libraries,
                identities,
                visited,
            )?;
        }
    }
    Ok(())
}

fn add_visited(
    object: &Object,
    identities: &mut HashSet<Identity>,
    visited: &mut Vec<VisitedObject>,
) {
    if identities.insert(object.identity) {
        visited.push(object.visited());
    }
}

fn validate_name(value: &str, label: &str) -> Result<()> {
    require(
        (1..=255).contains(&value.len())
            && !value
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\' | '$')),
        &format!("invalid {label}"),
    )
}

fn validate_root(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root).map_err(io_error)?;
    require(
        metadata.file_type().is_dir(),
        "image root is not a real directory",
    )
}

fn require_no_preload(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root.join("etc/ld.so.preload")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
        Ok(_) => Err("image contains /etc/ld.so.preload".into()),
    }
}

fn validate_relative_directory(root: &Path, components: &[&str]) -> Result<()> {
    let mut path = PathBuf::from(root);
    for component in components {
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        require(
            metadata.file_type().is_dir(),
            "image directory component is not a real directory",
        )?;
    }
    Ok(())
}

fn hash_file(file: &mut File, size: u64) -> Result<String> {
    seek(file, 0)?;
    let mut remaining = size;
    let mut hasher = Sha256::new();
    let mut buffer = [0; IO_CHUNK_SIZE];
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| "ELF hash size overflow")?;
        let read = read_retry(file, &mut buffer[..limit])?;
        require(read != 0, "ELF changed while hashing")?;
        hasher.update(&buffer[..read]);
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or("ELF hash size underflow")?;
    }
    let mut extra = [0];
    require(
        read_retry(file, &mut extra)? == 0,
        "ELF changed while hashing",
    )?;
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn read_range<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    size: usize,
    file_size: u64,
) -> Result<Vec<u8>> {
    checked_range(offset, size as u64, file_size, "ELF read")?;
    seek(reader, offset)?;
    let mut bytes = vec![0; size];
    let mut filled = 0;
    while filled < bytes.len() {
        let end = (filled + IO_CHUNK_SIZE).min(bytes.len());
        let read = read_retry(reader, &mut bytes[filled..end])?;
        require(read != 0, "unexpected end of ELF object")?;
        filled = filled.checked_add(read).ok_or("ELF read size overflow")?;
    }
    Ok(bytes)
}

fn require_zero_range<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    end: u64,
    file_size: u64,
) -> Result<()> {
    require(start <= end, "invalid zero range")?;
    checked_range(start, end - start, file_size, "dynamic trailing bytes")?;
    seek(reader, start)?;
    let mut remaining = end - start;
    let mut buffer = [0; IO_CHUNK_SIZE];
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| "dynamic trailing size overflow")?;
        let read = read_retry(reader, &mut buffer[..limit])?;
        require(read != 0, "unexpected end of PT_DYNAMIC")?;
        require(
            buffer[..read].iter().all(|byte| *byte == 0),
            "nonzero bytes after DT_NULL",
        )?;
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or("dynamic trailing size underflow")?;
    }
    Ok(())
}

fn checked_range(offset: u64, size: u64, file_size: u64, label: &str) -> Result<()> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| format!("{label} overflow"))?;
    require(
        end <= file_size,
        &format!("{label} is outside the ELF object"),
    )
}

fn seek<R: Seek>(reader: &mut R, offset: u64) -> Result<()> {
    loop {
        match reader.seek(SeekFrom::Start(offset)) {
            Ok(actual) => return require(actual == offset, "ELF seek returned wrong offset"),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn read_retry<R: Read>(reader: &mut R, bytes: &mut [u8]) -> Result<usize> {
    loop {
        match reader.read(bytes) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result.map_err(io_error),
        }
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("fixed ELF field"),
    )
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed ELF field"),
    )
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed ELF field"),
    )
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

fn require(valid: bool, reason: &str) -> Result<()> {
    if valid { Ok(()) } else { Err(reason.into()) }
}

#[cfg(test)]
mod tests {
    use super::{ClosureClaim, ObjectRole, direct_launch, inspect};
    use crate::json_contract::{INTERPRETER, METADATA_LIMIT};
    use std::{
        fs,
        io::{self, Cursor, Read, Seek, SeekFrom},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    const ELF_HEADER_SIZE: usize = 64;
    const PROGRAM_HEADER_SIZE: usize = 56;
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const PT_INTERP: u32 = 3;
    const PF_R: u32 = 4;
    const ET_EXEC: u16 = 2;
    const ET_DYN: u16 = 3;
    const BASE_VADDR: u64 = 0x400000;
    const INTERP_OFFSET: usize = 0x240;
    const DYNAMIC_OFFSET: usize = 0x300;
    const STRTAB_OFFSET: usize = 0x500;
    const FILE_SIZE: usize = 0x800;
    const LOADER_NAME: &str = "ld-linux-x86-64.so.2";

    const DT_NULL: u64 = 0;
    const DT_NEEDED: u64 = 1;
    const DT_STRTAB: u64 = 5;
    const DT_STRSZ: u64 = 10;
    const DT_SONAME: u64 = 14;
    const DT_RPATH: u64 = 15;
    const DT_RUNPATH: u64 = 29;
    const DT_FLAGS: u64 = 30;
    const DT_POSFLAG_1: u64 = 0x6ffffdfd;
    const DT_CONFIG: u64 = 0x6ffffefa;
    const DT_DEPAUDIT: u64 = 0x6ffffefb;
    const DT_AUDIT: u64 = 0x6ffffefc;
    const DT_FLAGS_1: u64 = 0x6ffffffb;
    const DT_AUXILIARY: u64 = 0x7ffffffd;
    const DT_FILTER: u64 = 0x7fffffff;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = Path::new("/tmp").join(format!("ezelf-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    struct Image {
        temp: TempDir,
    }

    impl Image {
        fn new() -> Self {
            Self {
                temp: TempDir::new(),
            }
        }

        fn root(&self) -> &Path {
            self.temp.path()
        }

        fn write(&self, image_path: &str, bytes: &[u8]) -> PathBuf {
            let path = self.root().join(image_path.trim_start_matches('/'));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            path
        }

        fn primary(&self, bytes: &[u8]) -> PathBuf {
            self.write("/work/bin/app-cli", bytes)
        }

        fn loader(&self, bytes: &[u8]) -> PathBuf {
            self.write(INTERPRETER, bytes)
        }

        fn library(&self, name: &str, bytes: &[u8]) -> PathBuf {
            self.write(&format!("/opt/edgezero/runtime-lib/{name}"), bytes)
        }

        fn runtime_dir(&self) -> PathBuf {
            let path = self.root().join("opt/edgezero/runtime-lib");
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn inspect(&self, primary: &Path) -> crate::Result<super::ElfInspection> {
            inspect(primary, self.root())
        }
    }

    #[derive(Clone)]
    struct Fixture {
        elf_type: u16,
        osabi: u8,
        interp: Option<Vec<u8>>,
        dynamic: bool,
        needed: Vec<Vec<u8>>,
        soname: Option<Vec<u8>>,
        extra_tags: Vec<(u64, u64)>,
    }

    impl Fixture {
        fn static_primary() -> Self {
            Self {
                elf_type: ET_EXEC,
                osabi: 0,
                interp: None,
                dynamic: false,
                needed: Vec::new(),
                soname: None,
                extra_tags: Vec::new(),
            }
        }

        fn dynamic_primary(needed: &[&str]) -> Self {
            Self {
                elf_type: ET_EXEC,
                osabi: 0,
                interp: Some(nul(INTERPRETER.as_bytes())),
                dynamic: true,
                needed: needed
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect(),
                soname: None,
                extra_tags: Vec::new(),
            }
        }

        fn library(needed: &[&str]) -> Self {
            Self {
                elf_type: ET_DYN,
                osabi: 3,
                interp: None,
                dynamic: true,
                needed: needed
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect(),
                soname: None,
                extra_tags: Vec::new(),
            }
        }

        fn loader() -> Self {
            Self::library(&[])
        }

        fn build(&self) -> Vec<u8> {
            let mut strings = vec![0];
            let mut string_tags = Vec::new();
            for needed in &self.needed {
                let offset = strings.len() as u64;
                strings.extend_from_slice(needed);
                strings.push(0);
                string_tags.push((DT_NEEDED, offset));
            }
            if let Some(soname) = &self.soname {
                let offset = strings.len() as u64;
                strings.extend_from_slice(soname);
                strings.push(0);
                string_tags.push((DT_SONAME, offset));
            }

            let mut tags = Vec::new();
            if !string_tags.is_empty() {
                tags.push((DT_STRTAB, BASE_VADDR + STRTAB_OFFSET as u64));
                tags.push((DT_STRSZ, strings.len() as u64));
                tags.extend(string_tags);
            }
            tags.extend_from_slice(&self.extra_tags);
            tags.push((DT_NULL, 0));

            let phnum = 1 + usize::from(self.interp.is_some()) + usize::from(self.dynamic);
            let mut bytes = vec![0; FILE_SIZE];
            bytes[..4].copy_from_slice(b"\x7fELF");
            bytes[4] = 2;
            bytes[5] = 1;
            bytes[6] = 1;
            bytes[7] = self.osabi;
            bytes[8] = 0;
            put_u16(&mut bytes, 16, self.elf_type);
            put_u16(&mut bytes, 18, 62);
            put_u32(&mut bytes, 20, 1);
            put_u64(&mut bytes, 32, ELF_HEADER_SIZE as u64);
            put_u32(&mut bytes, 48, 0);
            put_u16(&mut bytes, 52, ELF_HEADER_SIZE as u16);
            put_u16(&mut bytes, 54, PROGRAM_HEADER_SIZE as u16);
            put_u16(&mut bytes, 56, phnum as u16);

            write_ph(
                &mut bytes,
                0,
                PT_LOAD,
                PF_R,
                0,
                BASE_VADDR,
                FILE_SIZE as u64,
                FILE_SIZE as u64,
            );
            let mut index = 1;
            if let Some(interp) = &self.interp {
                bytes[INTERP_OFFSET..INTERP_OFFSET + interp.len()].copy_from_slice(interp);
                write_ph(
                    &mut bytes,
                    index,
                    PT_INTERP,
                    PF_R,
                    INTERP_OFFSET as u64,
                    BASE_VADDR + INTERP_OFFSET as u64,
                    interp.len() as u64,
                    interp.len() as u64,
                );
                index += 1;
            }
            if self.dynamic {
                for (entry, (tag, value)) in tags.iter().enumerate() {
                    let offset = DYNAMIC_OFFSET + entry * 16;
                    put_u64(&mut bytes, offset, *tag);
                    put_u64(&mut bytes, offset + 8, *value);
                }
                let dynamic_size = (tags.len() * 16) as u64;
                write_ph(
                    &mut bytes,
                    index,
                    PT_DYNAMIC,
                    PF_R,
                    DYNAMIC_OFFSET as u64,
                    BASE_VADDR + DYNAMIC_OFFSET as u64,
                    dynamic_size,
                    dynamic_size,
                );
                bytes[STRTAB_OFFSET..STRTAB_OFFSET + strings.len()].copy_from_slice(&strings);
            }
            bytes
        }
    }

    fn nul(bytes: &[u8]) -> Vec<u8> {
        let mut value = bytes.to_vec();
        value.push(0);
        value
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

    fn get_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn get_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn get_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_ph(
        bytes: &mut [u8],
        index: usize,
        kind: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        filesz: u64,
        memsz: u64,
    ) {
        let ph = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
        put_u32(bytes, ph, kind);
        put_u32(bytes, ph + 4, flags);
        put_u64(bytes, ph + 8, offset);
        put_u64(bytes, ph + 16, vaddr);
        put_u64(bytes, ph + 32, filesz);
        put_u64(bytes, ph + 40, memsz);
        put_u64(bytes, ph + 48, 8);
    }

    fn ph_offset(bytes: &[u8], kind: u32, occurrence: usize) -> usize {
        let count = usize::from(get_u16(bytes, 56));
        (0..count)
            .filter_map(|index| {
                let offset = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
                (get_u32(bytes, offset) == kind).then_some(offset)
            })
            .nth(occurrence)
            .unwrap()
    }

    fn append_ph(bytes: &mut [u8], source: usize) -> usize {
        let count = usize::from(get_u16(bytes, 56));
        let destination = ELF_HEADER_SIZE + count * PROGRAM_HEADER_SIZE;
        bytes.copy_within(source..source + PROGRAM_HEADER_SIZE, destination);
        put_u16(bytes, 56, (count + 1) as u16);
        destination
    }

    fn append_partial_load(bytes: &mut [u8], offset: u64, vaddr: u64, size: u64) {
        let load = ph_offset(bytes, PT_LOAD, 0);
        let partial = append_ph(bytes, load);
        put_u64(bytes, partial + 8, offset + size - 1);
        put_u64(bytes, partial + 16, vaddr + size - 1);
        put_u64(bytes, partial + 32, 2);
        put_u64(bytes, partial + 40, 2);
    }

    fn dynamic_entry(bytes: &[u8], tag: u64, occurrence: usize) -> usize {
        let ph = ph_offset(bytes, PT_DYNAMIC, 0);
        let start = get_u64(bytes, ph + 8) as usize;
        let size = get_u64(bytes, ph + 32) as usize;
        (start..start + size)
            .step_by(16)
            .filter(|offset| get_u64(bytes, *offset) == tag)
            .nth(occurrence)
            .unwrap()
    }

    fn assert_primary_rejected(bytes: &[u8]) {
        let image = Image::new();
        let primary = image.primary(bytes);
        assert!(image.inspect(&primary).is_err());
    }

    fn assert_dynamic_primary_rejected(bytes: &[u8]) {
        let image = Image::new();
        let primary = image.primary(bytes);
        image.loader(&Fixture::loader().build());
        image.runtime_dir();
        assert!(image.inspect(&primary).is_err());
    }

    fn inspect_with_library(
        primary_needed: &[&str],
        name: &str,
        library: &[u8],
    ) -> crate::Result<super::ElfInspection> {
        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(primary_needed).build());
        image.loader(&Fixture::loader().build());
        image.library(name, library);
        image.inspect(&primary)
    }

    #[test]
    fn valid_static_metadata_and_launch_contract() {
        use sha2::{Digest, Sha256};

        let image = Image::new();
        let bytes = Fixture::static_primary().build();
        let primary = image.primary(&bytes);
        let result = image.inspect(&primary).unwrap();

        assert_eq!(result.machine, "x86_64");
        assert_eq!(result.interpreter, None);
        assert!(result.needed.is_empty());
        assert_eq!(result.binary_size, bytes.len() as u64);
        assert_eq!(
            result.binary_sha256,
            format!("sha256:{:x}", Sha256::digest(&bytes))
        );
        assert_eq!(result.claim, ClosureClaim::StartupOnly);
        assert_eq!(result.visited.len(), 1);
        assert_eq!(result.visited[0].role, ObjectRole::Primary);

        let command = direct_launch(None, &["validate".into(), "--strict".into()]).unwrap();
        assert_eq!(command.program, "/usr/bin/env");
        assert_eq!(command.args, ["/work/bin/app-cli", "validate", "--strict"]);
    }

    #[test]
    fn valid_dynamic_closure_preserves_sorted_direct_needed_and_cycles() {
        let image = Image::new();
        let primary =
            image.primary(&Fixture::dynamic_primary(&["libz.so", "liba.so", "libz.so"]).build());
        image.loader(&Fixture::loader().build());
        let mut liba = Fixture::library(&["libz.so"]);
        liba.soname = Some(b"liba.so".to_vec());
        image.library("liba.so", &liba.build());
        let mut libz = Fixture::library(&["liba.so"]);
        libz.interp = Some(nul(INTERPRETER.as_bytes()));
        image.library("libz.so", &libz.build());

        let result = image.inspect(&primary).unwrap();
        assert_eq!(result.interpreter.as_deref(), Some(INTERPRETER));
        assert_eq!(result.needed, ["liba.so", "libz.so", "libz.so"]);
        assert_eq!(result.visited.len(), 4);
        assert_eq!(
            result
                .visited
                .iter()
                .filter(|object| object.role == ObjectRole::Interpreter)
                .count(),
            1
        );
        assert_eq!(
            result
                .visited
                .iter()
                .filter(|object| object.role == ObjectRole::Library)
                .count(),
            2
        );
        assert!(result.visited.iter().all(|object| object.device != 0));
        assert!(result.visited.iter().all(|object| object.inode != 0));

        let command = direct_launch(result.interpreter.as_deref(), &["package".into()]).unwrap();
        assert_eq!(command.program, "/usr/bin/env");
        assert_eq!(
            command.args,
            [
                INTERPRETER,
                "--inhibit-cache",
                "--glibc-hwcaps-mask",
                "",
                "--library-path",
                "/opt/edgezero/runtime-lib",
                "/work/bin/app-cli",
                "package",
            ]
        );
    }

    #[test]
    fn interpreter_dependencies_are_part_of_the_recursive_startup_closure() {
        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        image.loader(&Fixture::library(&["libloader-dep.so"]).build());
        image.library("libloader-dep.so", &Fixture::library(&[]).build());
        let result = image.inspect(&primary).unwrap();

        assert!(result.needed.is_empty());
        assert!(result.visited.iter().any(|object| {
            object.role == ObjectRole::Library
                && object.image_path == "/opt/edgezero/runtime-lib/libloader-dep.so"
        }));
    }

    #[test]
    fn loader_alias_resolves_to_the_validated_interpreter() {
        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[LOADER_NAME]).build());
        let mut loader = Fixture::loader();
        loader.soname = Some(LOADER_NAME.as_bytes().to_vec());
        image.loader(&loader.build());
        image.runtime_dir();

        let result = image.inspect(&primary).unwrap();
        assert_eq!(result.needed, [LOADER_NAME]);
        assert_eq!(result.visited.len(), 2);
        assert_eq!(result.visited[1].role, ObjectRole::Interpreter);
    }

    #[test]
    fn header_profile_is_exact() {
        for (offset, value) in [(4, 1), (5, 2), (6, 0), (7, 1), (8, 1), (9, 1)] {
            let mut bytes = Fixture::static_primary().build();
            bytes[offset] = value;
            assert_primary_rejected(&bytes);
        }
        for (offset, value) in [(16, 1), (18, 3), (52, 63), (54, 55), (56, 0), (56, 0xffff)] {
            let mut bytes = Fixture::static_primary().build();
            put_u16(&mut bytes, offset, value);
            assert_primary_rejected(&bytes);
        }
        for (offset, value) in [(20, 0), (48, 1)] {
            let mut bytes = Fixture::static_primary().build();
            put_u32(&mut bytes, offset, value);
            assert_primary_rejected(&bytes);
        }

        for osabi in [0, 3] {
            let image = Image::new();
            let mut fixture = Fixture::static_primary();
            fixture.osabi = osabi;
            let primary = image.primary(&fixture.build());
            image.inspect(&primary).unwrap();
        }
        let mut pie_static = Fixture::static_primary();
        pie_static.elf_type = ET_DYN;
        let image = Image::new();
        let primary = image.primary(&pie_static.build());
        image.inspect(&primary).unwrap();
    }

    #[test]
    fn magic_and_every_other_osabi_are_rejected() {
        let mut bad_magic = Fixture::static_primary().build();
        bad_magic[0] = 0;
        assert_primary_rejected(&bad_magic);

        for osabi in 0u8..=u8::MAX {
            let mut fixture = Fixture::static_primary();
            fixture.osabi = osabi;
            let bytes = fixture.build();
            let result = super::parse_elf(
                &mut Cursor::new(bytes.clone()),
                bytes.len() as u64,
                ObjectRole::Primary,
                "app-cli",
            );
            assert_eq!(result.is_ok(), matches!(osabi, 0 | 3), "OSABI {osabi}");
        }
    }

    #[test]
    fn checked_header_segment_and_virtual_ranges_reject_overflow() {
        for phoff in [FILE_SIZE as u64 - 8, u64::MAX - 8] {
            let mut bytes = Fixture::static_primary().build();
            put_u64(&mut bytes, 32, phoff);
            assert_primary_rejected(&bytes);
        }

        let mut segment_offset_overflow = Fixture::static_primary().build();
        let load = ph_offset(&segment_offset_overflow, PT_LOAD, 0);
        put_u64(&mut segment_offset_overflow, load + 8, u64::MAX - 1);
        put_u64(&mut segment_offset_overflow, load + 32, 2);
        assert_primary_rejected(&segment_offset_overflow);

        let mut segment_out_of_range = Fixture::static_primary().build();
        let load = ph_offset(&segment_out_of_range, PT_LOAD, 0);
        put_u64(&mut segment_out_of_range, load + 8, FILE_SIZE as u64 - 1);
        put_u64(&mut segment_out_of_range, load + 32, 2);
        assert_primary_rejected(&segment_out_of_range);

        let mut file_vaddr_overflow = Fixture::static_primary().build();
        let load = ph_offset(&file_vaddr_overflow, PT_LOAD, 0);
        put_u64(&mut file_vaddr_overflow, load + 16, u64::MAX - 1);
        put_u64(&mut file_vaddr_overflow, load + 32, 2);
        assert_primary_rejected(&file_vaddr_overflow);

        let mut memory_vaddr_overflow = Fixture::static_primary().build();
        let load = ph_offset(&memory_vaddr_overflow, PT_LOAD, 0);
        put_u64(&mut memory_vaddr_overflow, load + 16, u64::MAX - 1);
        put_u64(&mut memory_vaddr_overflow, load + 32, 1);
        put_u64(&mut memory_vaddr_overflow, load + 40, 2);
        assert_primary_rejected(&memory_vaddr_overflow);
    }

    #[test]
    fn section_header_fields_are_ignored() {
        let image = Image::new();
        let mut bytes = Fixture::static_primary().build();
        put_u64(&mut bytes, 40, u64::MAX);
        put_u16(&mut bytes, 58, u16::MAX);
        put_u16(&mut bytes, 60, u16::MAX);
        put_u16(&mut bytes, 62, u16::MAX);
        let primary = image.primary(&bytes);
        image.inspect(&primary).unwrap();
    }

    #[test]
    fn interpreter_profile_is_exact_and_well_formed() {
        let cases = [
            nul(b"/lib64/other-loader.so"),
            INTERPRETER.as_bytes().to_vec(),
            b"/lib64/ld-linux\0-x86-64.so.2\0".to_vec(),
            {
                let mut value = nul(INTERPRETER.as_bytes());
                value.push(0);
                value
            },
        ];
        for interp in cases {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.interp = Some(interp);
            assert_dynamic_primary_rejected(&fixture.build());
        }

        let mut no_interp = Fixture::dynamic_primary(&["liba.so"]);
        no_interp.interp = None;
        assert_dynamic_primary_rejected(&no_interp.build());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        let mut loader = Fixture::loader();
        loader.interp = Some(nul(INTERPRETER.as_bytes()));
        image.loader(&loader.build());
        image.runtime_dir();
        assert!(image.inspect(&primary).is_err());
    }

    #[test]
    fn duplicate_and_malformed_interp_segments_are_rejected() {
        let mut duplicate = Fixture::dynamic_primary(&[]).build();
        let interp = ph_offset(&duplicate, PT_INTERP, 0);
        append_ph(&mut duplicate, interp);
        assert_dynamic_primary_rejected(&duplicate);

        let mut unequal = Fixture::dynamic_primary(&[]).build();
        let interp = ph_offset(&unequal, PT_INTERP, 0);
        let filesz = get_u64(&unequal, interp + 32);
        put_u64(&mut unequal, interp + 40, filesz + 1);
        assert_dynamic_primary_rejected(&unequal);

        let mut contradictory = Fixture::dynamic_primary(&[]).build();
        let interp = ph_offset(&contradictory, PT_INTERP, 0);
        let offset = get_u64(&contradictory, interp + 8);
        put_u64(&mut contradictory, interp + 8, offset + 1);
        assert_dynamic_primary_rejected(&contradictory);

        let mut nonreadable = Fixture::dynamic_primary(&[]).build();
        let load = ph_offset(&nonreadable, PT_LOAD, 0);
        put_u32(&mut nonreadable, load + 4, 0);
        assert_dynamic_primary_rejected(&nonreadable);
    }

    #[test]
    fn static_and_dynamic_segment_rules_are_enforced() {
        let mut static_with_dynamic = Fixture::static_primary();
        static_with_dynamic.dynamic = true;
        assert_primary_rejected(&static_with_dynamic.build());

        let mut dynamic_without_table = Fixture::dynamic_primary(&[]);
        dynamic_without_table.dynamic = false;
        assert_dynamic_primary_rejected(&dynamic_without_table.build());

        let mut duplicate = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&duplicate, PT_DYNAMIC, 0);
        append_ph(&mut duplicate, dynamic);
        assert_dynamic_primary_rejected(&duplicate);

        let mut unequal = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&unequal, PT_DYNAMIC, 0);
        let file_size = get_u64(&unequal, dynamic + 32);
        put_u64(&mut unequal, dynamic + 40, file_size + 16);
        assert_dynamic_primary_rejected(&unequal);

        let mut empty = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&empty, PT_DYNAMIC, 0);
        put_u64(&mut empty, dynamic + 32, 0);
        put_u64(&mut empty, dynamic + 40, 0);
        assert_dynamic_primary_rejected(&empty);

        let mut malformed_width = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&malformed_width, PT_DYNAMIC, 0);
        put_u64(&mut malformed_width, dynamic + 32, 15);
        put_u64(&mut malformed_width, dynamic + 40, 15);
        assert_dynamic_primary_rejected(&malformed_width);
    }

    #[test]
    fn libraries_and_interpreter_require_exactly_one_dynamic_segment() {
        let mut library_without_dynamic = Fixture::library(&[]);
        library_without_dynamic.dynamic = false;
        assert!(
            inspect_with_library(&["liba.so"], "liba.so", &library_without_dynamic.build())
                .is_err()
        );

        let mut duplicate_library = Fixture::library(&[]).build();
        let dynamic = ph_offset(&duplicate_library, PT_DYNAMIC, 0);
        append_ph(&mut duplicate_library, dynamic);
        assert!(inspect_with_library(&["liba.so"], "liba.so", &duplicate_library).is_err());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        let mut loader_without_dynamic = Fixture::loader();
        loader_without_dynamic.dynamic = false;
        image.loader(&loader_without_dynamic.build());
        image.runtime_dir();
        assert!(image.inspect(&primary).is_err());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        let mut duplicate_loader = Fixture::loader().build();
        let dynamic = ph_offset(&duplicate_loader, PT_DYNAMIC, 0);
        append_ph(&mut duplicate_loader, dynamic);
        image.loader(&duplicate_loader);
        image.runtime_dir();
        assert!(image.inspect(&primary).is_err());
    }

    #[test]
    fn every_load_segment_requires_file_size_not_exceeding_memory_size() {
        let mut bytes = Fixture::static_primary().build();
        let load = ph_offset(&bytes, PT_LOAD, 0);
        let filesz = get_u64(&bytes, load + 32);
        put_u64(&mut bytes, load + 40, filesz - 1);
        assert_primary_rejected(&bytes);
    }

    #[test]
    fn dynamic_mapping_must_be_unique_readable_file_backed_and_consistent() {
        let mut contradictory = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&contradictory, PT_DYNAMIC, 0);
        let virtual_address = get_u64(&contradictory, dynamic + 16);
        put_u64(&mut contradictory, dynamic + 16, virtual_address + 16);
        assert_dynamic_primary_rejected(&contradictory);

        let mut nonreadable = Fixture::dynamic_primary(&[]).build();
        let load = ph_offset(&nonreadable, PT_LOAD, 0);
        put_u32(&mut nonreadable, load + 4, 0);
        assert_dynamic_primary_rejected(&nonreadable);

        let mut non_file_backed = Fixture::dynamic_primary(&[]).build();
        let load = ph_offset(&non_file_backed, PT_LOAD, 0);
        put_u64(&mut non_file_backed, load + 32, DYNAMIC_OFFSET as u64);
        assert_dynamic_primary_rejected(&non_file_backed);

        let mut ambiguous = Fixture::dynamic_primary(&[]).build();
        let load = ph_offset(&ambiguous, PT_LOAD, 0);
        append_ph(&mut ambiguous, load);
        assert_dynamic_primary_rejected(&ambiguous);

        let mut unmapped = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&unmapped, PT_DYNAMIC, 0);
        put_u64(
            &mut unmapped,
            dynamic + 16,
            BASE_VADDR + FILE_SIZE as u64 + 16,
        );
        assert_dynamic_primary_rejected(&unmapped);
    }

    #[test]
    fn partial_load_overlap_is_ambiguous_for_every_loader_visible_range() {
        let mut dynamic = Fixture::dynamic_primary(&[]).build();
        let ph = ph_offset(&dynamic, PT_DYNAMIC, 0);
        let offset = get_u64(&dynamic, ph + 8);
        let vaddr = get_u64(&dynamic, ph + 16);
        let size = get_u64(&dynamic, ph + 32);
        append_partial_load(&mut dynamic, offset, vaddr, size);
        assert_dynamic_primary_rejected(&dynamic);

        let mut interp = Fixture::dynamic_primary(&[]).build();
        let ph = ph_offset(&interp, PT_INTERP, 0);
        let offset = get_u64(&interp, ph + 8);
        let vaddr = get_u64(&interp, ph + 16);
        let size = get_u64(&interp, ph + 32);
        append_partial_load(&mut interp, offset, vaddr, size);
        assert_dynamic_primary_rejected(&interp);

        let mut strings = Fixture::dynamic_primary(&["liba.so"]).build();
        let strsz = get_u64(&strings, dynamic_entry(&strings, DT_STRSZ, 0) + 8);
        append_partial_load(
            &mut strings,
            STRTAB_OFFSET as u64,
            BASE_VADDR + STRTAB_OFFSET as u64,
            strsz,
        );
        assert_dynamic_primary_rejected(&strings);
    }

    #[test]
    fn dynamic_termination_is_exact() {
        let mut missing_null = Fixture::dynamic_primary(&[]).build();
        put_u64(&mut missing_null, DYNAMIC_OFFSET, 2);
        assert_dynamic_primary_rejected(&missing_null);

        let mut trailing_nonzero = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&trailing_nonzero, PT_DYNAMIC, 0);
        put_u64(&mut trailing_nonzero, dynamic + 32, 32);
        put_u64(&mut trailing_nonzero, dynamic + 40, 32);
        trailing_nonzero[DYNAMIC_OFFSET + 16] = 1;
        assert_dynamic_primary_rejected(&trailing_nonzero);

        let mut trailing_zero = Fixture::dynamic_primary(&[]).build();
        let dynamic = ph_offset(&trailing_zero, PT_DYNAMIC, 0);
        put_u64(&mut trailing_zero, dynamic + 32, 32);
        put_u64(&mut trailing_zero, dynamic + 40, 32);
        let image = Image::new();
        let primary = image.primary(&trailing_zero);
        image.loader(&Fixture::loader().build());
        image.runtime_dir();
        image.inspect(&primary).unwrap();

        let mut nonzero_null = Fixture::dynamic_primary(&[]).build();
        put_u64(&mut nonzero_null, DYNAMIC_OFFSET + 8, 1);
        assert_dynamic_primary_rejected(&nonzero_null);
    }

    #[test]
    fn string_table_mapping_and_strings_are_bounded_and_unambiguous() {
        let base = Fixture::dynamic_primary(&["liba.so"]).build();

        let mut duplicate_strtab = base.clone();
        let null = dynamic_entry(&duplicate_strtab, DT_NULL, 0);
        put_u64(&mut duplicate_strtab, null, DT_STRTAB);
        put_u64(
            &mut duplicate_strtab,
            null + 8,
            BASE_VADDR + STRTAB_OFFSET as u64 + 1,
        );
        let dynamic = ph_offset(&duplicate_strtab, PT_DYNAMIC, 0);
        let size = get_u64(&duplicate_strtab, dynamic + 32) + 16;
        put_u64(&mut duplicate_strtab, dynamic + 32, size);
        put_u64(&mut duplicate_strtab, dynamic + 40, size);
        assert_dynamic_primary_rejected(&duplicate_strtab);

        let mut duplicate_strsz = base.clone();
        let null = dynamic_entry(&duplicate_strsz, DT_NULL, 0);
        put_u64(&mut duplicate_strsz, null, DT_STRSZ);
        put_u64(&mut duplicate_strsz, null + 8, 1);
        let dynamic = ph_offset(&duplicate_strsz, PT_DYNAMIC, 0);
        let size = get_u64(&duplicate_strsz, dynamic + 32) + 16;
        put_u64(&mut duplicate_strsz, dynamic + 32, size);
        put_u64(&mut duplicate_strsz, dynamic + 40, size);
        assert_dynamic_primary_rejected(&duplicate_strsz);

        let mut unmapped = base.clone();
        let strtab = dynamic_entry(&unmapped, DT_STRTAB, 0);
        put_u64(&mut unmapped, strtab + 8, BASE_VADDR + FILE_SIZE as u64);
        assert_dynamic_primary_rejected(&unmapped);

        let mut ambiguous = base.clone();
        let load = ph_offset(&ambiguous, PT_LOAD, 0);
        let second = append_ph(&mut ambiguous, load);
        put_u64(&mut ambiguous, second + 8, STRTAB_OFFSET as u64);
        put_u64(
            &mut ambiguous,
            second + 16,
            BASE_VADDR + STRTAB_OFFSET as u64,
        );
        put_u64(&mut ambiguous, second + 32, 64);
        put_u64(&mut ambiguous, second + 40, 64);
        assert_dynamic_primary_rejected(&ambiguous);

        let mut unterminated = base.clone();
        let string_size =
            get_u64(&unterminated, dynamic_entry(&unterminated, DT_STRSZ, 0) + 8) as usize;
        unterminated[STRTAB_OFFSET + string_size - 1] = b'x';
        assert_dynamic_primary_rejected(&unterminated);

        for value in [b"bad\x01name".as_slice(), b"bad\xffname".as_slice()] {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.needed = vec![value.to_vec()];
            assert_dynamic_primary_rejected(&fixture.build());
        }

        let mut strtab_only = Fixture::dynamic_primary(&[]);
        strtab_only
            .extra_tags
            .push((DT_STRTAB, BASE_VADDR + STRTAB_OFFSET as u64));
        assert_dynamic_primary_rejected(&strtab_only.build());

        let mut strsz_only = Fixture::dynamic_primary(&[]);
        strsz_only.extra_tags.push((DT_STRSZ, 1));
        assert_dynamic_primary_rejected(&strsz_only.build());

        let mut zero_strsz = base.clone();
        let entry = dynamic_entry(&zero_strsz, DT_STRSZ, 0);
        put_u64(&mut zero_strsz, entry + 8, 0);
        assert_dynamic_primary_rejected(&zero_strsz);

        let mut out_of_range = base;
        let size = get_u64(&out_of_range, dynamic_entry(&out_of_range, DT_STRSZ, 0) + 8);
        let needed = dynamic_entry(&out_of_range, DT_NEEDED, 0);
        put_u64(&mut out_of_range, needed + 8, size);
        assert_dynamic_primary_rejected(&out_of_range);
    }

    #[test]
    fn soname_roles_and_aliases_are_exact() {
        let mut primary = Fixture::dynamic_primary(&[]);
        primary.soname = Some(b"app-cli".to_vec());
        assert_dynamic_primary_rejected(&primary.build());

        for soname in [
            Vec::new(),
            vec![b'a'; 256],
            b"dir/liba.so".to_vec(),
            b"dir\\liba.so".to_vec(),
            b"$ORIGIN".to_vec(),
            b"other.so".to_vec(),
        ] {
            let mut library = Fixture::library(&[]);
            library.soname = Some(soname);
            assert!(inspect_with_library(&["liba.so"], "liba.so", &library.build()).is_err());
        }

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        let mut loader = Fixture::loader();
        loader.soname = Some(b"other-loader.so".to_vec());
        image.loader(&loader.build());
        image.runtime_dir();
        assert!(image.inspect(&primary).is_err());

        let mut duplicate_soname = Fixture::library(&[]);
        duplicate_soname.soname = Some(b"liba.so".to_vec());
        duplicate_soname.extra_tags.push((DT_SONAME, 1));
        assert!(inspect_with_library(&["liba.so"], "liba.so", &duplicate_soname.build()).is_err());
    }

    #[test]
    fn cross_object_alias_collision_is_rejected_directly() {
        let object =
            |image_path: &str, role: ObjectRole, inode: u64, soname: Option<&str>| super::Object {
                image_path: image_path.into(),
                role,
                identity: super::Identity { device: 1, inode },
                parsed: super::ParsedElf {
                    interpreter: None,
                    needed: Vec::new(),
                    soname: soname.map(str::to_owned),
                },
            };
        let primary = object("/work/bin/app-cli", ObjectRole::Primary, 1, None);
        let loader = object(INTERPRETER, ObjectRole::Interpreter, 2, None);
        let colliding = object(
            "/opt/edgezero/runtime-lib/libalias.so",
            ObjectRole::Library,
            3,
            Some("app-cli"),
        );
        let libraries = std::collections::BTreeMap::from([("libalias.so".into(), colliding)]);

        assert!(super::validate_aliases(&primary, &loader, &libraries).is_err());
    }

    struct TrackingReader {
        inner: Cursor<Vec<u8>>,
        max_request: usize,
        total_read: usize,
        read_calls: usize,
        seek_calls: usize,
    }

    impl Read for TrackingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.max_request = self.max_request.max(buffer.len());
            self.read_calls += 1;
            let read = self.inner.read(buffer)?;
            self.total_read += read;
            Ok(read)
        }
    }

    impl Seek for TrackingReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.seek_calls += 1;
            self.inner.seek(position)
        }
    }

    #[test]
    fn parser_uses_bounded_reads_for_an_allowed_large_logical_file() {
        let mut reader = TrackingReader {
            inner: Cursor::new(Fixture::static_primary().build()),
            max_request: 0,
            total_read: 0,
            read_calls: 0,
            seek_calls: 0,
        };

        super::parse_elf(
            &mut reader,
            crate::json_contract::BINARY_LIMIT,
            ObjectRole::Primary,
            "app-cli",
        )
        .unwrap();

        assert!(reader.max_request <= ELF_HEADER_SIZE);
        assert_eq!(reader.total_read, ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE);
    }

    #[test]
    fn oversized_needed_table_is_rejected_before_tail_or_string_reads() {
        let max_needed_entries = METADATA_LIMIT / 3;
        assert_eq!(super::MAX_NEEDED_ENTRIES, max_needed_entries);
        let needed_entries = max_needed_entries + 1;
        let trailing_entries = 100_000;
        let dynamic_entries = 2 + needed_entries + 1 + trailing_entries;
        let dynamic_size = dynamic_entries * super::DYNAMIC_ENTRY_SIZE as usize;
        let strtab_offset = DYNAMIC_OFFSET + dynamic_size;
        let file_size = strtab_offset + 3;

        let mut bytes = Fixture::dynamic_primary(&[]).build();
        bytes.resize(file_size, 0);
        let load = ph_offset(&bytes, PT_LOAD, 0);
        put_u64(&mut bytes, load + 32, file_size as u64);
        put_u64(&mut bytes, load + 40, file_size as u64);
        let dynamic = ph_offset(&bytes, PT_DYNAMIC, 0);
        put_u64(&mut bytes, dynamic + 32, dynamic_size as u64);
        put_u64(&mut bytes, dynamic + 40, dynamic_size as u64);
        put_u64(&mut bytes, DYNAMIC_OFFSET, DT_STRTAB);
        put_u64(
            &mut bytes,
            DYNAMIC_OFFSET + 8,
            BASE_VADDR + strtab_offset as u64,
        );
        put_u64(&mut bytes, DYNAMIC_OFFSET + 16, DT_STRSZ);
        put_u64(&mut bytes, DYNAMIC_OFFSET + 24, 3);
        for index in 0..needed_entries {
            let offset = DYNAMIC_OFFSET + (index + 2) * super::DYNAMIC_ENTRY_SIZE as usize;
            put_u64(&mut bytes, offset, DT_NEEDED);
            put_u64(&mut bytes, offset + 8, 1);
        }
        bytes[strtab_offset..strtab_offset + 3].copy_from_slice(b"\0a\0");

        let mut reader = TrackingReader {
            inner: Cursor::new(bytes),
            max_request: 0,
            total_read: 0,
            read_calls: 0,
            seek_calls: 0,
        };
        let result = super::parse_elf(
            &mut reader,
            file_size as u64,
            ObjectRole::Primary,
            "app-cli",
        );

        assert_eq!(result.err().as_deref(), Some("too many DT_NEEDED entries"));
        let fixed_reads = 1 + 3 + 1;
        let scanned_dynamic_bytes = (2 + needed_entries) * super::DYNAMIC_ENTRY_SIZE as usize;
        let dynamic_reads = scanned_dynamic_bytes.div_ceil(super::IO_CHUNK_SIZE);
        assert!(reader.max_request <= super::IO_CHUNK_SIZE);
        assert!(reader.read_calls <= fixed_reads + dynamic_reads);
        assert!(reader.seek_calls <= fixed_reads + dynamic_reads);
        assert!(
            reader.total_read
                <= ELF_HEADER_SIZE
                    + 3 * PROGRAM_HEADER_SIZE
                    + INTERPRETER.len()
                    + 1
                    + dynamic_reads * super::IO_CHUNK_SIZE
        );
        assert!(reader.total_read < dynamic_size / 2);
    }

    #[test]
    fn forbidden_loader_acquisition_tags_are_always_rejected() {
        for tag in [
            DT_RPATH,
            DT_RUNPATH,
            DT_AUDIT,
            DT_DEPAUDIT,
            DT_CONFIG,
            DT_AUXILIARY,
            DT_FILTER,
            DT_POSFLAG_1,
        ] {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.push((tag, 0));
            assert_dynamic_primary_rejected(&fixture.build());
        }
    }

    #[test]
    fn dynamic_tag_vocabulary_is_closed_at_every_boundary() {
        let accepted = [
            2, 3, 4, 6, 7, 8, 9, 11, 12, 13, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            30, 32, 33, 34, 35, 36, 37, 0x6ffffef5, 0x6ffffef6, 0x6ffffef7, 0x6ffffff0, 0x6ffffff9,
            0x6ffffffa, 0x6ffffffb, 0x6ffffffc, 0x6ffffffd, 0x6ffffffe, 0x6fffffff, 0x70000000,
            0x70000001, 0x70000003,
        ];
        for tag in accepted {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.push((tag, 0));
            let image = Image::new();
            let primary = image.primary(&fixture.build());
            image.loader(&Fixture::loader().build());
            image.runtime_dir();
            image
                .inspect(&primary)
                .unwrap_or_else(|error| panic!("accepted tag {tag:#x}: {error}"));
        }

        for tag in [
            31,
            38,
            0x60000000,
            0x6ffffef4,
            0x6ffffef8,
            0x6fffffef,
            0x6ffffff1,
            0x6ffffff8,
            0x70000002,
            0x70000004,
            0x7ffffffc,
            0x80000000,
            u64::MAX,
        ] {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.push((tag, 0));
            assert_dynamic_primary_rejected(&fixture.build());
        }
    }

    #[test]
    fn flags_masks_are_exact() {
        for value in [0, 0x0000001e] {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.push((DT_FLAGS, value));
            let image = Image::new();
            let primary = image.primary(&fixture.build());
            image.loader(&Fixture::loader().build());
            image.runtime_dir();
            image.inspect(&primary).unwrap();
        }
        for value in [1, 0x1f, u64::from(u32::MAX)] {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.push((DT_FLAGS, value));
            assert_dynamic_primary_rejected(&fixture.build());
        }

        for value in [0, 0x5eff976f] {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.push((DT_FLAGS_1, value));
            let image = Image::new();
            let primary = image.primary(&fixture.build());
            image.loader(&Fixture::loader().build());
            image.runtime_dir();
            image.inspect(&primary).unwrap();
        }
        for value in [
            0x10,
            0x80,
            0x800,
            0x2000,
            0x4000,
            0x1000000,
            0x20000000,
            1u64 << 32,
        ] {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.push((DT_FLAGS_1, value));
            assert_dynamic_primary_rejected(&fixture.build());
        }
    }

    #[test]
    fn only_needed_may_repeat() {
        let singleton_tags = [
            2, 3, 4, 6, 7, 8, 9, 11, 12, 13, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            30, 32, 33, 34, 35, 36, 37, 0x6ffffef5, 0x6ffffef6, 0x6ffffef7, 0x6ffffff0, 0x6ffffff9,
            0x6ffffffa, 0x6ffffffb, 0x6ffffffc, 0x6ffffffd, 0x6ffffffe, 0x6fffffff, 0x70000000,
            0x70000001, 0x70000003,
        ];
        for tag in singleton_tags {
            let mut fixture = Fixture::dynamic_primary(&[]);
            fixture.extra_tags.extend([(tag, 0), (tag, 0)]);
            assert_dynamic_primary_rejected(&fixture.build());
        }

        for tag in [DT_STRTAB, DT_STRSZ] {
            let mut fixture = Fixture::dynamic_primary(&["liba.so"]);
            fixture.extra_tags.push((tag, 0));
            assert_dynamic_primary_rejected(&fixture.build());
        }

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&["liba.so", "liba.so"]).build());
        image.loader(&Fixture::loader().build());
        image.library("liba.so", &Fixture::library(&[]).build());
        assert_eq!(image.inspect(&primary).unwrap().needed.len(), 2);
    }

    #[test]
    fn dependency_names_reject_paths_expansion_and_empty_values() {
        let names = [
            "",
            "dir/liba.so",
            "dir\\liba.so",
            "$ORIGIN/liba.so",
            "${ORIGIN}/liba.so",
            "$LIB/liba.so",
            "${LIB}/liba.so",
            "$PLATFORM/liba.so",
            "${PLATFORM}/liba.so",
            "lib$dollar.so",
        ];
        for name in names {
            let fixture = Fixture::dynamic_primary(&[name]);
            assert_dynamic_primary_rejected(&fixture.build());
        }

        let oversized = "a".repeat(256);
        let fixture = Fixture::dynamic_primary(&[&oversized]);
        assert_dynamic_primary_rejected(&fixture.build());
    }

    #[test]
    fn missing_direct_transitive_and_mixed_architecture_fail() {
        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&["missing.so"]).build());
        image.loader(&Fixture::loader().build());
        image.runtime_dir();
        assert!(image.inspect(&primary).is_err());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&["liba.so"]).build());
        image.loader(&Fixture::loader().build());
        image.library("liba.so", &Fixture::library(&["missing.so"]).build());
        assert!(image.inspect(&primary).is_err());

        let mut wrong_arch = Fixture::library(&[]).build();
        put_u16(&mut wrong_arch, 18, 183);
        assert!(inspect_with_library(&["liba.so"], "liba.so", &wrong_arch).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_directory_rejects_symlinks_hardlinks_subdirectories_and_reserved_names() {
        use std::os::unix::fs::symlink;

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&["liba.so"]).build());
        image.loader(&Fixture::loader().build());
        let outside = image.write("/outside.so", &Fixture::library(&[]).build());
        let runtime = image.runtime_dir();
        symlink(&outside, runtime.join("liba.so")).unwrap();
        assert!(image.inspect(&primary).is_err());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&["liba.so"]).build());
        image.loader(&Fixture::loader().build());
        let runtime = image.runtime_dir();
        symlink("does-not-exist", runtime.join("liba.so")).unwrap();
        assert!(image.inspect(&primary).is_err());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&["liba.so"]).build());
        image.loader(&Fixture::loader().build());
        let library = image.library("liba.so", &Fixture::library(&[]).build());
        fs::hard_link(&library, image.root().join("second-link.so")).unwrap();
        assert!(image.inspect(&primary).is_err());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        image.loader(&Fixture::loader().build());
        image.library("nested/libdup.so", &Fixture::library(&[]).build());
        image.library("other/libdup.so", &Fixture::library(&[]).build());
        assert!(image.inspect(&primary).is_err());

        for reserved in ["app-cli", LOADER_NAME] {
            let image = Image::new();
            let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
            image.loader(&Fixture::loader().build());
            image.library(reserved, &Fixture::library(&[]).build());
            assert!(image.inspect(&primary).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn runtime_directory_rejects_non_regular_entries_and_interpreter_hardlinks() {
        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        image.loader(&Fixture::loader().build());
        let runtime = image.runtime_dir();
        fs::create_dir(runtime.join("directory.so")).unwrap();
        assert!(image.inspect(&primary).is_err());

        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
        let loader = image.loader(&Fixture::loader().build());
        fs::hard_link(&loader, image.root().join("loader-link")).unwrap();
        image.runtime_dir();
        assert!(image.inspect(&primary).is_err());
    }

    #[test]
    fn resolver_uses_only_runtime_lib_and_ignores_cache_defaults_and_hwcaps() {
        let image = Image::new();
        let primary = image.primary(&Fixture::dynamic_primary(&["libchoice.so"]).build());
        image.loader(&Fixture::loader().build());
        image.library("libchoice.so", &Fixture::library(&[]).build());
        image.write("/etc/ld.so.cache", b"synthetic cache-only entry");

        let mut alternate = Fixture::library(&[]).build();
        put_u16(&mut alternate, 18, 183);
        image.write("/lib/libchoice.so", &alternate);
        image.write("/usr/lib/libchoice.so", &alternate);
        image.write(
            "/lib/x86_64-linux-gnu/glibc-hwcaps/x86-64-v3/libchoice.so",
            &alternate,
        );

        let result = image.inspect(&primary).unwrap();
        let library = result
            .visited
            .iter()
            .find(|object| object.role == ObjectRole::Library)
            .unwrap();
        assert_eq!(library.image_path, "/opt/edgezero/runtime-lib/libchoice.so");
        assert_eq!(result.visited.len(), 3);

        let cache_only = Image::new();
        let primary = cache_only.primary(&Fixture::dynamic_primary(&["libcache.so"]).build());
        cache_only.loader(&Fixture::loader().build());
        cache_only.runtime_dir();
        cache_only.write("/etc/ld.so.cache", b"synthetic cache-only entry");
        cache_only.write("/usr/lib/libcache.so", &Fixture::library(&[]).build());
        assert!(cache_only.inspect(&primary).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn preload_presence_in_any_form_rejects_static_and_dynamic() {
        use std::os::unix::fs::symlink;

        for form in ["file", "directory", "symlink"] {
            let image = Image::new();
            let primary = image.primary(&Fixture::dynamic_primary(&[]).build());
            image.loader(&Fixture::loader().build());
            image.runtime_dir();
            let preload = image.root().join("etc/ld.so.preload");
            fs::create_dir_all(preload.parent().unwrap()).unwrap();
            match form {
                "file" => fs::write(&preload, b"/tmp/preload.so\n").unwrap(),
                "directory" => fs::create_dir(&preload).unwrap(),
                "symlink" => symlink("missing-preload", &preload).unwrap(),
                _ => unreachable!(),
            }
            assert!(image.inspect(&primary).is_err(), "preload form: {form}");
        }

        let image = Image::new();
        let primary = image.primary(&Fixture::static_primary().build());
        image.write("/etc/ld.so.preload", b"/tmp/preload.so\n");
        assert!(image.inspect(&primary).is_err());
    }

    #[test]
    fn dlopen_objects_are_explicitly_outside_the_startup_claim() {
        let image = Image::new();
        let mut primary_bytes = Fixture::dynamic_primary(&[]).build();
        let marker = b"dlopen:libplugin.so\0";
        primary_bytes[0x700..0x700 + marker.len()].copy_from_slice(marker);
        let primary = image.primary(&primary_bytes);
        image.loader(&Fixture::loader().build());
        image.library("libplugin.so", &Fixture::library(&[]).build());

        let result = image.inspect(&primary).unwrap();
        assert_eq!(result.claim, ClosureClaim::StartupOnly);
        assert!(
            primary_bytes
                .windows(marker.len())
                .any(|bytes| bytes == marker)
        );
        assert!(!result.needed.iter().any(|needed| needed == "libplugin.so"));
        assert!(
            result
                .visited
                .iter()
                .all(|object| object.image_path != "/opt/edgezero/runtime-lib/libplugin.so")
        );
    }
}
