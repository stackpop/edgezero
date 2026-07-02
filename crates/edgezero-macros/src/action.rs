use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser as _;
use syn::punctuated::Punctuated;
use syn::{spanned::Spanned as _, Error, FnArg, ItemFn, Pat, PathArguments, Token, Type};

/// `(extract_stmts, arg_idents)` produced from a handler's argument list — the
/// `FromRequest` extraction statements and the idents passed to the inner fn.
type ArgExtractors = (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>);

pub fn expand_action(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_action_impl(&attr.into(), item.into()).into()
}

fn expand_action_impl(
    attr: &proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    // `#[action]` takes an optional atomic capability list, e.g.
    // `#[action(manifest)]` / `#[action(routes)]` / `#[action(manifest, routes)]`.
    // Each names an introspection payload the handler needs injected; a handler
    // that opts in is emitted as a capability-carrying struct (see below).
    let (manifest_cap, routes_cap) = match parse_action_params(attr) {
        Ok(caps) => caps,
        Err(err) => return err.to_compile_error(),
    };
    let is_capability_handler = manifest_cap || routes_cap;

    let func: ItemFn = match syn::parse2(item) {
        Ok(func) => func,
        Err(err) => return err.to_compile_error(),
    };

    if func.sig.asyncness.is_none() {
        return syn::Error::new(func.sig.span(), "#[action] functions must be async")
            .to_compile_error();
    }

    for input in &func.sig.inputs {
        if matches!(input, FnArg::Receiver(_)) {
            return syn::Error::new(input.span(), "#[action] does not support self receivers")
                .to_compile_error();
        }
    }

    let attrs = func.attrs.clone();
    let vis = func.vis.clone();
    let ident = func.sig.ident.clone();
    let inner_ident = format_ident!("__{}_inner", ident);

    let mut inner_fn = func.clone();
    inner_fn.sig.ident = inner_ident.clone();
    inner_fn.vis = syn::Visibility::Inherited;
    inner_fn.attrs.clear();
    // `#[action]` requires the user fn to be `async` so we can `.await` it
    // from the generated outer fn. Some handler bodies have no awaits of
    // their own — silence `clippy::unused_async` for those.
    inner_fn
        .attrs
        .push(syn::parse_quote!(#[allow(clippy::unused_async)]));

    if let Err(err) = normalize_request_context_patterns(&mut inner_fn) {
        return err.to_compile_error();
    }

    let (extract_stmts, arg_idents) = match build_arg_extractors(&func) {
        Ok(parts) => parts,
        Err(err) => return err.to_compile_error(),
    };

    let output = if is_capability_handler {
        // A fn can't carry per-handler data past type-erasure into
        // `Arc<dyn DynHandler>`, so an opt-in handler becomes a unit struct with
        // its own `DynHandler` impl whose `introspection_needs()` reports which
        // payloads the router must inject for its route.
        quote! {
            #inner_fn

            #(#attrs)*
            #[allow(non_camel_case_types)]
            #vis struct #ident;

            impl ::edgezero_core::handler::DynHandler for #ident {
                #[inline]
                fn call(
                    &self,
                    __ctx: ::edgezero_core::context::RequestContext,
                ) -> ::edgezero_core::http::HandlerFuture {
                    ::std::boxed::Box::pin(async move {
                        #(#extract_stmts)*
                        let result = #inner_ident(#(#arg_idents),*).await;
                        ::edgezero_core::responder::Responder::respond(result)
                    })
                }

                #[inline]
                fn introspection_needs(&self) -> ::edgezero_core::handler::IntrospectionNeeds {
                    ::edgezero_core::handler::IntrospectionNeeds {
                        manifest: #manifest_cap,
                        routes: #routes_cap,
                    }
                }
            }
        }
    } else {
        quote! {
            #inner_fn

            #(#attrs)*
            #vis async fn #ident(
                __ctx: ::edgezero_core::context::RequestContext,
            ) -> ::std::result::Result<::edgezero_core::http::Response, ::edgezero_core::error::EdgeError> {
                #(#extract_stmts)*
                let result = #inner_ident(#(#arg_idents),*).await;
                ::edgezero_core::responder::Responder::respond(result)
            }
        }
    };

    output
}

/// Parse the optional `#[action(...)]` capability list into
/// `(needs_manifest, needs_routes)`. Empty attr → `(false, false)`. Unknown
/// idents are a compile error. Extend the known set as new capabilities land.
fn parse_action_params(attr: &proc_macro2::TokenStream) -> Result<(bool, bool), Error> {
    if attr.is_empty() {
        return Ok((false, false));
    }
    let params = Punctuated::<syn::Ident, Token![,]>::parse_terminated.parse2(attr.clone())?;
    let mut manifest_cap = false;
    let mut routes_cap = false;
    for param in &params {
        if param == "manifest" {
            manifest_cap = true;
        } else if param == "routes" {
            routes_cap = true;
        } else {
            return Err(Error::new(
                param.span(),
                format!("unknown #[action] parameter `{param}`; supported: manifest, routes"),
            ));
        }
    }
    Ok((manifest_cap, routes_cap))
}

