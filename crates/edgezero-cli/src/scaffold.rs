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

fn crate_name_from_repo_path(p: &str) -> &str {
    Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
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
            .map(|f| format!("\"{f}\""))
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

pub fn sanitize_crate_name(input: &str) -> String {
    let mut out = String::new();
    for (i, ch) in input.chars().enumerate() {
        let valid = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_';
        if valid {
            if i == 0 && ch.is_ascii_digit() {
                out.push('_');
            }
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "edgezero-app".to_owned()
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
        fs::create_dir_all(parent).map_err(|e| ScaffoldError::io(parent, e))?;
    }
    let rendered = hbs.render(name, data).map_err(|e| ScaffoldError::Render {
        message: e.to_string(),
        name: name.to_owned(),
    })?;
    fs::write(out_path, rendered).map_err(|e| ScaffoldError::io(out_path, e))
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
}
