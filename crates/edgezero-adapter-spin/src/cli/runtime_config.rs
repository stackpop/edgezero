//! Parse Spin's `runtime-config.toml` to dispatch `config push --adapter
//! spin` to the right backend writer.
//!
//! Spin's runtime config is a separate file from `spin.toml`. It declares
//! `[key_value_store.<label>]` stanzas selecting a backend per KV label
//! (`type = "spin"` for the default `SQLite` backend, `type = "redis"`
//! for Redis, `type = "azure-cosmos"` for Azure, etc.). Without it `spin
//! up` errors with `unknown key_value_stores label X` for any
//! non-`default` label, so the file is part of every multi-store Spin
//! project's checkout — we only need to READ it, never edit or scaffold
//! it differently.
//!
//! The push dispatcher consults this file to decide which writer to use.
//! Anything we don't recognise is preserved as [`KeyValueBackend::Unknown`]
//! with the raw TOML table so the dispatcher can name the type in the
//! error message.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::Error as DeError;

/// Backend selected for one `[key_value_store.<label>]` stanza. The
/// variant tells the dispatcher which writer to invoke.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum KeyValueBackend {
    /// `type = "azure_cosmos"` — deferred; dispatcher points at the
    /// Azure backend's native CLI.
    AzureCosmos,
    /// `type = "redis"` — deferred to a follow-up PR; the dispatcher
    /// returns an error pointing at `redis-cli SET` against `url`.
    /// `url` is also pre-parsed for the upcoming redis writer.
    Redis { url: String },
    /// `type = "spin"` — local `SQLite` at the path Spin would use
    /// (`<runtime-config dir>/.spin/sqlite_key_value.db` by default,
    /// or the explicit `path` if set).
    Spin { path: Option<PathBuf> },
    /// `type = "<something else>"` — surface the type in the error so
    /// the operator knows we don't recognise it and can plan accordingly.
    Unknown { type_name: String },
}

impl<'de> Deserialize<'de> for KeyValueBackend {
    fn deserialize_in_place<D>(deserializer: D, place: &mut Self) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        *place = Self::deserialize(deserializer)?;
        Ok(())
    }

    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Land the raw table first, then dispatch on the `type` field.
        // Spin's runtime-config format requires a `type` discriminant
        // for every store; we treat its absence as Unknown.
        let table = toml::Table::deserialize(deserializer)?;
        // A field that is PRESENT but not a string is a malformed
        // runtime-config, not a silent fall-through to the default: e.g.
        // `path = 123` must surface an error rather than be dropped and
        // resolve to the default SQLite location (which would push config
        // to the wrong database). Absent fields keep their defaults.
        let str_field = |field: &str| -> Result<Option<&str>, D::Error> {
            match table.get(field) {
                None => Ok(None),
                Some(toml::Value::String(value)) => Ok(Some(value.as_str())),
                Some(_) => Err(DeError::custom(format!(
                    "`{field}` in a `[key_value_store]` stanza must be a string"
                ))),
            }
        };
        let type_name = str_field("type")?.unwrap_or("").to_owned();
        Ok(match type_name.as_str() {
            "spin" => Self::Spin {
                path: str_field("path")?.map(PathBuf::from),
            },
            "redis" => Self::Redis {
                url: str_field("url")?.unwrap_or("").to_owned(),
            },
            "azure_cosmos" => Self::AzureCosmos,
            other => Self::Unknown {
                type_name: other.to_owned(),
            },
        })
    }
}

/// Parsed `runtime-config.toml` — only the bits we need to dispatch
/// `config push`. Any other Spin runtime-config sections (`SQLite`,
/// secrets stores, etc.) are intentionally ignored.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ParsedRuntimeConfig {
    #[serde(default, rename = "key_value_store")]
    pub key_value_stores: BTreeMap<String, KeyValueBackend>,
}

