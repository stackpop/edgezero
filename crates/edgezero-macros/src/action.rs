use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{spanned::Spanned, Error, FnArg, ItemFn, Pat, PathArguments, Type};

pub fn expand_action(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_action_impl(attr.into(), item.into()).into()
}

pub(crate) fn expand_action_impl(
    attr: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(attr.span(), "#[action] does not accept arguments")
            .to_compile_error();
    }

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

    if let Err(err) = normalize_request_context_patterns(&mut inner_fn) {
        return err.to_compile_error();
    }

    let mut extract_stmts = Vec::new();
    let mut arg_idents = Vec::new();
    let mut has_request_context = false;

    for (index, arg) in func.sig.inputs.iter().enumerate() {
        let pat_type = match arg {
            FnArg::Typed(pat_type) => pat_type,
            FnArg::Receiver(_) => unreachable!(),
        };

        let ty = &pat_type.ty;
        if is_request_context_type(ty) {
            if has_request_context {
                return syn::Error::new(
                    ty.span(),
                    "#[action] functions support at most one RequestContext argument",
                )
                .to_compile_error();
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

    let output = quote! {
        #inner_fn

        #(#attrs)*
        #vis async fn #ident(
            __ctx: ::edgezero_core::context::RequestContext,
        ) -> ::std::result::Result<::edgezero_core::http::Response, ::edgezero_core::error::EdgeError> {
            #(#extract_stmts)*
            let result = #inner_ident(#(#arg_idents),*).await;
            ::edgezero_core::responder::Responder::respond(result)
        }
    };

    output
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
    *pat = Box::new(replacement);
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
    path.segments
        .last()
        .map(|segment| {
            segment.ident == "RequestContext" && matches!(segment.arguments, PathArguments::None)
        })
        .unwrap_or(false)
}

fn normalize_request_context_patterns(func: &mut ItemFn) -> Result<(), Error> {
    let mut error: Option<Error> = None;
    for arg in func.sig.inputs.iter_mut() {
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

    fn render(tokens: TokenStream) -> String {
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
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
        assert!(rendered.contains("__demo_inner"));
        assert!(rendered.contains("fn demo"));
        assert!(rendered.contains("responder :: Responder :: respond"));
    }

    #[test]
    fn rejects_non_async_functions() {
        let input = quote! {
            fn invalid() {}
        };
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
        assert!(rendered.contains("must be async"));
    }

    #[test]
    fn rejects_attribute_arguments() {
        let input = quote! {
            async fn demo(ctx: ::edgezero_core::context::RequestContext) -> ::edgezero_core::http::Response {
                unimplemented!()
            }
        };
        let output = expand_action_impl(quote!(path = "/demo"), input);
        let rendered = render(output);
        assert!(rendered.contains("does not accept arguments"));
    }

    #[test]
    fn rejects_self_receivers() {
        let input = quote! {
            async fn invalid(&self) -> ::edgezero_core::http::Response {
                unimplemented!()
            }
        };
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
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
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
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
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
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
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
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
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
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
        let output = expand_action_impl(TokenStream::new(), input);
        let rendered = render(output);
        let collapsed = collapse_whitespace(&rendered);
        assert!(
            collapsed.contains("FromRequest>::from_request"),
            "expected extractor call in generated output: {rendered}"
        );
    }
}
