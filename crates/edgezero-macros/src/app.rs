use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use std::env;
use std::fs;
use std::path::PathBuf;
use syn::parse::{Parse, ParseStream};
use syn::{parse_macro_input, Ident, LitStr, Token};
use validator::Validate as _;

// Many manifest fields exist for downstream consumers (CLI, runtime
// adapters, etc.) but are unused inside the proc-macro itself, which only
// reads enough of the structure to generate routing. Allow `dead_code` so
// those fields don't trip warnings just because the macro doesn't touch them.
#[allow(
    dead_code,
    reason = "macro-side reads only the routing-relevant fields"
)]
mod manifest_definitions {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../edgezero-core/src/manifest.rs"
    ));
}
use manifest_definitions::{Manifest, DEFAULT_CONFIG_STORE_NAME};

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

    let app_ident = args
        .app_ident
        .unwrap_or_else(|| Ident::new("App", Span::call_site()));
    let app_name = manifest
        .app
        .name
        .clone()
        .unwrap_or_else(|| "EdgeZero App".to_owned());
    let app_name_lit = LitStr::new(&app_name, Span::call_site());

    let middleware_tokens = build_middleware_tokens(&manifest);
    let route_tokens = build_route_tokens(&manifest);
    let config_store_tokens = build_config_store_tokens(&manifest);

    let output = quote! {
        pub struct #app_ident;

        impl edgezero_core::app::Hooks for #app_ident {
            fn routes() -> edgezero_core::router::RouterService {
                build_router()
            }

            fn name() -> &'static str {
                #app_name_lit
            }

            #config_store_tokens
        }

        pub fn build_router() -> edgezero_core::router::RouterService {
            let mut builder = edgezero_core::router::RouterService::builder();
            #(#middleware_tokens)*
            #(#route_tokens)*
            builder.build()
        }
    };

    output.into()
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

fn build_route_tokens(manifest: &Manifest) -> Vec<TokenStream2> {
    manifest
        .triggers
        .http
        .iter()
        .filter_map(|trigger| {
            let handler = trigger.handler.as_deref()?;
            let handler_path = parse_handler_path(handler);
            let path_lit = LitStr::new(&trigger.path, Span::call_site());

            let methods = trigger.methods();

            let mut tokens = Vec::new();
            for method in methods {
                let route_tokens = route_for_method(method, &path_lit, &handler_path);
                tokens.push(route_tokens);
            }
            Some(tokens)
        })
        .flatten()
        .collect()
}

fn build_middleware_tokens(manifest: &Manifest) -> Vec<TokenStream2> {
    manifest
        .app
        .middleware
        .iter()
        .map(|middleware| {
            let path = parse_handler_path(middleware);
            quote! {
                builder = builder.middleware(#path);
            }
        })
        .collect()
}

fn build_config_store_tokens(manifest: &Manifest) -> TokenStream2 {
    let Some(config) = manifest.stores.config.as_ref() else {
        return quote! {};
    };

    let fallback_name = config.name.as_deref().unwrap_or(DEFAULT_CONFIG_STORE_NAME);
    let fallback_name_lit = LitStr::new(fallback_name, Span::call_site());
    let override_entries: Vec<_> = config
        .adapters
        .iter()
        .map(|(adapter, cfg)| {
            let adapter_lit = LitStr::new(adapter, Span::call_site());
            let name_lit = LitStr::new(&cfg.name, Span::call_site());
            quote! {
                edgezero_core::app::ConfigStoreAdapterMetadata::new(#adapter_lit, #name_lit),
            }
        })
        .collect();

    quote! {
        fn config_store() -> Option<&'static edgezero_core::app::ConfigStoreMetadata> {
            static CONFIG_STORE: edgezero_core::app::ConfigStoreMetadata =
                edgezero_core::app::ConfigStoreMetadata::new(
                    #fallback_name_lit,
                    &[
                        #(#override_entries)*
                    ],
                );
            Some(&CONFIG_STORE)
        }
    }
}

/// Parses a handler reference like `crate::handlers::root` from `edgezero.toml`
/// into the `syn::ExprPath` that the generated router code references.
///
/// Called at proc-macro expansion time. If the user's manifest contains a
/// syntactically-invalid handler path, the only useful recovery is to halt
/// macro expansion with a clear message — there is no runtime to propagate
/// the error to. The panic is caught by `rustc` and surfaces as a normal
/// build failure with the file/line of the call site.
#[expect(
    clippy::panic,
    reason = "macro-expansion-time error: rustc surfaces the panic as a build failure"
)]
fn parse_handler_path(handler: &str) -> syn::ExprPath {
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
        .unwrap_or_else(|err| panic!("invalid handler path `{handler}`: {err}"))
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

struct AppArgs {
    path: LitStr,
    app_ident: Option<Ident>,
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
        Ok(Self { path, app_ident })
    }
}
