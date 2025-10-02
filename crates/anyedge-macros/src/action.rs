use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{spanned::Spanned, FnArg, ItemFn};

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

    let mut extract_stmts = Vec::new();
    let mut arg_idents = Vec::new();

    for (index, arg) in func.sig.inputs.iter().enumerate() {
        let pat_type = match arg {
            FnArg::Typed(pat_type) => pat_type,
            FnArg::Receiver(_) => unreachable!(),
        };

        let ty = &pat_type.ty;
        let var_ident = format_ident!("__arg{}", index);
        extract_stmts.push(quote! {
            let #var_ident = <#ty as ::anyedge_core::extractor::FromRequest>::from_request(&__ctx).await?;
        });
        arg_idents.push(var_ident);
    }

    let output = quote! {
        #inner_fn

        #(#attrs)*
        #vis async fn #ident(
            __ctx: ::anyedge_core::context::RequestContext,
        ) -> ::std::result::Result<::anyedge_core::http::Response, ::anyedge_core::error::EdgeError> {
            #(#extract_stmts)*
            let result = #inner_ident(#(#arg_idents),*).await;
            ::anyedge_core::responder::Responder::respond(result)
        }
    };

    output
}

#[cfg(test)]
mod tests {
    use super::expand_action_impl;
    use proc_macro2::TokenStream;
    use quote::quote;

    fn render(tokens: TokenStream) -> String {
        tokens.to_string()
    }

    #[test]
    fn wraps_async_function() {
        let input = quote! {
            async fn demo(ctx: ::anyedge_core::context::RequestContext) -> ::anyedge_core::http::Response {
                ::anyedge_core::http::response_builder()
                    .status(::anyedge_core::http::StatusCode::OK)
                .body(::anyedge_core::body::Body::empty())
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
}
