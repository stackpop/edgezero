use crate::Result;
use crate::json_contract::{BINARY_LIMIT, METADATA_LIMIT};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};

pub const BLOCK_SIZE: usize = 512;
const IO_CHUNK_SIZE: usize = 8 * 1024;
const METADATA_NAME: &[u8] = b"app-cli-meta.json";
const BINARY_NAME: &[u8] = b"app-cli-bin";
const METADATA_MODE: &[u8; 8] = b"0000644\0";
const BINARY_MODE: &[u8; 8] = b"0000755\0";
const ZERO_ID: &[u8; 8] = b"0000000\0";
const ZERO_MTIME: &[u8; 12] = b"00000000000\0";
const ZERO_BLOCK: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedArchive {
    pub metadata: Vec<u8>,
    pub binary_offset: u64,
    pub binary_size: u64,
}

pub fn encode<W: Write, M: Read, B: Read>(
    writer: &mut W,
    metadata: &mut M,
    metadata_size: u64,
    binary: &mut B,
    binary_size: u64,
) -> Result<()> {
    validate_sizes(metadata_size, binary_size)?;
    let metadata_header = header(METADATA_NAME, METADATA_MODE, metadata_size)?;
    let binary_header = header(BINARY_NAME, BINARY_MODE, binary_size)?;

    write_all(writer, &metadata_header)?;
    copy_complete(metadata, writer, metadata_size)?;
    write_padding(writer, metadata_size)?;
    write_all(writer, &binary_header)?;
    copy_complete(binary, writer, binary_size)?;
    write_padding(writer, binary_size)?;
    write_all(writer, &ZERO_BLOCK)?;
    write_all(writer, &ZERO_BLOCK)?;
    writer.flush().map_err(|error| error.to_string())
}

pub fn parse<R: Read + Seek>(reader: &mut R) -> Result<ParsedArchive> {
    require(
        checked_seek(reader, SeekFrom::Current(0))? == 0,
        "archive parsing must begin at offset zero",
    )?;

    let metadata_size = read_header(reader, METADATA_NAME, METADATA_MODE)?;
    require(
        (1..=METADATA_LIMIT as u64).contains(&metadata_size),
        "metadata payload is outside protocol bounds",
    )?;
    let metadata_len = usize::try_from(metadata_size).map_err(|_| "metadata size overflow")?;
    let mut metadata = vec![0; metadata_len];
    read_exact(reader, &mut metadata)?;
    read_zero_padding(reader, metadata_size)?;

    let binary_size = read_header(reader, BINARY_NAME, BINARY_MODE)?;
    validate_sizes(metadata_size, binary_size)?;
    let binary_offset = checked_seek(reader, SeekFrom::Current(0))?;
    let binary_end = binary_offset
        .checked_add(binary_size)
        .ok_or("binary offset overflow")?;
    checked_seek(reader, SeekFrom::Start(binary_end))?;
    read_zero_padding(reader, binary_size)?;

    let mut end = [0; BLOCK_SIZE * 2];
    read_exact(reader, &mut end)?;
    require(
        end.iter().all(|byte| *byte == 0),
        "invalid archive end blocks",
    )?;
    let mut trailing = [0];
    require(
        read_retry(reader, &mut trailing)? == 0,
        "archive has trailing data",
    )?;

    Ok(ParsedArchive {
        metadata,
        binary_offset,
        binary_size,
    })
}

