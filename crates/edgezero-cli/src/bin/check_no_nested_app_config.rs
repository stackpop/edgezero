//! `check_no_nested_app_config` — CI audit binary (spec §10.2.1).
//!
//! Detects `AppConfig`-derived structs used as fields inside other
//! `AppConfig`-derived structs. The check operates at the AST level
//! using a two-pass strategy so it catches nesting through common
//! container wrappers (`Option<T>`, `Vec<T>`, `Box<T>`, `Rc<T>`,
//! `Arc<T>`, tuples, arrays) and not just bare field types.
//!
//! ## Algorithm
//!
//! **Pass 1** — collect every struct identifier that carries
//! `#[derive(...AppConfig...)]` anywhere in the searched trees.
//!
//! **Pass 2** — for each `AppConfig`-derived struct, walk its fields.
//! For each field's type, recursively unwrap common containers
//! (`Option`, `Vec`, `Box`, `Rc`, `Arc`, tuples, arrays). At each
//! leaf check whether the type's final path segment names another
//! struct in the collected set. If so, emit a violation.
//!
//! Operating at the AST level means string literals that happen to
//! contain `AppConfig<AppConfig<…>>` (like in test doc-comments) will
//! never trigger a false positive.
//!
//! Exit codes:
//! - 0 — no violations found.
//! - 1 — one or more violations; violation lines written to stdout.
//! - 2 — one or more files could not be parsed; errors on stderr.
//!
//! Enabled only behind the `nested-app-config-check` feature so that the
//! normal workspace build does not pull in `syn` / `walkdir` / `proc-macro2`.

#![cfg(feature = "nested-app-config-check")]
// This is a CLI diagnostic binary; printing to stdout/stderr is its purpose.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI diagnostic binary — stdout/stderr output is intentional"
)]
// Free helpers (`struct_derives_app_config`, `type_contains_app_config_struct`,
// `rs_files_in`) are grouped with the pass they belong to, so they sit
// between the visitor `impl` blocks rather than below them. Reads better than
// hoisting every free fn to the bottom of the file.
#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "items are grouped by pass (collector pass / nesting pass / type-unwrap helpers / entry point), not by item kind"
)]

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::result::Result;
use std::string::ToString;

use proc_macro2::{Ident, Span};
use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::{visit, GenericArgument, PathArguments, Token, Type};
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Pass 1: collect struct identifiers that derive AppConfig
// ---------------------------------------------------------------------------

struct AppConfigStructCollector {
    app_config_structs: HashSet<String>,
}

// ---------------------------------------------------------------------------
// Pass 2: detect fields that reference another AppConfig-derived struct
// ---------------------------------------------------------------------------

struct NestedAppConfigVisitor<'src, 'set> {
    app_config_structs: &'set HashSet<String>,
    parse_errors: usize,
    source_path: &'src Path,
    violations: usize,
}

impl AppConfigStructCollector {
    fn new() -> Self {
        Self {
            app_config_structs: HashSet::new(),
        }
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "syn::visit::Visit has ~200 default methods; we only override visit_item_struct"
)]
impl<'ast> Visit<'ast> for AppConfigStructCollector {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        if struct_derives_app_config(i) {
            self.app_config_structs.insert(i.ident.to_string());
        }
        visit::visit_item_struct(self, i);
    }
}

/// Returns `true` when the struct has a `#[derive(…AppConfig…)]` attribute.
fn struct_derives_app_config(item: &syn::ItemStruct) -> bool {
    for attr in &item.attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        // Parse the derive list as a comma-separated sequence of paths.
        // We look for any path whose final segment is `AppConfig`.
        let found = attr
            .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
            .is_ok_and(|paths| {
                paths.iter().any(|path| {
                    path.segments
                        .last()
                        .is_some_and(|seg| seg.ident == "AppConfig")
                })
            });
        if found {
            return true;
        }
    }
    false
}

impl<'src, 'set> NestedAppConfigVisitor<'src, 'set> {
    fn new(source_path: &'src Path, app_config_structs: &'set HashSet<String>) -> Self {
        Self {
            app_config_structs,
            parse_errors: 0,
            source_path,
            violations: 0,
        }
    }

    fn report(&mut self, span: Span, outer: &str, field_name: &str, inner: &str) {
        let lc = span.start();
        println!(
            "{}:{}:{}: nested AppConfig: struct `{outer}` field `{field_name}` \
             references AppConfig-derived struct `{inner}`",
            self.source_path.display(),
            lc.line,
            lc.column.saturating_add(1),
        );
        self.violations = self.violations.saturating_add(1);
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "syn::visit::Visit has ~200 default methods; we only override visit_item_struct"
)]
impl<'ast> Visit<'ast> for NestedAppConfigVisitor<'_, '_> {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        // Only inspect structs that themselves derive AppConfig.
        if !struct_derives_app_config(i) {
            visit::visit_item_struct(self, i);
            return;
        }
        let outer_name = i.ident.to_string();
        for field in &i.fields {
            let field_name = field
                .ident
                .as_ref()
                .map_or_else(|| "<unnamed>".to_owned(), ToString::to_string);
            if let Some(inner_name) =
                type_contains_app_config_struct(&field.ty, self.app_config_structs)
            {
                let span = field
                    .ident
                    .as_ref()
                    .map_or_else(Span::call_site, Ident::span);
                self.report(span, &outer_name, &field_name, &inner_name);
            }
        }
        visit::visit_item_struct(self, i);
    }
}