/// Read and parse `runtime-config.toml` at `path`. Missing file is
/// treated as "no custom KV backends declared" — the dispatcher then
/// falls back to the default `SQLite` location (Spin's behaviour for
/// any label not declared here).
///
/// # Errors
/// Returns a human-readable error string if the file exists but is
/// malformed TOML or doesn't deserialise into the expected shape.
pub(crate) fn read(path: &Path) -> Result<ParsedRuntimeConfig, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(ParsedRuntimeConfig::default());
        }
        Err(err) => {
            return Err(format!(
                "failed to read runtime-config at `{}`: {err}",
                path.display()
            ));
        }
    };
    toml::from_str::<ParsedRuntimeConfig>(&contents).map_err(|err| {
        format!(
            "failed to parse runtime-config at `{}`: {err}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn missing_runtime_config_returns_empty() {
        let dir = tempdir().expect("tempdir");
        let parsed = read(&dir.path().join("absent.toml")).expect("missing file is fine");
        assert!(parsed.key_value_stores.is_empty());
    }

    #[test]
    fn spin_backend_parses_without_path() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(&path, "[key_value_store.app_config]\ntype = \"spin\"\n").expect("write");
        let parsed = read(&path).expect("parse");
        assert!(matches!(
            parsed.key_value_stores["app_config"],
            KeyValueBackend::Spin { path: None }
        ));
    }

    #[test]
    fn spin_backend_parses_with_explicit_path() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(
            &path,
            "[key_value_store.app_config]\ntype = \"spin\"\npath = \"/custom/kv.db\"\n",
        )
        .expect("write");
        let parsed = read(&path).expect("parse");
        let KeyValueBackend::Spin {
            path: Some(custom), ..
        } = &parsed.key_value_stores["app_config"]
        else {
            panic!(
                "expected Spin with custom path, got: {:?}",
                parsed.key_value_stores["app_config"]
            );
        };
        assert_eq!(custom, &PathBuf::from("/custom/kv.db"));
    }

    #[test]
    fn redis_backend_parses_url() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(
            &path,
            "[key_value_store.cache]\ntype = \"redis\"\nurl = \"redis://localhost:6379\"\n",
        )
        .expect("write");
        let parsed = read(&path).expect("parse");
        let KeyValueBackend::Redis { url } = &parsed.key_value_stores["cache"] else {
            panic!(
                "expected Redis, got: {:?}",
                parsed.key_value_stores["cache"]
            );
        };
        assert_eq!(url, "redis://localhost:6379");
    }

    #[test]
    fn unknown_backend_preserves_type_name() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(
            &path,
            "[key_value_store.future]\ntype = \"new-backend-spin-will-add-tomorrow\"\n",
        )
        .expect("write");
        let parsed = read(&path).expect("parse");
        let KeyValueBackend::Unknown { type_name } = &parsed.key_value_stores["future"] else {
            panic!(
                "expected Unknown, got: {:?}",
                parsed.key_value_stores["future"]
            );
        };
        assert_eq!(type_name, "new-backend-spin-will-add-tomorrow");
    }

    #[test]
    fn azure_cosmos_backend_parses() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(&path, "[key_value_store.global]\ntype = \"azure_cosmos\"\n").expect("write");
        let parsed = read(&path).expect("parse");
        assert!(matches!(
            parsed.key_value_stores["global"],
            KeyValueBackend::AzureCosmos
        ));
    }

    #[test]
    fn spin_backend_rejects_non_string_path() {
        // `path = 123` is malformed: silently defaulting to the standard
        // SQLite location would push config to the wrong database. It must
        // surface an error.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(
            &path,
            "[key_value_store.app_config]\ntype = \"spin\"\npath = 123\n",
        )
        .expect("write");
        let err = read(&path).expect_err("a non-string `path` must error, not silently default");
        assert!(
            err.contains("`path`") && err.contains("must be a string"),
            "error names the offending field: {err}"
        );
    }

    #[test]
    fn redis_backend_rejects_non_string_url() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(
            &path,
            "[key_value_store.cache]\ntype = \"redis\"\nurl = true\n",
        )
        .expect("write");
        let err = read(&path).expect_err("a non-string `url` must error");
        assert!(
            err.contains("`url`") && err.contains("must be a string"),
            "error names the offending field: {err}"
        );
    }

    #[test]
    fn malformed_toml_returns_named_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("runtime-config.toml");
        fs::write(&path, "[key_value_store.app_config\ntype = \"spin\"\n").expect("write");
        let err = read(&path).expect_err("malformed TOML must error");
        assert!(
            err.contains(&path.display().to_string()) && err.contains("failed to parse"),
            "error names the file + the failure: {err}"
        );
    }
}