/// Build the per-argument extractor statements and the argument idents passed to
/// the inner fn. `RequestContext` arguments map to `__ctx`; every other argument
/// is extracted via `FromRequest`. Returns the `(extract_stmts, arg_idents)`
/// used by both the fn and struct codegen forms.
fn build_arg_extractors(func: &ItemFn) -> Result<ArgExtractors, Error> {
    let mut extract_stmts = Vec::new();
    let mut arg_idents = Vec::new();
    let mut has_request_context = false;

    for (index, arg) in func.sig.inputs.iter().enumerate() {
        let pat_type = match arg {
            FnArg::Typed(pat_type) => pat_type,
            FnArg::Receiver(receiver) => {
                return Err(Error::new(
                    receiver.span(),
                    "#[action] functions cannot have a `self` receiver",
                ));
            }
        };

        let ty = &pat_type.ty;
        if is_request_context_type(ty) {
            if has_request_context {
                return Err(Error::new(
                    ty.span(),
                    "#[action] functions support at most one RequestContext argument",
                ));
            }
            has_request_context = true;
            arg_idents.push(quote! { __ctx });
            continue;
        }

        let var_ident = format_ident!("__arg{}", index);
        extract_stmts.push(quote! {
            let #var_ident = <#ty as ::edgezero_core::extractor::FromRequest>::from_request(&__ctx).await?;
        });
        arg_idents.push(quote! { #var_ident });
    }

    Ok((extract_stmts, arg_idents))
}

fn is_request_context_type(ty: &Type) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };
    if type_path.qself.is_some() {
        return false;
    }
    path_is_request_context(&type_path.path)
}

fn normalize_request_context_pat(pat: &mut Box<Pat>) -> syn::Result<()> {
    let Some(replacement) = extract_request_context_binding(pat.as_ref())? else {
        return Ok(());
    };
    **pat = replacement;
    Ok(())
}

fn extract_request_context_binding(pat: &Pat) -> syn::Result<Option<Pat>> {
    let Pat::TupleStruct(tuple_pat) = pat else {
        return Ok(None);
    };
    if !path_is_request_context(&tuple_pat.path) {
        return Ok(None);
    }
    if tuple_pat.elems.len() != 1 {
        return Err(syn::Error::new(
            tuple_pat.span(),
            "RequestContext destructuring expects exactly one binding",
        ));
    }
    Ok(tuple_pat.elems.first().cloned())
}

fn path_is_request_context(path: &syn::Path) -> bool {
    path.segments.last().is_some_and(|segment| {
        segment.ident == "RequestContext" && matches!(segment.arguments, PathArguments::None)
    })
}