pub(crate) fn checked_seek<R: Seek>(reader: &mut R, position: SeekFrom) -> Result<u64> {
    let expected = match position {
        SeekFrom::Start(expected) => Some(expected),
        SeekFrom::Current(0) => None,
        _ => return Err("unsupported seek operation".into()),
    };
    let actual = loop {
        match reader.seek(position) {
            Ok(actual) => break actual,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
    };
    if let Some(expected) = expected {
        require(
            actual == expected,
            &format!("seek returned unexpected position: expected {expected}, got {actual}"),
        )?;
    }
    Ok(actual)
}

fn validate_sizes(metadata_size: u64, binary_size: u64) -> Result<()> {
    require(
        (1..=METADATA_LIMIT as u64).contains(&metadata_size),
        "metadata payload is outside protocol bounds",
    )?;
    require(binary_size != 0, "binary payload is empty")?;
    let payload_size = metadata_size
        .checked_add(binary_size)
        .ok_or("payload size overflow")?;
    require(
        payload_size <= BINARY_LIMIT,
        "archive payloads exceed protocol bounds",
    )?;
    archive_size(metadata_size, binary_size)?;
    Ok(())
}

fn archive_size(metadata_size: u64, binary_size: u64) -> Result<u64> {
    let metadata_blocks = padded_size(metadata_size)?;
    let binary_blocks = padded_size(binary_size)?;
    (BLOCK_SIZE as u64)
        .checked_add(metadata_blocks)
        .and_then(|size| size.checked_add(BLOCK_SIZE as u64))
        .and_then(|size| size.checked_add(binary_blocks))
        .and_then(|size| size.checked_add((BLOCK_SIZE * 2) as u64))
        .ok_or_else(|| "archive size overflow".into())
}

fn padded_size(size: u64) -> Result<u64> {
    let mask = BLOCK_SIZE as u64 - 1;
    size.checked_add(mask)
        .map(|value| value & !mask)
        .ok_or_else(|| "payload padding overflow".into())
}

fn padding_size(size: u64) -> Result<usize> {
    usize::try_from(padded_size(size)? - size).map_err(|_| "padding size overflow".into())
}

fn header(name: &[u8], mode: &[u8; 8], size: u64) -> Result<[u8; BLOCK_SIZE]> {
    require(name.len() < 100, "archive member name is too long")?;
    let size_field = format!("{size:011o}\0");
    require(size_field.len() == 12, "archive member size does not fit")?;

    let mut header = [0; BLOCK_SIZE];
    header[..name.len()].copy_from_slice(name);
    header[100..108].copy_from_slice(mode);
    header[108..116].copy_from_slice(ZERO_ID);
    header[116..124].copy_from_slice(ZERO_ID);
    header[124..136].copy_from_slice(size_field.as_bytes());
    header[136..148].copy_from_slice(ZERO_MTIME);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[329..337].copy_from_slice(ZERO_ID);
    header[337..345].copy_from_slice(ZERO_ID);

    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    let checksum_field = format!("{checksum:06o}\0 ");
    require(checksum_field.len() == 8, "archive checksum does not fit")?;
    header[148..156].copy_from_slice(checksum_field.as_bytes());
    Ok(header)
}

fn read_header<R: Read>(reader: &mut R, name: &[u8], mode: &[u8; 8]) -> Result<u64> {
    let mut actual = [0; BLOCK_SIZE];
    read_exact(reader, &mut actual)?;
    let size = parse_octal(&actual[124..136])?;
    let expected = header(name, mode, size)?;
    require(actual == expected, "noncanonical ustar header")?;
    Ok(size)
}

fn parse_octal(field: &[u8]) -> Result<u64> {
    require(
        field.len() == 12 && field[11] == 0,
        "invalid octal field padding",
    )?;
    field[..11].iter().try_fold(0u64, |value, byte| {
        require((b'0'..=b'7').contains(byte), "invalid octal digit")?;
        value
            .checked_mul(8)
            .and_then(|value| value.checked_add(u64::from(*byte - b'0')))
            .ok_or_else(|| "octal field overflow".into())
    })
}

fn copy_complete<R: Read, W: Write>(reader: &mut R, writer: &mut W, size: u64) -> Result<()> {
    copy_exact(reader, writer, size)?;
    let mut extra = [0];
    require(
        read_retry(reader, &mut extra)? == 0,
        "payload input is longer than its declared size",
    )
}

pub(crate) fn copy_exact<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut remaining: u64,
) -> Result<()> {
    let mut buffer = [0; IO_CHUNK_SIZE];
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| "payload size overflow")?;
        let read = read_retry(reader, &mut buffer[..limit])?;
        require(read != 0, "payload input is shorter than its declared size")?;
        write_all(writer, &buffer[..read])?;
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or("payload size underflow")?;
    }
    Ok(())
}

pub(crate) fn verify_binary_sha256<R: Read + Seek>(
    reader: &mut R,
    parsed: &ParsedArchive,
    expected: &str,
) -> Result<()> {
    checked_seek(reader, SeekFrom::Start(parsed.binary_offset))?;
    let mut digest = Sha256::new();
    let mut remaining = parsed.binary_size;
    let mut buffer = [0; IO_CHUNK_SIZE];
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| "binary size overflow")?;
        let read = read_retry(reader, &mut buffer[..limit])?;
        require(read != 0, "staged binary is shorter than declared")?;
        digest.update(&buffer[..read]);
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or("binary size underflow")?;
    }
    require(
        format!("sha256:{:x}", digest.finalize()) == expected,
        "staged binary digest changed",
    )
}

fn write_padding<W: Write>(writer: &mut W, size: u64) -> Result<()> {
    write_all(writer, &ZERO_BLOCK[..padding_size(size)?])
}

fn read_zero_padding<R: Read>(reader: &mut R, size: u64) -> Result<()> {
    let mut padding = [0; BLOCK_SIZE];
    let length = padding_size(size)?;
    read_exact(reader, &mut padding[..length])?;
    require(
        padding[..length].iter().all(|byte| *byte == 0),
        "nonzero payload padding",
    )
}

fn read_exact<R: Read>(reader: &mut R, mut output: &mut [u8]) -> Result<()> {
    while !output.is_empty() {
        let limit = output.len().min(IO_CHUNK_SIZE);
        let read = read_retry(reader, &mut output[..limit])?;
        require(read != 0, "unexpected end of archive")?;
        output = &mut output[read..];
    }
    Ok(())
}

fn read_retry<R: Read>(reader: &mut R, output: &mut [u8]) -> Result<usize> {
    loop {
        match reader.read(output) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result.map_err(|error| error.to_string()),
        }
    }
}

fn write_all<W: Write>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    writer.write_all(bytes).map_err(|error| error.to_string())
}

