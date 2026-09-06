use crate::Result;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{DeserializeOwned, Visitor},
};

pub const EXPECTED_LIMIT: usize = 16 * 1024;
pub const METADATA_LIMIT: usize = 64 * 1024;
pub const BINARY_LIMIT: u64 = 512 * 1024 * 1024;
pub const INTERPRETER: &str = "/lib64/ld-linux-x86-64.so.2";
pub const IMAGE_REPOSITORY: &str = "ghcr.io/stackpop/edgezero-build-app-cli";

// All object keys are fixed ASCII and declared in lexical order. Only bounded
// integers and strings reach this encoder; it is not a generic JCS serializer.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Caller {
    app_cli_bin: String,
    app_cli_package: String,
    app_repo_id: String,
    source_revision: String,
    workspace_id: String,
}

impl Caller {
    fn validate(&self) -> Result<()> {
        name(&self.app_cli_bin)?;
        name(&self.app_cli_package)?;
        let id = self
            .app_repo_id
            .parse::<u64>()
            .map_err(|_| "invalid repository id")?;
        require(
            id != 0 && id.to_string() == self.app_repo_id,
            "noncanonical repository id",
        )?;
        require(
            nonzero_hex(&self.source_revision, 40),
            "invalid source revision",
        )?;
        digest(&self.workspace_id)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Platform {
    container_ref: String,
    platform_id: String,
    provenance_protocol: u8,
}

impl Platform {
    fn validate(&self) -> Result<()> {
        digest(&self.platform_id)?;
        require(
            self.provenance_protocol == 1,
            "unsupported provenance protocol",
        )?;
        require(
            self.container_ref == format!("{IMAGE_REPOSITORY}@{}", self.platform_id),
            "container-ref differs from platform-id",
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Expected {
    caller: Caller,
    platform: Platform,
    schema_version: u8,
}

impl Expected {
    pub fn new(
        repo: &str,
        source: &str,
        package: &str,
        bin: &str,
        workspace: &str,
        platform: &str,
    ) -> Result<Self> {
        let expected = Self {
            caller: Caller {
                app_cli_bin: bin.into(),
                app_cli_package: package.into(),
                app_repo_id: repo.into(),
                source_revision: source.into(),
                workspace_id: workspace.into(),
            },
            platform: Platform {
                container_ref: format!("{IMAGE_REPOSITORY}@{platform}"),
                platform_id: platform.into(),
                provenance_protocol: 1,
            },
            schema_version: 1,
        };
        expected.validate()?;
        Ok(expected)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        parse(bytes, EXPECTED_LIMIT, Self::validate)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode(self, EXPECTED_LIMIT)
    }

    fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "unsupported schema version")?;
        self.caller.validate()?;
        self.platform.validate()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interpreter {
    Static,
    Dynamic,
}

impl Serialize for Interpreter {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Static => serializer.serialize_unit(),
            Self::Dynamic => serializer.serialize_str(INTERPRETER),
        }
    }
}

impl<'de> Deserialize<'de> for Interpreter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct RequiredInterpreter;
        impl Visitor<'_> for RequiredInterpreter {
            type Value = Interpreter;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("null or the fixed x86-64 interpreter")
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(Interpreter::Static)
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                if value == INTERPRETER {
                    Ok(Interpreter::Dynamic)
                } else {
                    Err(E::custom("unsupported ELF interpreter"))
                }
            }
        }
        deserializer.deserialize_any(RequiredInterpreter)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Abi {
    interpreter: Interpreter,
    machine: String,
    needed: Vec<String>,
}

impl Abi {
    pub fn new(interpreter: Interpreter, mut needed: Vec<String>) -> Result<Self> {
        needed.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        let abi = Self {
            interpreter,
            machine: "x86_64".into(),
            needed,
        };
        abi.validate()?;
        Ok(abi)
    }

