use crate::manifest_definitions::{Manifest, StoreDeclaration};
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use std::env;
use std::fs;
use std::path::PathBuf;
use syn::parse::{Parse, ParseStream};
use syn::{parse_macro_input, Ident, LitStr, Token};
use validator::Validate as _;

struct AppArgs {
    app_ident: Option<Ident>,
    path: LitStr,
}

impl Parse for AppArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;
        let app_ident = if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            Some(input.parse::<Ident>()?)
        } else {
            None
        };
        if !input.is_empty() {
            return Err(input.error("unexpected tokens after app! macro arguments"));
        }
        Ok(Self { app_ident, path })
    }
}

/// Render a `StoreMetadata { default, ids }` literal for one `[stores.<kind>]`
/// declaration, or `None` when the declaration is absent.
fn store_metadata_tokens(maybe_declaration: Option<&StoreDeclaration>) -> TokenStream2 {
    let Some(declaration) = maybe_declaration else {
        return quote! { None };
    };
    let default_lit = LitStr::new(declaration.default_id(), Span::call_site());
    let id_lits = declaration
        .ids
        .iter()
        .map(|id| LitStr::new(id, Span::call_site()));
    quote! {
        Some(edgezero_core::app::StoreMetadata {
            default: #default_lit,
            ids: &[#(#id_lits),*],
        })
    }
}

/// Codegen the `Hooks::stores()` impl from the portable `[stores.*]` schema.
fn build_stores_tokens(manifest: &Manifest) -> TokenStream2 {
    let config = store_metadata_tokens(manifest.stores.config.as_ref());
    let kv = store_metadata_tokens(manifest.stores.kv.as_ref());
    let secrets = store_metadata_tokens(manifest.stores.secrets.as_ref());
    quote! {
        fn stores() -> edgezero_core::app::StoresMetadata {
            edgezero_core::app::StoresMetadata {
                config: #config,
                kv: #kv,
                secrets: #secrets,
            }
        }
    }
}

fn build_middleware_tokens(manifest: &Manifest) -> Result<Vec<TokenStream2>, String> {
    manifest
        .app
        .middleware
        .iter()
        .map(|middleware| {
            let path = parse_handler_path(middleware)?;
            Ok(quote! {
                builder = builder.middleware(#path);
            })
        })
        .collect()
}

fn build_route_tokens(manifest: &Manifest) -> Result<Vec<TokenStream2>, String> {
    let mut tokens = Vec::new();
    for trigger in &manifest.triggers.http {
        let Some(handler) = trigger.handler.as_deref() else {
            continue;
        };
        let handler_path = parse_handler_path(handler)?;
        let path_lit = LitStr::new(&trigger.path, Span::call_site());

        for method in trigger.methods() {
            tokens.push(route_for_method(method, &path_lit, &handler_path));
        }
    }
    Ok(tokens)
}

pub fn expand_app(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as AppArgs);

    let manifest_path = resolve_manifest_path(args.path.value());
    let manifest_source = match fs::read_to_string(&manifest_path) {
        Ok(source) => source,
        Err(err) => {
            let msg = format!("failed to read {}: {err}", manifest_path.display());
            return quote!(compile_error!(#msg);).into();
        }
    };

    let mut manifest: Manifest = match toml::from_str(&manifest_source) {
        Ok(parsed) => parsed,
        Err(err) => {
            let msg = format!("failed to parse {}: {err}", manifest_path.display());
            return quote!(compile_error!(#msg);).into();
        }
    };
    if let Err(err) = manifest.validate() {
        let msg = format!("failed to validate {}: {err}", manifest_path.display());
        return quote!(compile_error!(#msg);).into();
    }
    manifest.finalize();

    let manifest_json = match serde_json::to_string(&manifest) {
        Ok(json) => json,
        Err(err) => {
            let msg = format!("failed to serialize manifest to JSON: {err}");
            return quote!(compile_error!(#msg);).into();
        }
    };
    let manifest_json_lit = LitStr::new(&manifest_json, Span::call_site());

    let app_ident = args
        .app_ident
        .unwrap_or_else(|| Ident::new("App", Span::call_site()));
    let app_name = manifest
        .app
        .name
        .clone()
        .unwrap_or_else(|| "EdgeZero App".to_owned());
    let app_name_lit = LitStr::new(&app_name, Span::call_site());

    let middleware_tokens = match build_middleware_tokens(&manifest) {
        Ok(tokens) => tokens,
        Err(msg) => return quote!(compile_error!(#msg);).into(),
    };
    let route_tokens = match build_route_tokens(&manifest) {
        Ok(tokens) => tokens,
        Err(msg) => return quote!(compile_error!(#msg);).into(),
    };
    let stores_tokens = build_stores_tokens(&manifest);

    let manifest_path_lit = LitStr::new(&manifest_path.to_string_lossy(), Span::call_site());

    // The emitted `Hooks` impl below explicitly defines `configure` and
    // `build_app` even though their bodies mirror the trait defaults. This is
    // required because `missing_trait_methods` (restriction = deny) forbids
    // relying on trait defaults in the impl. If `Hooks::configure` or
    // `Hooks::build_app` defaults change, update these emitted bodies to match.
    let output = quote! {
        // Force a rebuild when the manifest file changes (include_bytes tracks it as a build input).
        const _: &[u8] = include_bytes!(#manifest_path_lit);

        pub struct #app_ident;

        impl edgezero_core::app::Hooks for #app_ident {
            fn routes() -> edgezero_core::router::RouterService {
                build_router()
            }

            fn configure(_app: &mut edgezero_core::app::App) {}

            fn name() -> &'static str {
                #app_name_lit
            }

            #stores_tokens

            fn build_app() -> edgezero_core::app::App {
                let mut app = edgezero_core::app::App::with_name(Self::routes(), Self::name());
                Self::configure(&mut app);
                app
            }
        }

        pub fn build_router() -> edgezero_core::router::RouterService {
            let mut builder = edgezero_core::router::RouterService::builder();
            builder = builder.with_manifest_json(#manifest_json_lit);
            #(#middleware_tokens)*
            #(#route_tokens)*
            builder.build()
        }
    };

    output.into()
}

/// Parses a handler reference like `crate::handlers::root` from `edgezero.toml`
/// into the `syn::ExprPath` that the generated router code references.
///
/// Returns `Err(message)` when the manifest contains a syntactically-invalid
/// handler path. Callers propagate the message into a `compile_error!()` so
/// rustc surfaces it as a normal build failure with the file/line of the
/// `app!(...)` call site, instead of as a "proc-macro panicked".
fn parse_handler_path(handler: &str) -> Result<syn::ExprPath, String> {
    let mut handler_str = handler.trim().to_owned();
    if handler_str.starts_with("crate::")
        || handler_str.starts_with("self::")
        || handler_str.starts_with("super::")
    {
        // leave as is
    } else {
        let crate_name = env::var("CARGO_PKG_NAME")
            .map(|name| name.replace('-', "_"))
            .unwrap_or_default();
        if !crate_name.is_empty() && handler_str.starts_with(&format!("{crate_name}::")) {
            handler_str = format!(
                "crate::{}",
                handler_str
                    .get(crate_name.len().saturating_add(2)..)
                    .unwrap_or_default(),
            );
        }
    }

    syn::parse_str::<syn::ExprPath>(&handler_str)
        .map_err(|err| format!("invalid handler path `{handler}`: {err}"))
}

/// Resolves the manifest path passed to `app!(...)` against the
/// invoking crate's `CARGO_MANIFEST_DIR`.
///
/// `CARGO_MANIFEST_DIR` is unconditionally set by Cargo whenever a
/// proc-macro runs against a normal crate, so the lookup cannot fail in
/// practice. Treating it as fallible would require every caller of
/// `app!(...)` to handle an outcome that has never been observed and
/// cannot be triggered without bypassing Cargo entirely.
#[expect(
    clippy::expect_used,
    reason = "CARGO_MANIFEST_DIR is a Cargo invariant during macro expansion; \
              there is no realistic failure mode to propagate"
)]
fn resolve_manifest_path(relative: String) -> PathBuf {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR env var");
    PathBuf::from(manifest_dir).join(relative)
}

fn route_for_method(method: &str, path: &LitStr, handler: &syn::ExprPath) -> TokenStream2 {
    match method {
        "GET" => quote! { builder = builder.get(#path, #handler); },
        "POST" => quote! { builder = builder.post(#path, #handler); },
        "PUT" => quote! { builder = builder.put(#path, #handler); },
        "DELETE" => quote! { builder = builder.delete(#path, #handler); },
        _ => {
            let method_bytes = syn::LitByteStr::new(method.as_bytes(), Span::call_site());
            quote! {
                builder = builder.route(
                    #path,
                    edgezero_core::http::Method::from_bytes(#method_bytes)
                        .expect("invalid HTTP method in manifest"),
                    #handler,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_handler_path;

    #[test]
    fn parse_handler_path_accepts_absolute_crate_path() {
        let parsed =
            parse_handler_path("crate::handlers::root").expect("valid handler path should parse");
        let rendered = quote::quote!(#parsed).to_string();
        assert_eq!(rendered, "crate :: handlers :: root");
    }

    #[test]
    fn parse_handler_path_rejects_invalid_syntax_with_message() {
        let err = parse_handler_path("not a valid path!").expect_err("expected parse failure");
        assert!(
            err.contains("invalid handler path"),
            "error message should name the failure, got: {err}"
        );
        assert!(
            err.contains("not a valid path!"),
            "error message should echo the offending input, got: {err}"
        );
    }
}