fn require(valid: bool, reason: &str) -> Result<()> {
    if valid { Ok(()) } else { Err(reason.into()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{cleanup_owned_path, extract_binary};
    use std::{
        fs,
        io::{self, Cursor, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    const FIXTURE_ROOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker/build-app-cli/fixtures/provenance"
    );
    const METADATA: &[u8] =
        include_bytes!("../../../docker/build-app-cli/fixtures/provenance/valid/static-meta.json");
    const BINARY: &[u8] = include_bytes!(
        "../../../docker/build-app-cli/fixtures/provenance/valid/elf-static/app-cli"
    );
    const GOLDEN: &[u8] =
        include_bytes!("../../../docker/build-app-cli/fixtures/provenance/valid/archive.tar");

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "edgezero-provenance-archive-{}-{sequence}",
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
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn encoded(metadata: &[u8], binary: &[u8]) -> Result<Vec<u8>> {
        let mut archive = Vec::new();
        encode(
            &mut archive,
            &mut Cursor::new(metadata),
            metadata.len() as u64,
            &mut Cursor::new(binary),
            binary.len() as u64,
        )?;
        Ok(archive)
    }

    #[test]
    fn encoder_is_byte_exact_deterministic_and_normalized() {
        let first = encoded(METADATA, BINARY).unwrap();
        let second = encoded(METADATA, BINARY).unwrap();

        assert_eq!(first, second);
        assert_eq!(first, GOLDEN);
        assert_eq!(first.len(), 10 * BLOCK_SIZE);

        let metadata_padding = 512 + METADATA.len()..3 * BLOCK_SIZE;
        assert!(first[metadata_padding].iter().all(|byte| *byte == 0));
        let binary_padding = 4 * BLOCK_SIZE + BINARY.len()..8 * BLOCK_SIZE;
        assert!(first[binary_padding].iter().all(|byte| *byte == 0));
        assert_eq!(&first[8 * BLOCK_SIZE..], &[0; 2 * BLOCK_SIZE]);
    }

    #[test]
    fn golden_headers_have_every_protocol_field_exact() {
        assert_header(
            &GOLDEN[..BLOCK_SIZE],
            b"app-cli-meta.json",
            b"0000644\0",
            METADATA.len() as u64,
        );
        assert_header(
            &GOLDEN[3 * BLOCK_SIZE..4 * BLOCK_SIZE],
            b"app-cli-bin",
            b"0000755\0",
            BINARY.len() as u64,
        );
    }

    fn assert_header(header: &[u8], name: &[u8], mode: &[u8], size: u64) {
        let mut expected_name = [0; 100];
        expected_name[..name.len()].copy_from_slice(name);
        assert_eq!(&header[..100], &expected_name);
        assert_eq!(&header[100..108], mode);
        assert_eq!(&header[108..116], b"0000000\0");
        assert_eq!(&header[116..124], b"0000000\0");
        assert_eq!(&header[124..136], format!("{size:011o}\0").as_bytes());
        assert_eq!(&header[136..148], b"00000000000\0");
        assert!(header[148..154].iter().all(u8::is_ascii_digit));
        assert_eq!(&header[154..156], b"\0 ");
        assert_eq!(header[156], b'0');
        assert!(header[157..257].iter().all(|byte| *byte == 0));
        assert_eq!(&header[257..263], b"ustar\0");
        assert_eq!(&header[263..265], b"00");
        assert!(header[265..329].iter().all(|byte| *byte == 0));
        assert_eq!(&header[329..337], b"0000000\0");
        assert_eq!(&header[337..345], b"0000000\0");
        assert!(header[345..].iter().all(|byte| *byte == 0));

        let mut checksum_input = header.to_vec();
        checksum_input[148..156].fill(b' ');
        let checksum: u64 = checksum_input.iter().map(|byte| u64::from(*byte)).sum();
        assert_eq!(&header[148..156], format!("{checksum:06o}\0 ").as_bytes());
    }

    #[test]
    fn parser_accepts_the_golden_archive_without_loading_the_binary() {
        let mut archive = Cursor::new(GOLDEN);
        let parsed = parse(&mut archive).unwrap();

        assert_eq!(parsed.metadata, METADATA);
        assert_eq!(parsed.binary_offset, 4 * BLOCK_SIZE as u64);
        assert_eq!(parsed.binary_size, BINARY.len() as u64);
        assert_eq!(archive.stream_position().unwrap(), GOLDEN.len() as u64);
    }

    #[test]
    fn parser_rejects_every_malformed_fixture_category() {
        for name in [
            "archive-base256.tar",
            "archive-checksum.tar",
            "archive-duplicate.tar",
            "archive-embedded-nul.tar",
            "archive-end-block.tar",
            "archive-extra-end-block.tar",
            "archive-extra.tar",
            "archive-gnu.tar",
            "archive-header.tar",
            "archive-link.tar",
            "archive-octal-digit.tar",
            "archive-octal-padding.tar",
            "archive-order.tar",
            "archive-max-size.tar",
            "archive-padding.tar",
            "archive-pax.tar",
            "archive-size-limit.tar",
            "archive-size.tar",
            "archive-sparse.tar",
            "archive-special.tar",
            "archive-trailing.tar",
            "archive-traversal.tar",
        ] {
            let path = Path::new(FIXTURE_ROOT).join("invalid").join(name);
            let mut archive = Cursor::new(fs::read(path).unwrap());
            assert!(parse(&mut archive).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn size_bound_fixtures_fail_before_payload_io() {
        let size = fs::read(
            Path::new(FIXTURE_ROOT)
                .join("invalid")
                .join("archive-size.tar"),
        )
        .unwrap();
        assert_eq!(&size[124..136], b"00000000000\0");
        let mut size = RejectIoPast::new(size, BLOCK_SIZE as u64);
        assert_eq!(
            parse(&mut size).unwrap_err(),
            "metadata payload is outside protocol bounds"
        );
        assert!(!size.attempted_forbidden_io);

        let size_limit = fs::read(
            Path::new(FIXTURE_ROOT)
                .join("invalid")
                .join("archive-size-limit.tar"),
        )
        .unwrap();
        assert_eq!(
            &size_limit[3 * BLOCK_SIZE + 124..3 * BLOCK_SIZE + 136],
            b"04000000000\0"
        );
        let mut size_limit = RejectIoPast::new(size_limit, (4 * BLOCK_SIZE) as u64);
        assert_eq!(
            parse(&mut size_limit).unwrap_err(),
            "archive payloads exceed protocol bounds"
        );
        assert!(!size_limit.attempted_forbidden_io);
    }

    #[test]
    fn parser_rejects_metadata_size_65537_before_payload_io() {
        let bytes = mutate_header(0, |header| {
            header[124..136].copy_from_slice(b"00000200001\0")
        });
        let mut archive = RejectIoPast::new(bytes, BLOCK_SIZE as u64);

        assert_eq!(
            parse(&mut archive).unwrap_err(),
            "metadata payload is outside protocol bounds"
        );
        assert!(!archive.attempted_forbidden_io);
    }

    #[test]
    fn parser_rejects_binary_offset_overflow_before_seek_or_payload_read() {
        let mut archive = NearMaxBinaryOffset::new(GOLDEN.to_vec());

        assert_eq!(parse(&mut archive).unwrap_err(), "binary offset overflow");
        assert!(archive.returned_adversarial_offset);
        assert!(!archive.attempted_binary_io);
    }

    #[test]
    fn parser_rejects_a_successful_absolute_seek_to_the_wrong_position() {
        let mut archive = WrongParserSkip::new(GOLDEN.to_vec());
        let expected = (4 * BLOCK_SIZE + BINARY.len()) as u64;

        assert_eq!(
            parse(&mut archive).unwrap_err(),
            format!(
                "seek returned unexpected position: expected {expected}, got {}",
                expected + 1
            )
        );
        assert!(archive.returned_wrong_position);
        assert!(!archive.attempted_read_after_wrong_position);
    }

    #[test]
    fn embedded_nul_fixture_preserves_the_canonical_name_before_garbage() {
        let fixture = fs::read(
            Path::new(FIXTURE_ROOT)
                .join("invalid")
                .join("archive-embedded-nul.tar"),
        )
        .unwrap();
        let header = &fixture[..BLOCK_SIZE];
        let name = &header[..100];

        assert_eq!(&name[..METADATA_NAME.len()], METADATA_NAME);
        assert_eq!(name[METADATA_NAME.len()], 0);
        assert!(
            name[METADATA_NAME.len() + 1..]
                .iter()
                .any(|byte| *byte != 0)
        );

        let mut checksum_input = header.to_vec();
        checksum_input[148..156].fill(b' ');
        let checksum: u64 = checksum_input.iter().map(|byte| u64::from(*byte)).sum();
        assert_eq!(&header[148..156], format!("{checksum:06o}\0 ").as_bytes());
    }

    #[test]
    fn parser_rejects_all_exact_header_and_member_variants() {
        let mut variants = vec![
            mutate_header(0, |header| set_name(header, b"renamed-meta.json")),
            mutate_header(3 * BLOCK_SIZE, |header| {
                header[156] = b'1';
                header[157..163].copy_from_slice(b"target");
            }),
            mutate_header(3 * BLOCK_SIZE, |header| header[156] = b'4'),
            mutate_header(3 * BLOCK_SIZE, |header| header[156] = b'5'),
            mutate_header(3 * BLOCK_SIZE, |header| header[156] = b'6'),
            mutate_header(3 * BLOCK_SIZE, |header| {
                header[124..136].copy_from_slice(b"00000000000\0")
            }),
            mutate_header(3 * BLOCK_SIZE, |header| {
                header[124..136].copy_from_slice(b"04000000000\0")
            }),
        ];
        for offset in [
            100, 108, 116, 136, 157, 257, 263, 265, 297, 329, 337, 345, 500,
        ] {
            variants.push(mutate_header(0, |header| header[offset] ^= 1));
        }

        for bytes in variants {
            assert!(parse(&mut Cursor::new(bytes)).is_err());
        }
    }

    fn mutate_header(offset: usize, change: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut archive = GOLDEN.to_vec();
        let header = &mut archive[offset..offset + BLOCK_SIZE];
        change(header);
        header[148..156].fill(b' ');
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        archive
    }

    fn set_name(header: &mut [u8], name: &[u8]) {
        header[..100].fill(0);
        header[..name.len()].copy_from_slice(name);
    }

    #[test]
    fn encoder_rejects_bounds_overflow_and_inexact_inputs() {
        let mut output = Vec::new();
        assert_eq!(
            encode(
                &mut output,
                &mut Cursor::new([b'm']),
                1,
                &mut Cursor::new([b'b']),
                u64::MAX,
            )
            .unwrap_err(),
            "payload size overflow"
        );
        assert!(output.is_empty());

        for (metadata_size, binary_size) in
            [(0, 1), (65_537, 1), (1, 0), (1, 536_870_912), (u64::MAX, 1)]
        {
            let mut output = Vec::new();
            assert!(
                encode(
                    &mut output,
                    &mut Cursor::new([b'm']),
                    metadata_size,
                    &mut Cursor::new([b'b']),
                    binary_size,
                )
                .is_err()
            );
            assert!(output.is_empty(), "wrote before rejecting size bounds");
        }

        for (metadata, metadata_size, binary, binary_size) in [
            (&b"metadata"[..], 7, &b"binary"[..], 6),
            (&b"metadata"[..], 9, &b"binary"[..], 6),
            (&b"metadata"[..], 8, &b"binary"[..], 5),
            (&b"metadata"[..], 8, &b"binary"[..], 7),
        ] {
            let mut output = Vec::new();
            assert!(
                encode(
                    &mut output,
                    &mut Cursor::new(metadata),
                    metadata_size,
                    &mut Cursor::new(binary),
                    binary_size,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn size_bounds_accept_both_inclusive_limits_without_allocating_payloads() {
        let metadata_size = METADATA_LIMIT as u64;
        let binary_size = BINARY_LIMIT - metadata_size;

        validate_sizes(metadata_size, binary_size).unwrap();
        assert_eq!(
            validate_sizes(metadata_size, binary_size + 1).unwrap_err(),
            "archive payloads exceed protocol bounds"
        );
    }

    #[test]
    fn review_staged_archive_rejects_same_size_binary_payload_corruption() {
        use sha2::{Digest, Sha256};

        let mut bytes = Vec::new();
        encode(
            &mut bytes,
            &mut Cursor::new(METADATA),
            METADATA.len() as u64,
            &mut Cursor::new(BINARY),
            BINARY.len() as u64,
        )
        .unwrap();
        let parsed = parse(&mut Cursor::new(&bytes)).unwrap();
        bytes[usize::try_from(parsed.binary_offset).unwrap()] ^= 1;
        let parsed = parse(&mut Cursor::new(&bytes)).unwrap();
        let expected = format!("sha256:{:x}", Sha256::digest(BINARY));

        let error = verify_binary_sha256(&mut Cursor::new(bytes), &parsed, &expected).unwrap_err();
        assert_eq!(error, "staged binary digest changed");
    }

    #[test]
    fn owned_path_cleanup_removes_files_accepts_absence_and_reports_failure() {
        let parent = TempDir::new();
        let path = parent.path().join("owned");
        fs::write(&path, b"temporary").unwrap();

        cleanup_owned_path(&path).unwrap();
        assert!(!path.exists());
        cleanup_owned_path(&path).unwrap();

        fs::create_dir(&path).unwrap();
        let error = cleanup_owned_path(&path).unwrap_err();
        assert!(error.starts_with("failed to remove owned path"));
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn extractor_atomically_publishes_one_executable_unlinked_binary() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let parent = TempDir::new();
        let mut archive = Cursor::new(GOLDEN);
        let output = extract_binary(&mut archive, parent.path()).unwrap();

        assert_eq!(output, parent.path().join("app-cli"));
        assert_eq!(fs::read(&output).unwrap(), BINARY);
        let metadata = fs::symlink_metadata(&output).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o755);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
    }

    #[test]
    fn extractor_rejects_a_nonempty_output_parent_without_changes() {
        let parent = TempDir::new();
        fs::write(parent.path().join("occupied"), b"sentinel").unwrap();
        let mut archive = Cursor::new(GOLDEN);

        assert!(extract_binary(&mut archive, parent.path()).is_err());
        assert_eq!(
            fs::read(parent.path().join("occupied")).unwrap(),
            b"sentinel"
        );
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
    }

    #[test]
    fn extraction_failure_removes_the_temporary_output() {
        let parent = TempDir::new();
        let mut archive = FailAtBinary::new(GOLDEN.to_vec());

        assert!(extract_binary(&mut archive, parent.path()).is_err());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn extraction_reports_primary_and_cleanup_errors() {
        let parent = TempDir::new();
        let temporary = parent.path().join(".app-cli.tmp");
        let mut archive = ReplaceTempWithDirectory::new(GOLDEN.to_vec(), temporary.clone());

        let error = extract_binary(&mut archive, parent.path()).unwrap_err();
        assert!(error.contains("controlled binary read failure"));
        assert!(error.contains("cleanup failed: owned path identity changed"));
        assert!(temporary.is_dir());
        assert!(!parent.path().join("app-cli").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extraction_never_removes_a_replacement_at_its_owned_temporary_path() {
        let parent = TempDir::new();
        let temporary = parent.path().join(".app-cli.tmp");
        let mut archive = ReplaceTempWithFile::new(GOLDEN.to_vec(), temporary.clone());

        let error = extract_binary(&mut archive, parent.path()).unwrap_err();
        assert!(error.contains("controlled binary read failure"));
        assert!(error.contains("cleanup failed: owned path identity changed"));
        assert_eq!(fs::read(temporary).unwrap(), b"sentinel");
        assert!(!parent.path().join("app-cli").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extraction_never_publishes_a_same_shape_temporary_replacement() {
        let parent = TempDir::new();
        let temporary = parent.path().join(".app-cli.tmp");
        let mut archive = ReplaceTempAfterBinaryRead::new(GOLDEN.to_vec(), temporary.clone());

        let error = extract_binary(&mut archive, parent.path()).unwrap_err();
        assert!(error.contains("owned path identity changed"));
        assert_eq!(fs::read(temporary).unwrap(), vec![b'x'; BINARY.len()]);
        assert!(!parent.path().join("app-cli").exists());
    }

    #[cfg(unix)]
    #[test]
    fn final_name_collision_preserves_sentinel_and_cleans_temporary_output() {
        let parent = TempDir::new();
        let final_path = parent.path().join("app-cli");
        let temporary_path = parent.path().join(".app-cli.tmp");
        let mut archive = CreateFinalAtBinaryRead::new(GOLDEN.to_vec(), final_path.clone());

        assert!(extract_binary(&mut archive, parent.path()).is_err());
        assert!(archive.created_final);
        assert_eq!(fs::read(final_path).unwrap(), b"sentinel");
        assert!(!temporary_path.exists());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn extractor_rejects_a_successful_binary_seek_to_the_wrong_position() {
        let parent = TempDir::new();
        let mut archive = WrongExtractionRewind::new(GOLDEN.to_vec());
        let expected = (4 * BLOCK_SIZE) as u64;

        assert_eq!(
            extract_binary(&mut archive, parent.path()).unwrap_err(),
            format!(
                "seek returned unexpected position: expected {expected}, got {}",
                expected + 1
            )
        );
        assert!(archive.returned_wrong_position);
        assert!(fs::read_dir(parent.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_seeks_are_retried_for_parser_queries_and_extraction() {
        let mut parser = InterruptInitialQuery::new(GOLDEN.to_vec());
        parse(&mut parser).unwrap();
        assert!(parser.interrupted);

        let parent = TempDir::new();
        let mut extractor = InterruptExtractionRewind::new(GOLDEN.to_vec());
        let output = extract_binary(&mut extractor, parent.path()).unwrap();
        assert!(extractor.interrupted);
        assert_eq!(fs::read(output).unwrap(), BINARY);
    }

    #[test]
    fn extraction_never_removes_a_temporary_file_it_did_not_create() {
        let parent = TempDir::new();
        let temporary = parent.path().join(".app-cli.tmp");
        let mut archive = CreateTempOnRead::new(GOLDEN.to_vec(), temporary.clone());

        assert!(extract_binary(&mut archive, parent.path()).is_err());
        assert_eq!(fs::read(temporary).unwrap(), b"sentinel");
        assert!(!parent.path().join("app-cli").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extractor_rejects_invalid_output_parent_shapes() {
        use std::os::unix::fs::symlink;

        let lexical = TempDir::new();
        let lexical_path = lexical.path().join(".");
        assert_eq!(
            extract_binary(&mut Cursor::new(GOLDEN), &lexical_path).unwrap_err(),
            "output parent is not canonical"
        );

        let symlink_target = TempDir::new();
        let symlink_holder = TempDir::new();
        let symlink_path = symlink_holder.path().join("parent-link");
        symlink(symlink_target.path(), &symlink_path).unwrap();
        assert_eq!(
            extract_binary(&mut Cursor::new(GOLDEN), &symlink_path).unwrap_err(),
            "output parent is not canonical"
        );

        let missing = lexical.path().join("missing");
        assert!(extract_binary(&mut Cursor::new(GOLDEN), &missing).is_err());

        let regular_file = lexical.path().join("regular-file");
        fs::write(&regular_file, b"not a directory").unwrap();
        assert_eq!(
            extract_binary(&mut Cursor::new(GOLDEN), &regular_file).unwrap_err(),
            "output parent is not a directory"
        );
    }

    #[test]
    fn encoder_parser_and_extractor_never_request_unbounded_io() {
        const MAX_CHUNK: usize = 8 * 1024;
        let metadata_bytes = vec![b'm'; 32 * 1024];
        let binary = vec![b'x'; 256 * 1024];
        let mut metadata = LimitedReader::new(Cursor::new(&metadata_bytes), MAX_CHUNK);
        let mut binary_reader = LimitedReader::new(Cursor::new(&binary), MAX_CHUNK);
        let mut output = LimitedWriter::new(Vec::new(), MAX_CHUNK);
        encode(
            &mut output,
            &mut metadata,
            metadata_bytes.len() as u64,
            &mut binary_reader,
            binary.len() as u64,
        )
        .unwrap();
        assert!(metadata.max_requested <= MAX_CHUNK);
        assert!(binary_reader.max_requested <= MAX_CHUNK);
        let bytes = output.inner;

        let mut archive = LimitedReader::new(Cursor::new(bytes.clone()), MAX_CHUNK);
        let parsed = parse(&mut archive).unwrap();
        assert_eq!(parsed.metadata, metadata_bytes);
        assert_eq!(parsed.binary_size, binary.len() as u64);
        assert!(archive.max_requested <= MAX_CHUNK);

        let parent = TempDir::new();
        let mut archive = LimitedReader::new(Cursor::new(bytes), MAX_CHUNK);
        let output = extract_binary(&mut archive, parent.path()).unwrap();
        assert_eq!(fs::metadata(output).unwrap().len(), binary.len() as u64);
        assert!(archive.max_requested <= MAX_CHUNK);
    }

    struct LimitedReader<R> {
        inner: R,
        limit: usize,
        max_requested: usize,
    }

    impl<R> LimitedReader<R> {
        fn new(inner: R, limit: usize) -> Self {
            Self {
                inner,
                limit,
                max_requested: 0,
            }
        }
    }

    impl<R: Read> Read for LimitedReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.max_requested = self.max_requested.max(buffer.len());
            if buffer.len() > self.limit {
                return Err(io::Error::other("oversized read request"));
            }
            self.inner.read(buffer)
        }
    }

    impl<R: Seek> Seek for LimitedReader<R> {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    struct RejectIoPast {
        inner: Cursor<Vec<u8>>,
        read_limit: u64,
        attempted_forbidden_io: bool,
    }

    impl RejectIoPast {
        fn new(bytes: Vec<u8>, read_limit: u64) -> Self {
            Self {
                inner: Cursor::new(bytes),
                read_limit,
                attempted_forbidden_io: false,
            }
        }
    }

    impl Read for RejectIoPast {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let remaining = self.read_limit.saturating_sub(self.inner.position());
            if remaining == 0 {
                self.attempted_forbidden_io = true;
                return Err(io::Error::other("read beyond rejection boundary"));
            }
            let limit =
                usize::try_from(remaining.min(buffer.len() as u64)).map_err(io::Error::other)?;
            self.inner.read(&mut buffer[..limit])
        }
    }

    impl Seek for RejectIoPast {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if position != SeekFrom::Current(0) {
                self.attempted_forbidden_io = true;
                return Err(io::Error::other("seek beyond rejection boundary"));
            }
            self.inner.seek(position)
        }
    }

    struct NearMaxBinaryOffset {
        inner: Cursor<Vec<u8>>,
        returned_adversarial_offset: bool,
        attempted_binary_io: bool,
    }

    impl NearMaxBinaryOffset {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                returned_adversarial_offset: false,
                attempted_binary_io: false,
            }
        }
    }

    impl Read for NearMaxBinaryOffset {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.returned_adversarial_offset {
                self.attempted_binary_io = true;
                return Err(io::Error::other("binary payload read attempted"));
            }
            self.inner.read(buffer)
        }
    }

    impl Seek for NearMaxBinaryOffset {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if position == SeekFrom::Current(0) && self.inner.position() == (4 * BLOCK_SIZE) as u64
            {
                self.returned_adversarial_offset = true;
                return Ok(u64::MAX);
            }
            if self.returned_adversarial_offset {
                self.attempted_binary_io = true;
                return Err(io::Error::other("binary payload seek attempted"));
            }
            self.inner.seek(position)
        }
    }

    struct WrongParserSkip {
        inner: Cursor<Vec<u8>>,
        returned_wrong_position: bool,
        attempted_read_after_wrong_position: bool,
    }

    impl WrongParserSkip {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                returned_wrong_position: false,
                attempted_read_after_wrong_position: false,
            }
        }
    }

    impl Read for WrongParserSkip {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.returned_wrong_position {
                self.attempted_read_after_wrong_position = true;
                return Err(io::Error::other("read after wrong seek position"));
            }
            self.inner.read(buffer)
        }
    }

    impl Seek for WrongParserSkip {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if let SeekFrom::Start(expected) = position {
                self.returned_wrong_position = true;
                return Ok(expected + 1);
            }
            self.inner.seek(position)
        }
    }

    #[cfg(unix)]
    struct WrongExtractionRewind {
        inner: Cursor<Vec<u8>>,
        returned_wrong_position: bool,
    }

    #[cfg(unix)]
    impl WrongExtractionRewind {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                returned_wrong_position: false,
            }
        }
    }

    #[cfg(unix)]
    impl Read for WrongExtractionRewind {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buffer)
        }
    }

    #[cfg(unix)]
    impl Seek for WrongExtractionRewind {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if position == SeekFrom::Start((4 * BLOCK_SIZE) as u64) {
                self.returned_wrong_position = true;
                return Ok((4 * BLOCK_SIZE + 1) as u64);
            }
            self.inner.seek(position)
        }
    }

    struct InterruptInitialQuery {
        inner: Cursor<Vec<u8>>,
        interrupted: bool,
    }

    impl InterruptInitialQuery {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                interrupted: false,
            }
        }
    }

    impl Read for InterruptInitialQuery {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buffer)
        }
    }

    impl Seek for InterruptInitialQuery {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if !self.interrupted && position == SeekFrom::Current(0) {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.inner.seek(position)
        }
    }

    #[cfg(unix)]
    struct InterruptExtractionRewind {
        inner: Cursor<Vec<u8>>,
        interrupted: bool,
    }

    #[cfg(unix)]
    impl InterruptExtractionRewind {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                interrupted: false,
            }
        }
    }

    #[cfg(unix)]
    impl Read for InterruptExtractionRewind {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buffer)
        }
    }

    #[cfg(unix)]
    impl Seek for InterruptExtractionRewind {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if !self.interrupted && position == SeekFrom::Start((4 * BLOCK_SIZE) as u64) {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.inner.seek(position)
        }
    }

    struct LimitedWriter<W> {
        inner: W,
        limit: usize,
    }

    impl<W> LimitedWriter<W> {
        fn new(inner: W, limit: usize) -> Self {
            Self { inner, limit }
        }
    }

    impl<W: Write> Write for LimitedWriter<W> {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if buffer.len() > self.limit {
                return Err(io::Error::other("oversized write request"));
            }
            self.inner.write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    struct FailAtBinary {
        inner: Cursor<Vec<u8>>,
    }

    impl FailAtBinary {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
            }
        }
    }

    impl Read for FailAtBinary {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.inner.position() == 4 * BLOCK_SIZE as u64 {
                return Err(io::Error::other("controlled binary read failure"));
            }
            self.inner.read(buffer)
        }
    }

    impl Seek for FailAtBinary {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[cfg(unix)]
    struct ReplaceTempWithDirectory {
        inner: Cursor<Vec<u8>>,
        temporary_path: PathBuf,
        replaced: bool,
    }

    #[cfg(unix)]
    impl ReplaceTempWithDirectory {
        fn new(bytes: Vec<u8>, temporary_path: PathBuf) -> Self {
            Self {
                inner: Cursor::new(bytes),
                temporary_path,
                replaced: false,
            }
        }
    }

    #[cfg(unix)]
    impl Read for ReplaceTempWithDirectory {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.replaced && self.inner.position() == (4 * BLOCK_SIZE) as u64 {
                fs::remove_file(&self.temporary_path)?;
                fs::create_dir(&self.temporary_path)?;
                self.replaced = true;
                return Err(io::Error::other("controlled binary read failure"));
            }
            self.inner.read(buffer)
        }
    }

    #[cfg(unix)]
    impl Seek for ReplaceTempWithDirectory {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[cfg(unix)]
    struct ReplaceTempWithFile {
        inner: Cursor<Vec<u8>>,
        temporary_path: PathBuf,
        replaced: bool,
    }

    #[cfg(unix)]
    impl ReplaceTempWithFile {
        fn new(bytes: Vec<u8>, temporary_path: PathBuf) -> Self {
            Self {
                inner: Cursor::new(bytes),
                temporary_path,
                replaced: false,
            }
        }
    }

    #[cfg(unix)]
    impl Read for ReplaceTempWithFile {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.replaced && self.inner.position() == (4 * BLOCK_SIZE) as u64 {
                fs::remove_file(&self.temporary_path)?;
                fs::write(&self.temporary_path, b"sentinel")?;
                self.replaced = true;
                return Err(io::Error::other("controlled binary read failure"));
            }
            self.inner.read(buffer)
        }
    }

    #[cfg(unix)]
    impl Seek for ReplaceTempWithFile {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[cfg(unix)]
    struct ReplaceTempAfterBinaryRead {
        inner: Cursor<Vec<u8>>,
        temporary_path: PathBuf,
        replaced: bool,
    }

    #[cfg(unix)]
    impl ReplaceTempAfterBinaryRead {
        fn new(bytes: Vec<u8>, temporary_path: PathBuf) -> Self {
            Self {
                inner: Cursor::new(bytes),
                temporary_path,
                replaced: false,
            }
        }
    }

    #[cfg(unix)]
    impl Read for ReplaceTempAfterBinaryRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            use std::os::unix::fs::PermissionsExt;

            let read = self.inner.read(buffer)?;
            if !self.replaced && self.inner.position() == (4 * BLOCK_SIZE + BINARY.len()) as u64 {
                fs::remove_file(&self.temporary_path)?;
                fs::write(&self.temporary_path, vec![b'x'; BINARY.len()])?;
                fs::set_permissions(&self.temporary_path, fs::Permissions::from_mode(0o755))?;
                self.replaced = true;
            }
            Ok(read)
        }
    }

    #[cfg(unix)]
    impl Seek for ReplaceTempAfterBinaryRead {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[cfg(unix)]
    struct CreateFinalAtBinaryRead {
        inner: Cursor<Vec<u8>>,
        final_path: PathBuf,
        created_final: bool,
    }

    #[cfg(unix)]
    impl CreateFinalAtBinaryRead {
        fn new(bytes: Vec<u8>, final_path: PathBuf) -> Self {
            Self {
                inner: Cursor::new(bytes),
                final_path,
                created_final: false,
            }
        }
    }

    #[cfg(unix)]
    impl Read for CreateFinalAtBinaryRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.created_final && self.inner.position() == (4 * BLOCK_SIZE) as u64 {
                fs::write(&self.final_path, b"sentinel")?;
                self.created_final = true;
            }
            self.inner.read(buffer)
        }
    }

    #[cfg(unix)]
    impl Seek for CreateFinalAtBinaryRead {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    struct CreateTempOnRead {
        inner: Cursor<Vec<u8>>,
        path: PathBuf,
        created: bool,
    }

    impl CreateTempOnRead {
        fn new(bytes: Vec<u8>, path: PathBuf) -> Self {
            Self {
                inner: Cursor::new(bytes),
                path,
                created: false,
            }
        }
    }

    impl Read for CreateTempOnRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.created {
                fs::write(&self.path, b"sentinel")?;
                self.created = true;
            }
            self.inner.read(buffer)
        }
    }

    impl Seek for CreateTempOnRead {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }
}
