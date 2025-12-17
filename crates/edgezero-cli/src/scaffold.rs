use edgezero_adapter::scaffold;
use handlebars::Handlebars;

pub fn register_templates(hbs: &mut Handlebars) {
    // Root
    hbs.register_template_string(
        "root_Cargo_toml",
        include_str!("templates/root/Cargo.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "root_edgezero_toml",
        include_str!("templates/root/edgezero.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "root_README_md",
        include_str!("templates/root/README.md.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "root_gitignore",
        include_str!("templates/root/gitignore.hbs"),
    )
    .unwrap();
    // Core
    hbs.register_template_string(
        "core_Cargo_toml",
        include_str!("templates/core/Cargo.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "core_src_lib_rs",
        include_str!("templates/core/src/lib.rs.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "core_src_handlers_rs",
        include_str!("templates/core/src/handlers.rs.hbs"),
    )
    .unwrap();
    // Adapter-specific templates
    for adapter in scaffold::registered_blueprints() {
        for template in adapter.template_registrations {
            hbs.register_template_string(template.name, template.contents)
                .expect("register adapter template");
        }
    }
}

pub fn write_tmpl(
    hbs: &handlebars::Handlebars,
    name: &str,
    data: &serde_json::Value,
    out_path: &std::path::Path,
) -> std::io::Result<()> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = hbs
        .render(name, data)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(out_path, rendered)
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
        "edgezero-app".to_string()
    } else {
        out
    }
}

pub struct ResolvedDependency {
    pub name: String,
    pub workspace_line: String,
    pub crate_line: String,
}

pub fn resolve_dep_line(
    workspace_dir: &std::path::Path,
    repo_root: &std::path::Path,
    repo_rel_crate: &str,
    fallback: &str,
    features: &[&str],
) -> ResolvedDependency {
    let crate_name = crate_name_from_repo_path(repo_rel_crate).to_string();
    let candidate = repo_root.join(repo_rel_crate);
    let workspace_line = if candidate.exists() {
        if let Some(rel) = relative_to(workspace_dir, repo_root) {
            let dep_path = std::path::Path::new(&rel).join(repo_rel_crate);
            format!("{} = {{ path = \"{}\" }}", crate_name, dep_path.display())
        } else {
            fallback.to_string()
        }
    } else {
        fallback.to_string()
    };

    let feature_fragment = if features.is_empty() {
        String::new()
    } else {
        let joined = features
            .iter()
            .map(|f| format!("\"{}\"", f))
            .collect::<Vec<_>>()
            .join(", ");
        format!(", features = [{}]", joined)
    };
    let crate_line = format!(
        "{} = {{ workspace = true{} }}",
        crate_name, feature_fragment
    );

    ResolvedDependency {
        name: crate_name,
        workspace_line,
        crate_line,
    }
}

fn crate_name_from_repo_path(p: &str) -> &str {
    std::path::Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
}

pub fn relative_to(from: &std::path::Path, to: &std::path::Path) -> Option<String> {
    let from_abs = std::fs::canonicalize(from).ok()?;
    let to_abs = std::fs::canonicalize(to).ok()?;
    let suffix = from_abs.strip_prefix(&to_abs).ok()?;
    let depth = suffix.components().count();
    if depth == 0 {
        return Some(".".into());
    }
    let mut ups = String::new();
    for i in 0..depth {
        let _ = i;
        if !ups.is_empty() {
            ups.push('/');
        }
        ups.push_str("..");
    }
    Some(ups)
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
