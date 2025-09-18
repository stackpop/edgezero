use handlebars::Handlebars;

pub fn register_templates(hbs: &mut Handlebars) {
    // Root
    hbs.register_template_string(
        "root_Cargo_toml",
        include_str!("templates/root/Cargo.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "root_README_md",
        include_str!("templates/root/README.md.hbs"),
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
    // Fastly
    hbs.register_template_string(
        "fastly_Cargo_toml",
        include_str!("templates/fastly/Cargo.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "fastly_src_main_rs",
        include_str!("templates/fastly/src/main.rs.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "fastly_cargo_config_toml",
        include_str!("templates/fastly/.cargo/config.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "fastly_fastly_toml",
        include_str!("templates/fastly/fastly.toml.hbs"),
    )
    .unwrap();
    // Cloudflare
    hbs.register_template_string(
        "cf_Cargo_toml",
        include_str!("templates/cloudflare/Cargo.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "cf_src_main_rs",
        include_str!("templates/cloudflare/src/main.rs.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "cf_cargo_config_toml",
        include_str!("templates/cloudflare/.cargo/config.toml.hbs"),
    )
    .unwrap();
    hbs.register_template_string(
        "cf_wrangler_toml",
        include_str!("templates/cloudflare/wrangler.toml.hbs"),
    )
    .unwrap();
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
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
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
        "anyedge-app".to_string()
    } else {
        out
    }
}

pub fn resolve_dep_line(
    from_dir: &std::path::Path,
    repo_root: &std::path::Path,
    repo_rel_crate: &str,
    fallback: &str,
) -> String {
    let candidate = repo_root.join(repo_rel_crate);
    if candidate.exists() {
        if let Some(rel) = relative_to(from_dir, repo_root) {
            let dep_path = std::path::Path::new(&rel).join(repo_rel_crate);
            // For fastly/cloudflare crates we still want features; detect by crate name
            let cname = crate_name_from_repo_path(repo_rel_crate);
            let features = if cname == "anyedge-fastly" {
                " , features = [\"fastly\"]"
            } else if cname == "anyedge-cloudflare" {
                " , features = [\"cloudflare\"]"
            } else {
                ""
            };
            return format!(
                "{} = {{ path = \"{}\"{} }}",
                cname,
                dep_path.display(),
                features
            );
        }
    }
    fallback.to_string()
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