// ---------------------------------------------------------------------------
// Type-unwrapping helpers
// ---------------------------------------------------------------------------

/// Recursively unwrap common container types (`Option<T>`, `Vec<T>`,
/// `Box<T>`, `Rc<T>`, `Arc<T>`, tuples, arrays) and return the name of
/// the first leaf path segment that is in `app_config_structs`, or `None`.
///
/// The catch-all wildcard is intentional: `syn` may add `Type` variants in
/// future minor releases; new variants that don't involve a named path cannot
/// contain an `AppConfig`-derived reference, so `None` is the correct
/// forward-compatible answer.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "syn may add Type variants; forward-compat fallback returns None"
)]
fn type_contains_app_config_struct(ty: &Type, set: &HashSet<String>) -> Option<String> {
    match ty {
        Type::Path(tp) => {
            let last = tp.path.segments.last()?;
            let ident = last.ident.to_string();
            // Transparent single-argument wrappers: unwrap and recurse.
            if matches!(ident.as_str(), "Option" | "Vec" | "Box" | "Rc" | "Arc") {
                if let PathArguments::AngleBracketed(ab) = &last.arguments {
                    for arg in &ab.args {
                        if let GenericArgument::Type(inner) = arg {
                            if let Some(found) = type_contains_app_config_struct(inner, set) {
                                return Some(found);
                            }
                        }
                    }
                }
                return None;
            }
            // Leaf: is it an AppConfig-derived struct?
            if set.contains(&ident) {
                return Some(ident);
            }
            None
        }
        Type::Array(ta) => type_contains_app_config_struct(&ta.elem, set),
        Type::Paren(tp) => type_contains_app_config_struct(&tp.elem, set),
        Type::Reference(tr) => type_contains_app_config_struct(&tr.elem, set),
        Type::Slice(ts) => type_contains_app_config_struct(&ts.elem, set),
        Type::Tuple(tt) => tt
            .elems
            .iter()
            .find_map(|inner| type_contains_app_config_struct(inner, set)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// File walker
// ---------------------------------------------------------------------------

fn collect_app_config_structs(path: &Path, set: &mut HashSet<String>, parse_errors: &mut usize) {
    let source = match fs::read_to_string(path) {
        Ok(src) => src,
        Err(err) => {
            eprintln!("{}: read error: {err}", path.display());
            *parse_errors = parse_errors.saturating_add(1);
            return;
        }
    };
    let file = match syn::parse_file(&source) {
        Ok(ff) => ff,
        Err(err) => {
            eprintln!("{}: parse error: {err}", path.display());
            *parse_errors = parse_errors.saturating_add(1);
            return;
        }
    };
    let mut collector = AppConfigStructCollector::new();
    collector.visit_file(&file);
    set.extend(collector.app_config_structs);
}

fn check_file(
    path: &Path,
    app_config_structs: &HashSet<String>,
    violations: &mut usize,
    parse_errors: &mut usize,
) {
    let source = match fs::read_to_string(path) {
        Ok(src) => src,
        Err(err) => {
            eprintln!("{}: read error: {err}", path.display());
            *parse_errors = parse_errors.saturating_add(1);
            return;
        }
    };
    let file = match syn::parse_file(&source) {
        Ok(ff) => ff,
        Err(err) => {
            eprintln!("{}: parse error: {err}", path.display());
            *parse_errors = parse_errors.saturating_add(1);
            return;
        }
    };
    let mut visitor = NestedAppConfigVisitor::new(path, app_config_structs);
    visitor.visit_file(&file);
    *violations = violations.saturating_add(visitor.violations);
    *parse_errors = parse_errors.saturating_add(visitor.parse_errors);
}

fn rs_files_in(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            let ep = entry.path();
            // Skip build artefacts.
            if ep.components().any(|cc| cc.as_os_str() == "target") {
                return false;
            }
            ep.extension().and_then(|ex| ex.to_str()) == Some("rs")
        })
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let roots: Vec<&Path> = if args.is_empty() {
        vec![Path::new(".")]
    } else {
        args.iter().map(|ss| Path::new(ss.as_str())).collect()
    };

    let mut parse_errors: usize = 0;

    // Pass 1: collect all AppConfig-derived struct names across the entire tree.
    let mut app_config_structs: HashSet<String> = HashSet::new();
    for root in &roots {
        for path in rs_files_in(root) {
            collect_app_config_structs(&path, &mut app_config_structs, &mut parse_errors);
        }
    }

    // Pass 2: check for fields that reference another AppConfig-derived struct.
    let mut violations: usize = 0;
    for root in &roots {
        for path in rs_files_in(root) {
            check_file(
                &path,
                &app_config_structs,
                &mut violations,
                &mut parse_errors,
            );
        }
    }

    if violations > 0 {
        eprintln!(
            "\n{violations} nested-AppConfig violation(s). \
             A struct with #[derive(AppConfig)] must not contain fields whose \
             type resolves to another #[derive(AppConfig)] struct, even through \
             Option/Vec/Box wrappers (spec \u{00a7}3.3)."
        );
        process::exit(1);
    }
    if parse_errors > 0 {
        process::exit(2);
    }

    println!("check_no_nested_app_config: OK");
}