    fn validate(&self) -> Result<()> {
        require(self.machine == "x86_64", "unsupported machine")?;
        for dependency in &self.needed {
            name(dependency)?;
            require(
                !dependency.contains('$'),
                "dependency expansion is unsupported",
            )?;
        }
        require(
            self.needed
                .windows(2)
                .all(|pair| pair[0].as_bytes() <= pair[1].as_bytes()),
            "needed entries are not byte-sorted",
        )?;
        require(
            self.interpreter != Interpreter::Static || self.needed.is_empty(),
            "static binary has dependencies",
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Metadata {
    abi: Abi,
    app_cli_version: String,
    binary_sha256: String,
    binary_size: u64,
    caller: Caller,
    platform: Platform,
    schema_version: u8,
}

impl Metadata {
    pub fn new(
        expected: &Expected,
        abi: Abi,
        version: &str,
        hash: &str,
        size: u64,
    ) -> Result<Self> {
        expected.validate()?;
        let metadata = Self {
            abi,
            app_cli_version: version.into(),
            binary_sha256: hash.into(),
            binary_size: size,
            caller: expected.caller.clone(),
            platform: expected.platform.clone(),
            schema_version: expected.schema_version,
        };
        metadata.canonical_bytes()?;
        Ok(metadata)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        parse(bytes, METADATA_LIMIT, Self::validate)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode(self, METADATA_LIMIT)
    }

    pub fn matches(&self, expected: &Expected) -> Result<()> {
        self.validate()?;
        expected.validate()?;
        require(
            self.caller == expected.caller
                && self.platform == expected.platform
                && self.schema_version == expected.schema_version,
            "provenance identity mismatch",
        )
    }

    fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "unsupported schema version")?;
        self.caller.validate()?;
        self.platform.validate()?;
        self.abi.validate()?;
        name(&self.app_cli_version)?;
        digest(&self.binary_sha256)?;
        require(
            (1..=BINARY_LIMIT).contains(&self.binary_size),
            "binary-size is outside protocol bounds",
        )
    }
}

fn require(valid: bool, reason: &str) -> Result<()> {
    if valid { Ok(()) } else { Err(reason.into()) }
}

fn nonzero_hex(value: &str, width: usize) -> bool {
    value.len() == width
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && value.bytes().any(|b| b != b'0')
}

fn digest(value: &str) -> Result<()> {
    require(
        value
            .strip_prefix("sha256:")
            .is_some_and(|hash| nonzero_hex(hash, 64)),
        "invalid sha256 digest",
    )
}

fn name(value: &str) -> Result<()> {
    require(
        (1..=255).contains(&value.len())
            && !value
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\'),
        "invalid bounded name",
    )
}

fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    require(bytes.len() <= limit, "JSON exceeds protocol byte limit")?;
    Ok(bytes)
}

fn parse<T: DeserializeOwned + Serialize>(
    bytes: &[u8],
    limit: usize,
    validate: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    require(
        !bytes.is_empty() && bytes.len() <= limit,
        "JSON exceeds protocol byte limit",
    )?;
    // Each derived struct rejects unknown and duplicate fields while visiting
    // keys, before constructing the enclosing object. No Value/map intermediate.
    let value: T = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    validate(&value)?;
    require(
        encode(&value, limit)? == bytes,
        "JSON bytes are not canonical",
    )?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const EXPECTED: &[u8] =
        include_bytes!("../../../docker/build-app-cli/fixtures/provenance/valid/expected.json");
    const STATIC: &[u8] =
        include_bytes!("../../../docker/build-app-cli/fixtures/provenance/valid/static-meta.json");
    const DYNAMIC: &[u8] =
        include_bytes!("../../../docker/build-app-cli/fixtures/provenance/valid/dynamic-meta.json");

    fn changed(bytes: &[u8], path: &str, value: Value) -> Vec<u8> {
        let mut document: Value = serde_json::from_slice(bytes).unwrap();
        *document.pointer_mut(path).expect("fixture field") = value;
        serde_json::to_vec(&document).unwrap()
    }

    #[test]
    fn golden_bytes_round_trip_and_complete_identity_matches() {
        let expected = Expected::parse(EXPECTED).unwrap();
        assert_eq!(expected.canonical_bytes().unwrap(), EXPECTED);
        for bytes in [STATIC, DYNAMIC] {
            let metadata = Metadata::parse(bytes).unwrap();
            assert_eq!(metadata.canonical_bytes().unwrap(), bytes);
            metadata.matches(&expected).unwrap();
        }
    }

    #[test]
    fn canonical_wire_rejects_alternate_json_spellings() {
        for bytes in [EXPECTED, STATIC, DYNAMIC] {
            let text = std::str::from_utf8(bytes).unwrap();
            let bad = [
                format!(" {text}"),
                format!("{text}\n"),
                format!("\u{feff}{text}"),
                format!("{text}{{}}"),
                text.replace("\"schema-version\":1", "\"schema-version\":1.0"),
                text.replace("\"schema-version\":1", "\"schema-version\":1e0"),
                text.replace("edgezero-cli", "edgezero\\u002dcli"),
                text.replace("\"caller\":", "\"caller\" :"),
                text.replace("ghcr.io/", "ghcr.io\\/"),
            ];
            for candidate in bad {
                let valid = if bytes == EXPECTED {
                    Expected::parse(candidate.as_bytes()).is_ok()
                } else {
                    Metadata::parse(candidate.as_bytes()).is_ok()
                };
                assert!(!valid, "accepted alternate bytes: {candidate}");
            }
        }
        assert!(Expected::parse(&[0xff]).is_err());
        assert!(Expected::parse(&vec![b' '; 16385]).is_err());
        assert!(Metadata::parse(&vec![b' '; 65537]).is_err());
    }

    #[test]
    fn every_object_rejects_unknown_missing_and_duplicate_fields() {
        for fixture in [EXPECTED, STATIC, DYNAMIC] {
            let original: Value = serde_json::from_slice(fixture).unwrap();
            for path in ["", "/caller", "/platform", "/abi"] {
                let Some(object) = original.pointer(path).and_then(Value::as_object) else {
                    continue;
                };
                for key in object.keys() {
                    let mut missing = original.clone();
                    missing
                        .pointer_mut(path)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .remove(key);
                    let encoded = serde_json::to_vec(&missing).unwrap();
                    assert!(
                        if fixture == EXPECTED {
                            Expected::parse(&encoded).is_err()
                        } else {
                            Metadata::parse(&encoded).is_err()
                        },
                        "missing {path}/{key}"
                    );
                    let prefix = format!("\"{key}\":");
                    let duplicate = std::str::from_utf8(fixture).unwrap().replacen(
                        &prefix,
                        &format!("{prefix}{},{prefix}", object[key]),
                        1,
                    );
                    let error = if fixture == EXPECTED {
                        Expected::parse(duplicate.as_bytes()).unwrap_err()
                    } else {
                        Metadata::parse(duplicate.as_bytes()).unwrap_err()
                    };
                    assert!(error.contains("duplicate field"), "{path}/{key}: {error}");
                }
                let mut extra = original.clone();
                extra
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert("unknown".into(), json!(1));
                let encoded = serde_json::to_vec(&extra).unwrap();
                assert!(if fixture == EXPECTED {
                    Expected::parse(&encoded).is_err()
                } else {
                    Metadata::parse(&encoded).is_err()
                });
            }
        }
    }

    #[test]
    fn exact_types_versions_decimals_and_hashes() {
        for (path, values) in [
            (
                "/schema-version",
                vec![json!(0), json!(2), json!("1"), json!(true), Value::Null],
            ),
            (
                "/platform/provenance-protocol",
                vec![json!(0), json!(2), json!("1"), json!(-1)],
            ),
            (
                "/caller/app-repo-id",
                vec![
                    json!("0"),
                    json!("01"),
                    json!("+1"),
                    json!("-1"),
                    json!(" 1"),
                    json!("18446744073709551616"),
                    json!(1),
                ],
            ),
            (
                "/caller/source-revision",
                vec![
                    json!("0".repeat(40)),
                    json!("A".repeat(40)),
                    json!("1".repeat(39)),
                    json!("g".repeat(40)),
                    Value::Null,
                ],
            ),
            (
                "/caller/workspace-id",
                vec![
                    json!("sha256:".to_owned() + &"0".repeat(64)),
                    json!("sha256:".to_owned() + &"A".repeat(64)),
                    json!("1".repeat(64)),
                    json!(1),
                ],
            ),
            (
                "/platform/platform-id",
                vec![
                    json!("sha256:".to_owned() + &"0".repeat(64)),
                    json!("sha256:123"),
                    json!("sha512:".to_owned() + &"1".repeat(64)),
                ],
            ),
            (
                "/platform/container-ref",
                vec![
                    json!("ghcr.io/attacker/image@sha256:".to_owned() + &"3".repeat(64)),
                    json!("ghcr.io/stackpop/edgezero-build-app-cli:v1"),
                    json!(
                        "ghcr.io/stackpop/edgezero-build-app-cli@sha256:".to_owned()
                            + &"4".repeat(64)
                    ),
                ],
            ),
        ] {
            for value in values {
                assert!(
                    Expected::parse(&changed(EXPECTED, path, value.clone())).is_err(),
                    "{path}={value}"
                );
                assert!(
                    Metadata::parse(&changed(DYNAMIC, path, value.clone())).is_err(),
                    "{path}={value}"
                );
            }
        }
        for value in ["1", "18446744073709551615"] {
            Expected::parse(&changed(EXPECTED, "/caller/app-repo-id", json!(value))).unwrap();
        }
    }

    #[test]
    fn names_are_bounded_by_utf8_bytes_and_reject_controls_and_separators() {
        for path in [
            "/caller/app-cli-bin",
            "/caller/app-cli-package",
            "/app-cli-version",
        ] {
            for value in [
                "".into(),
                "x".repeat(256),
                "\u{e9}".repeat(128),
                "a/b".into(),
                "a\\b".into(),
                "a\n".into(),
                "a\u{85}".into(),
            ] {
                assert!(
                    Metadata::parse(&changed(DYNAMIC, path, json!(value))).is_err(),
                    "{path}"
                );
            }
            for value in [
                "x".into(),
                "x".repeat(255),
                "\u{e9}".repeat(127) + "a",
                "a\"b".into(),
            ] {
                let bytes = changed(DYNAMIC, path, json!(value));
                assert_eq!(
                    Metadata::parse(&bytes).unwrap().canonical_bytes().unwrap(),
                    bytes
                );
            }
        }
    }

    #[test]
    fn binary_observations_and_abi_are_closed() {
        for value in [
            json!(0),
            json!(536870913u64),
            json!(-1),
            json!(1.5),
            json!("1"),
        ] {
            assert!(Metadata::parse(&changed(DYNAMIC, "/binary-size", value)).is_err());
        }
        for value in [1, 536870912] {
            Metadata::parse(&changed(DYNAMIC, "/binary-size", json!(value))).unwrap();
        }
        for value in [
            json!("0".repeat(64)),
            json!("sha256:".to_owned() + &"0".repeat(64)),
            Value::Null,
        ] {
            assert!(Metadata::parse(&changed(DYNAMIC, "/binary-sha256", value)).is_err());
        }
        for value in [json!("aarch64"), json!(62), Value::Null] {
            assert!(Metadata::parse(&changed(DYNAMIC, "/abi/machine", value)).is_err());
        }
        for value in [json!("/wrong/loader"), json!(""), json!(true)] {
            assert!(Metadata::parse(&changed(DYNAMIC, "/abi/interpreter", value)).is_err());
        }
        for value in [
            json!(["z.so", "a.so"]),
            json!([""]),
            json!(["a/b"]),
            json!(["a\\b"]),
            json!(["$ORIGIN"]),
            json!(["a\u{7f}"]),
            json!(["a".repeat(256)]),
            json!([1]),
            Value::Null,
        ] {
            assert!(Metadata::parse(&changed(DYNAMIC, "/abi/needed", value)).is_err());
        }
        for value in [
            json!([]),
            json!(["a.so", "a.so", "b.so"]),
            json!(["a".repeat(255)]),
        ] {
            Metadata::parse(&changed(DYNAMIC, "/abi/needed", value)).unwrap();
        }
    }

    #[test]
    fn each_caller_or_platform_identity_change_is_detected() {
        let expected = Expected::parse(EXPECTED).unwrap();
        for (path, value) in [
            ("/caller/app-cli-bin", json!("different")),
            ("/caller/app-cli-package", json!("different")),
            ("/caller/app-repo-id", json!("2")),
            ("/caller/source-revision", json!("2".repeat(40))),
            (
                "/caller/workspace-id",
                json!("sha256:".to_owned() + &"4".repeat(64)),
            ),
        ] {
            Metadata::parse(&changed(DYNAMIC, path, value))
                .unwrap()
                .matches(&expected)
                .unwrap_err();
        }
        let bytes = changed(
            DYNAMIC,
            "/platform/platform-id",
            json!("sha256:".to_owned() + &"4".repeat(64)),
        );
        let bytes = changed(
            &bytes,
            "/platform/container-ref",
            json!("ghcr.io/stackpop/edgezero-build-app-cli@sha256:".to_owned() + &"4".repeat(64)),
        );
        Metadata::parse(&bytes)
            .unwrap()
            .matches(&expected)
            .unwrap_err();
    }

    #[test]
    fn expected_constructor_fixes_versions_and_derives_container_ref() {
        let source = "1".repeat(40);
        let workspace = "sha256:".to_owned() + &"2".repeat(64);
        let platform = "sha256:".to_owned() + &"3".repeat(64);
        let expected = Expected::new(
            "123456",
            &source,
            "edgezero-cli",
            "edgezero",
            &workspace,
            &platform,
        )
        .unwrap();
        assert_eq!(expected.canonical_bytes().unwrap(), EXPECTED);
        assert!(
            Expected::new(
                "0",
                &source,
                "edgezero-cli",
                "edgezero",
                &workspace,
                &platform
            )
            .is_err()
        );
        assert!(
            Expected::new(
                "1",
                &source,
                "edgezero-cli",
                "../bad",
                &workspace,
                &platform
            )
            .is_err()
        );
    }

    #[test]
    fn schema_matches_both_closed_documents_but_is_not_the_wire_parser() {
        let schema: Value = serde_json::from_slice(include_bytes!(
            "../../../docker/build-app-cli/provenance.schema.json"
        ))
        .unwrap();
        assert!(jsonschema::draft202012::meta::is_valid(&schema));
        let validator = jsonschema::draft202012::new(&schema).unwrap();
        for fixture in [EXPECTED, STATIC, DYNAMIC] {
            let original: Value = serde_json::from_slice(fixture).unwrap();
            assert!(validator.is_valid(&original));
            for path in ["", "/caller", "/platform", "/abi"] {
                let Some(object) = original.pointer(path).and_then(Value::as_object) else {
                    continue;
                };
                for key in object.keys() {
                    let mut missing = original.clone();
                    missing
                        .pointer_mut(path)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .remove(key);
                    assert!(!validator.is_valid(&missing), "missing {path}/{key}");
                    let mut wrong = original.clone();
                    *wrong.pointer_mut(&format!("{path}/{key}")).unwrap() = json!(true);
                    assert!(!validator.is_valid(&wrong), "wrong type {path}/{key}");
                }
                let mut extra = original.clone();
                extra
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert("unknown".into(), json!(1));
                assert!(!validator.is_valid(&extra));
            }
        }
        let noncanonical = [EXPECTED, b"\n"].concat();
        assert!(validator.is_valid(&serde_json::from_slice::<Value>(&noncanonical).unwrap()));
        assert!(Expected::parse(&noncanonical).is_err());
    }

    #[test]
    fn metadata_byte_limit_is_inclusive_and_needed_order_is_utf8() {
        let empty = changed(DYNAMIC, "/abi/needed", json!([]));
        let room = METADATA_LIMIT - empty.len();
        let count = (room + 1) / 4;
        let remainder = room - (4 * count - 1);
        let mut needed = vec!["a".to_owned(); count];
        needed[count - 1] = "z".repeat(1 + remainder);
        let exact = changed(DYNAMIC, "/abi/needed", json!(needed));
        assert_eq!(exact.len(), METADATA_LIMIT);
        assert_eq!(
            Metadata::parse(&exact).unwrap().canonical_bytes().unwrap(),
            exact
        );
        needed[count - 1].push('z');
        let oversized = changed(DYNAMIC, "/abi/needed", json!(needed));
        assert_eq!(oversized.len(), METADATA_LIMIT + 1);
        assert!(Metadata::parse(&oversized).is_err());
        Metadata::parse(&changed(
            DYNAMIC,
            "/abi/needed",
            json!(["\u{e000}", "\u{10000}"]),
        ))
        .unwrap();
        assert!(
            Metadata::parse(&changed(
                DYNAMIC,
                "/abi/needed",
                json!(["\u{10000}", "\u{e000}"])
            ))
            .is_err()
        );
        assert!(Metadata::parse(&changed(DYNAMIC, "/abi/interpreter", Value::Null)).is_err());
    }

    #[test]
    fn escaped_duplicate_keys_are_rejected_during_typed_parsing() {
        let expected = std::str::from_utf8(EXPECTED).unwrap().replace(
            "\"app-repo-id\":",
            "\"app-repo-\\u0069d\":\"123456\",\"app-repo-id\":",
        );
        assert!(
            Expected::parse(expected.as_bytes())
                .unwrap_err()
                .contains("duplicate field")
        );
        let metadata = std::str::from_utf8(STATIC).unwrap().replace(
            "\"interpreter\":",
            "\"\\u0069nterpreter\":null,\"interpreter\":",
        );
        assert!(
            Metadata::parse(metadata.as_bytes())
                .unwrap_err()
                .contains("duplicate field")
        );
    }

    #[test]
    fn typed_metadata_encoder_preserves_observations_and_sorts_duplicates() {
        let expected = Expected::parse(EXPECTED).unwrap();
        let abi = Abi::new(
            Interpreter::Dynamic,
            vec!["libm.so.6".into(), "libc.so.6".into(), "libc.so.6".into()],
        )
        .unwrap();
        let hash = "sha256:".to_owned() + &"4".repeat(64);
        assert_eq!(
            Metadata::new(&expected, abi.clone(), "0.1.0", &hash, 123)
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            DYNAMIC
        );
        let static_abi = Abi::new(Interpreter::Static, vec![]).unwrap();
        assert_eq!(
            Metadata::new(&expected, static_abi, "0.1.0", &hash, 123)
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            STATIC
        );
        assert!(Metadata::new(&expected, abi.clone(), "", &hash, 123).is_err());
        assert!(Metadata::new(&expected, abi, "0.1.0", &hash, 0).is_err());
        assert!(Abi::new(Interpreter::Static, vec!["libc.so.6".into()]).is_err());
        assert!(Abi::new(Interpreter::Dynamic, vec!["$ORIGIN".into()]).is_err());
    }

    #[test]
    fn reordered_keys_and_invalid_surrogates_are_not_canonical_wire() {
        let source = std::str::from_utf8(EXPECTED).unwrap();
        let reordered = source.replace(
            "\"app-cli-bin\":\"edgezero\",\"app-cli-package\":\"edgezero-cli\"",
            "\"app-cli-package\":\"edgezero-cli\",\"app-cli-bin\":\"edgezero\"",
        );
        assert!(
            Expected::parse(reordered.as_bytes())
                .unwrap_err()
                .contains("not canonical")
        );
        for spelling in ["\\uD800", "\\uDC00", "\\uD800x"] {
            assert!(Expected::parse(source.replace("edgezero-cli", spelling).as_bytes()).is_err());
        }
    }

    #[test]
    fn schema_enforces_scalar_patterns_constants_and_bounds() {
        let schema: Value = serde_json::from_slice(include_bytes!(
            "../../../docker/build-app-cli/provenance.schema.json"
        ))
        .unwrap();
        let validator = jsonschema::draft202012::new(&schema).unwrap();
        for (path, values) in [
            ("/schema-version", vec![json!(0), json!(2)]),
            ("/platform/provenance-protocol", vec![json!(0), json!(2)]),
            (
                "/caller/app-repo-id",
                vec![
                    json!("0"),
                    json!("01"),
                    json!("-1"),
                    json!("x"),
                    json!("1".repeat(21)),
                ],
            ),
            (
                "/caller/source-revision",
                vec![
                    json!("0".repeat(40)),
                    json!("A".repeat(40)),
                    json!("1".repeat(39)),
                    json!("1".repeat(41)),
                ],
            ),
            (
                "/caller/workspace-id",
                vec![
                    json!("sha256:".to_owned() + &"0".repeat(64)),
                    json!("sha256:".to_owned() + &"A".repeat(64)),
                    json!("sha256:1"),
                ],
            ),
            (
                "/platform/platform-id",
                vec![
                    json!("sha256:".to_owned() + &"0".repeat(64)),
                    json!("sha256:1"),
                ],
            ),
            (
                "/platform/container-ref",
                vec![
                    json!("ghcr.io/attacker/image@sha256:".to_owned() + &"3".repeat(64)),
                    json!("ghcr.io/stackpop/edgezero-build-app-cli:v1"),
                ],
            ),
            (
                "/caller/app-cli-bin",
                vec![
                    json!(""),
                    json!("x".repeat(256)),
                    json!("a/b"),
                    json!("a\\b"),
                    json!("a\n"),
                    json!("a\u{85}"),
                ],
            ),
            ("/caller/app-cli-package", vec![json!(""), json!("a/b")]),
            ("/app-cli-version", vec![json!(""), json!("a/b")]),
            ("/binary-size", vec![json!(0), json!(536870913u64)]),
            (
                "/binary-sha256",
                vec![
                    json!("sha256:".to_owned() + &"0".repeat(64)),
                    json!("sha256:1"),
                ],
            ),
            ("/abi/machine", vec![json!("aarch64")]),
            ("/abi/interpreter", vec![json!("/wrong/loader")]),
            (
                "/abi/needed",
                vec![
                    json!([""]),
                    json!(["x".repeat(256)]),
                    json!(["$LIB"]),
                    json!(["a/b"]),
                    json!(["a\\b"]),
                    json!(["a\u{7f}"]),
                ],
            ),
        ] {
            for value in values {
                let candidate: Value =
                    serde_json::from_slice(&changed(DYNAMIC, path, value.clone())).unwrap();
                assert!(
                    !validator.is_valid(&candidate),
                    "schema accepted {path}={value}"
                );
            }
        }
        let multibyte = changed(DYNAMIC, "/app-cli-version", json!("\u{e9}".repeat(128)));
        assert!(validator.is_valid(&serde_json::from_slice::<Value>(&multibyte).unwrap()));
        assert!(Metadata::parse(&multibyte).is_err());
    }

    #[test]
    fn invalid_wire_fixtures_remain_invalid() {
        let expected = include_bytes!(
            "../../../docker/build-app-cli/fixtures/provenance/invalid/expected-duplicate.json"
        );
        let metadata = include_bytes!(
            "../../../docker/build-app-cli/fixtures/provenance/invalid/meta-missing-interpreter.json"
        );
        assert!(
            Expected::parse(expected)
                .unwrap_err()
                .contains("duplicate field")
        );
        assert!(
            Metadata::parse(metadata)
                .unwrap_err()
                .contains("missing field `interpreter`")
        );
    }
}