fn normalize_request_context_patterns(func: &mut ItemFn) -> Result<(), Error> {
    let mut error: Option<Error> = None;
    for arg in &mut func.sig.inputs {
        if let FnArg::Typed(pat_type) = arg {
            if is_request_context_type(&pat_type.ty) {
                if let Err(err) = normalize_request_context_pat(&mut pat_type.pat) {
                    if let Some(existing) = error.as_mut() {
                        existing.combine(err);
                    } else {
                        error = Some(err);
                    }
                }
            }
        }
    }

    if let Some(err) = error {
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::expand_action_impl;
    use proc_macro2::TokenStream;
    use quote::quote;

    fn render(tokens: &TokenStream) -> String {
        tokens.to_string()
    }

    fn collapse_whitespace(input: &str) -> String {
        input.split_whitespace().collect()
    }

    #[test]
    fn wraps_async_function() {
        let input = quote! {
            async fn demo(ctx: ::edgezero_core::context::RequestContext) -> ::edgezero_core::http::Response {
                ::edgezero_core::http::response_builder()
                    .status(::edgezero_core::http::StatusCode::OK)
                .body(::edgezero_core::body::Body::empty())
                    .unwrap()
            }
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        assert!(rendered.contains("__demo_inner"));
        assert!(rendered.contains("responder :: Responder :: respond"));
        // No params → a plain `async fn`, NOT a capability-carrying struct.
        let collapsed = collapse_whitespace(&rendered);
        assert!(collapsed.contains("asyncfndemo"));
        assert!(!collapsed.contains("structdemo"));
        assert!(!collapsed.contains("introspection_needs"));
    }

    #[test]
    fn rejects_non_async_functions() {
        let input = quote! {
            fn invalid() {}
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        assert!(rendered.contains("must be async"));
    }

    #[test]
    fn rejects_unknown_param() {
        let input = quote! {
            async fn demo(ctx: ::edgezero_core::context::RequestContext) -> ::edgezero_core::http::Response {
                unimplemented!()
            }
        };
        let output = expand_action_impl(&quote!(bogus), input);
        let rendered = render(&output);
        assert!(rendered.contains("unknown #[action] parameter"));
    }

    #[test]
    fn manifest_param_emits_capability_struct() {
        let input = quote! {
            async fn manifest(
                ManifestJson(json): ManifestJson,
            ) -> ::std::result::Result<
                ::edgezero_core::http::Response,
                ::edgezero_core::error::EdgeError,
            > {
                let _ = json;
                unimplemented!()
            }
        };
        let output = expand_action_impl(&quote!(manifest), input);
        let collapsed = collapse_whitespace(&render(&output));
        // Opt-in handlers become a capability-carrying struct, not a fn.
        assert!(collapsed.contains("structmanifest"));
        assert!(collapsed.contains("DynHandlerformanifest"));
        assert!(collapsed.contains("fnintrospection_needs"));
        // The `manifest` capability field is set true; `routes` false.
        assert!(collapsed.contains("manifest:true"));
        assert!(collapsed.contains("routes:false"));
    }

    #[test]
    fn manifest_and_routes_params_set_both_capabilities() {
        let input = quote! {
            async fn both(
                ManifestJson(json): ManifestJson,
                RouteTable(table): RouteTable,
            ) -> ::std::result::Result<
                ::edgezero_core::http::Response,
                ::edgezero_core::error::EdgeError,
            > {
                let _ = (json, table);
                unimplemented!()
            }
        };
        let output = expand_action_impl(&quote!(manifest, routes), input);
        let collapsed = collapse_whitespace(&render(&output));
        // The combined form emits a struct whose `introspection_needs` sets both.
        assert!(collapsed.contains("structboth"));
        assert!(collapsed.contains("manifest:true"));
        assert!(collapsed.contains("routes:true"));
    }

    #[test]
    fn rejects_self_receivers() {
        let input = quote! {
            async fn invalid(&self) -> ::edgezero_core::http::Response {
                unimplemented!()
            }
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        assert!(rendered.contains("does not support self receivers"));
    }

    #[test]
    fn allows_request_context_argument() {
        let input = quote! {
            async fn with_ctx(
                ctx: ::edgezero_core::context::RequestContext
            ) -> ::std::result::Result<
                ::edgezero_core::http::Response,
                ::edgezero_core::error::EdgeError
            > {
                let _ = ctx;
                Ok(::edgezero_core::http::response_builder()
                    .status(::edgezero_core::http::StatusCode::OK)
                    .body(::edgezero_core::body::Body::empty())
                    .unwrap())
            }
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        let collapsed = collapse_whitespace(&rendered);
        assert!(collapsed.contains("__with_ctx_inner(__ctx)"));
    }

    #[test]
    fn allows_request_context_tuple_pattern_argument() {
        let input = quote! {
            async fn tuple_ctx(
                RequestContext(ctx): ::edgezero_core::context::RequestContext
            ) -> ::std::result::Result<
                ::edgezero_core::http::Response,
                ::edgezero_core::error::EdgeError
            > {
                let _ = ctx;
                Ok(::edgezero_core::http::response_builder()
                    .status(::edgezero_core::http::StatusCode::OK)
                    .body(::edgezero_core::body::Body::empty())
                    .unwrap())
            }
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        let collapsed = collapse_whitespace(&rendered);
        assert!(collapsed.contains("__tuple_ctx_inner(__ctx)"));
    }

    #[test]
    fn rejects_multiple_request_context_arguments() {
        let input = quote! {
            async fn invalid(
                first: ::edgezero_core::context::RequestContext,
                second: ::edgezero_core::context::RequestContext,
            ) {}
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        assert!(rendered.contains("support at most one RequestContext argument"));
    }

    #[test]
    fn rejects_request_context_tuple_with_multiple_bindings() {
        let input = quote! {
            async fn invalid(
                RequestContext(a, b): ::edgezero_core::context::RequestContext
            ) -> ::edgezero_core::http::Response {
                unimplemented!()
            }
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        assert!(rendered.contains("expects exactly one binding"));
    }

    #[test]
    fn generates_extractor_calls_for_arguments() {
        let input = quote! {
            async fn demo(
                value: demo::ExtractorType
            ) -> ::edgezero_core::http::Response {
                let _ = value;
                ::edgezero_core::http::response_builder()
                    .status(::edgezero_core::http::StatusCode::OK)
                    .body(::edgezero_core::body::Body::empty())
                    .unwrap()
            }
        };
        let output = expand_action_impl(&TokenStream::new(), input);
        let rendered = render(&output);
        let collapsed = collapse_whitespace(&rendered);
        assert!(
            collapsed.contains("FromRequest>::from_request"),
            "expected extractor call in generated output: {rendered}"
        );
    }
}
