use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use std::env;
use std::fs;
use std::path::PathBuf;
use syn::parse::{Parse, ParseStream};
use syn::{parse_macro_input, Ident, LitStr, Token};

#[allow(dead_code)]
mod manifest_definitions {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../anyedge-core/src/manifest.rs"
    ));
}
use manifest_definitions::Manifest;

pub fn expand_app(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as AppArgs);

    let manifest_path = resolve_manifest_path(args.path.value());
    let manifest_source = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));

    let manifest: Manifest = toml::from_str(&manifest_source)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", manifest_path.display()));

    let app_ident = args
        .app_ident
        .unwrap_or_else(|| Ident::new("App", Span::call_site()));
    let app_name = manifest
        .app
        .name
        .clone()
        .unwrap_or_else(|| "AnyEdge App".to_string());
    let app_name_lit = LitStr::new(&app_name, Span::call_site());

    let middleware_tokens = build_middleware_tokens(&manifest);
    let route_tokens = build_route_tokens(&manifest);

    let output = quote! {
        pub struct #app_ident;

        impl anyedge_core::app::Hooks for #app_ident {
            fn routes() -> anyedge_core::router::RouterService {
                build_router()
            }

            fn name() -> &'static str {
                #app_name_lit
            }
        }

        pub fn build_router() -> anyedge_core::router::RouterService {
            let mut builder = anyedge_core::router::RouterService::builder();
            #(#middleware_tokens)*
            #(#route_tokens)*
            builder.build()
        }
    };

    output.into()
}

fn resolve_manifest_path(relative: String) -> PathBuf {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR env var");
    let mut path = PathBuf::from(manifest_dir);
    path.push(relative);
    path
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

fn parse_handler_path(handler: &str) -> syn::ExprPath {
    let mut handler_str = handler.trim().to_string();
    if handler_str.starts_with("crate::")
        || handler_str.starts_with("self::")
        || handler_str.starts_with("super::")
    {
        // leave as is
    } else {
        let crate_name = env::var("CARGO_PKG_NAME")
            .map(|name| name.replace('-', "_"))
            .unwrap_or_default();
        if !crate_name.is_empty() && handler_str.starts_with(&(crate_name.clone() + "::")) {
            handler_str = format!("crate::{}", &handler_str[crate_name.len() + 2..]);
        }
    }

    syn::parse_str::<syn::ExprPath>(&handler_str)
        .unwrap_or_else(|err| panic!("invalid handler path `{}`: {err}", handler))
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
                    anyedge_core::http::Method::from_bytes(#method_bytes)
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
