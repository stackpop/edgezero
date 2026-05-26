use edgezero_adapter::scaffold;
use handlebars::Handlebars;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub struct ResolvedDependency {
    pub crate_line: String,
    pub name: String,
    pub workspace_line: String,
}

/// Errors produced while scaffolding files for a generated project.
#[derive(Debug, Error)]
pub enum ScaffoldError {
    /// Failed to read or write a path on disk while emitting a template.
    #[error("scaffold io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The Handlebars renderer rejected the template or its data.
    #[error("template '{name}' failed to render: {message}")]
    Render { message: String, name: String },
}

impl ScaffoldError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        ScaffoldError::Io {
            path: path.into(),
            source,
        }
    }
}

fn crate_name_from_repo_path(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

/// Registers all compile-time-embedded templates.
///
/// Each `register_template_string` call uses `.expect(..)` because the inputs
/// are static strings via `include_str!` — failure can only happen if the
/// template source itself has invalid Handlebars syntax, which is a
/// build-time programmer error caught the moment the binary is run.
#[expect(
    clippy::expect_used,
    reason = "compile-time-embedded templates: parse failure is a build bug"
)]
pub fn register_templates(hbs: &mut Handlebars) {
    // Root
    hbs.register_template_string(
        "root_Cargo_toml",
        include_str!("templates/root/Cargo.toml.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "root_edgezero_toml",
        include_str!("templates/root/edgezero.toml.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "root_README_md",
        include_str!("templates/root/README.md.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "root_gitignore",
        include_str!("templates/root/gitignore.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "root_clippy_toml",
        include_str!("templates/root/clippy.toml.hbs"),
    )
    .expect("compiled-in template is valid");
    // Core
    hbs.register_template_string(
        "core_Cargo_toml",
        include_str!("templates/core/Cargo.toml.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "core_src_lib_rs",
        include_str!("templates/core/src/lib.rs.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "core_src_handlers_rs",
        include_str!("templates/core/src/handlers.rs.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "core_src_config_rs",
        include_str!("templates/core/src/config.rs.hbs"),
    )
    .expect("compiled-in template is valid");
    // App-config (`<name>.toml`)
    hbs.register_template_string("app_name_toml", include_str!("templates/app/name.toml.hbs"))
        .expect("compiled-in template is valid");
    // CLI
    hbs.register_template_string(
        "cli_Cargo_toml",
        include_str!("templates/cli/Cargo.toml.hbs"),
    )
    .expect("compiled-in template is valid");
    hbs.register_template_string(
        "cli_src_main_rs",
        include_str!("templates/cli/src/main.rs.hbs"),
    )
    .expect("compiled-in template is valid");
    // Adapter-specific templates
    for adapter in scaffold::registered_blueprints() {
        for template in adapter.template_registrations {
            hbs.register_template_string(template.name, template.contents)
                .expect("register adapter template");
        }
    }
}

pub fn relative_to(from: &Path, to: &Path) -> Option<String> {
    let from_abs = fs::canonicalize(from).ok()?;
    let to_abs = fs::canonicalize(to).ok()?;
    let suffix = from_abs.strip_prefix(&to_abs).ok()?;
    let depth = suffix.components().count();
    if depth == 0 {
        return Some(".".into());
    }
    let mut ups = String::new();
    for _ in 0..depth {
        if !ups.is_empty() {
            ups.push('/');
        }
        ups.push_str("..");
    }
    Some(ups)
}

pub fn resolve_dep_line(
    workspace_dir: &Path,
    repo_root: &Path,
    repo_rel_crate: &str,
    fallback: &str,
    features: &[&str],
) -> ResolvedDependency {
    let crate_name = crate_name_from_repo_path(repo_rel_crate).to_owned();
    let candidate = repo_root.join(repo_rel_crate);
    let workspace_line = if candidate.exists() {
        if let Some(rel) = relative_to(workspace_dir, repo_root) {
            let dep_path = Path::new(&rel).join(repo_rel_crate);
            format!("{} = {{ path = \"{}\" }}", crate_name, dep_path.display())
        } else if let Ok(absolute) = fs::canonicalize(&candidate) {
            // The output directory is outside the edgezero checkout, so a
            // relative path cannot be expressed cleanly. Depend on the local
            // crate by absolute path rather than falling back to Git.
            format!("{} = {{ path = \"{}\" }}", crate_name, absolute.display())
        } else {
            fallback.to_owned()
        }
    } else {
        fallback.to_owned()
    };

    let feature_fragment = if features.is_empty() {
        String::new()
    } else {
        let joined = features
            .iter()
            .map(|feat| format!("\"{feat}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(", features = [{joined}]")
    };
    let crate_line = format!("{crate_name} = {{ workspace = true{feature_fragment} }}");

    ResolvedDependency {
        crate_line,
        name: crate_name,
        workspace_line,
    }
}

/// Normalise an arbitrary project name into a valid Cargo package name.
///
/// ASCII letters are lower-cased (so `MyApp` becomes `myapp`, not the
/// invalid `-y-pp`); `-` and `_` are kept; every other character collapses
/// to a single `-`. Leading separators are dropped and trailing separators
/// trimmed, so the result never starts or ends with `-`/`_`. A digit-leading
/// result is prefixed with `_`, and an empty result falls back to
/// `edgezero-app`.
pub fn sanitize_crate_name(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            // `-`, `_`, and every other invalid character collapse to a
            // single separator; leading and doubled separators are dropped.
            let separator = if ch == '_' { '_' } else { '-' };
            if !out.is_empty() && !out.ends_with(['-', '_']) {
                out.push(separator);
            }
        }
    }
    while out.ends_with(['-', '_']) {
        out.pop();
    }
    if out.is_empty() {
        "edgezero-app".to_owned()
    } else if out.starts_with(|ch: char| ch.is_ascii_digit()) {
        format!("_{out}")
    } else {
        out
    }
}

/// # Errors
/// Returns [`ScaffoldError::Io`] if the parent directory cannot be created
/// or the rendered template cannot be written; [`ScaffoldError::Render`] if
/// Handlebars rejects the template or its data.
pub fn write_tmpl(
    hbs: &handlebars::Handlebars,
    name: &str,
    data: &serde_json::Value,
    out_path: &Path,
) -> Result<(), ScaffoldError> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|err| ScaffoldError::io(parent, err))?;
    }
    let rendered = hbs
        .render(name, data)
        .map_err(|err| ScaffoldError::Render {
            message: err.to_string(),
            name: name.to_owned(),
        })?;
    fs::write(out_path, rendered).map_err(|err| ScaffoldError::io(out_path, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use handlebars::Handlebars;

    #[test]
    fn register_templates_registers_all_known_templates() {
        let mut hbs = Handlebars::new();
        register_templates(&mut hbs);

        for name in [
            "root_Cargo_toml",
            "root_edgezero_toml",
            "root_README_md",
            "root_gitignore",
            "root_clippy_toml",
            "core_Cargo_toml",
            "core_src_lib_rs",
            "core_src_handlers_rs",
            "core_src_config_rs",
            "cli_Cargo_toml",
            "cli_src_main_rs",
            "app_name_toml",
        ] {
            assert!(hbs.has_template(name), "missing template {name}");
        }

        for blueprint in scaffold::registered_blueprints() {
            for template in blueprint.template_registrations {
                assert!(
                    hbs.has_template(template.name),
                    "adapter template {} not registered",
                    template.name
                );
            }
        }
    }

    #[test]
    fn sanitize_crate_name_lowercases_mixed_case() {
        // Regression: uppercase letters were mangled to `-`, producing the
        // invalid package name `-y-pp` for `MyApp`.
        assert_eq!(sanitize_crate_name("MyApp"), "myapp");
        assert_eq!(sanitize_crate_name("My App"), "my-app");
    }

    #[test]
    fn sanitize_crate_name_keeps_valid_separators() {
        assert_eq!(sanitize_crate_name("my-edge-app"), "my-edge-app");
        assert_eq!(sanitize_crate_name("my_app"), "my_app");
    }

    #[test]
    fn sanitize_crate_name_trims_and_collapses_separators() {
        assert_eq!(sanitize_crate_name("  spaced  "), "spaced");
        assert_eq!(sanitize_crate_name("a@@@b"), "a-b");
        assert_eq!(sanitize_crate_name("-leading-"), "leading");
    }

    #[test]
    fn sanitize_crate_name_handles_digit_leading_and_empty() {
        assert_eq!(sanitize_crate_name("123app"), "_123app");
        assert_eq!(sanitize_crate_name("!!!"), "edgezero-app");
    }
}
