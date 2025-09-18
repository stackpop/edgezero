use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{parse_macro_input, spanned::Spanned, FnArg, ItemFn, Result};

#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            Span::call_site(),
            "`action` attribute does not accept arguments",
        )
        .to_compile_error()
        .into();
    }

    let input_fn = parse_macro_input!(item as ItemFn);
    match expand_action(input_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_action(item: ItemFn) -> Result<TokenStream2> {
    if !item.sig.generics.params.is_empty() {
        return Err(syn::Error::new(
            item.sig.generics.span(),
            "`action` functions cannot be generic",
        ));
    }

    if item.sig.variadic.is_some() {
        return Err(syn::Error::new(
            item.sig.variadic.span(),
            "variadic parameters are not supported",
        ));
    }

    let attrs = item.attrs.clone();
    let vis = item.vis.clone();
    let fn_name = item.sig.ident.clone();
    let inner_name = format_ident!("__anyedge_inner_{}", fn_name);
    let mut inner_fn = item.clone();
    inner_fn.attrs.clear();
    inner_fn.vis = syn::Visibility::Inherited;
    inner_fn.sig.ident = inner_name.clone();
    let inputs = inner_fn.sig.inputs.clone();
    let is_async = inner_fn.sig.asyncness.is_some();

    let mut extraction = Vec::new();
    let mut arg_idents = Vec::new();

    for (index, arg) in inputs.iter().enumerate() {
        let typed = match arg {
            FnArg::Typed(pat) => pat,
            FnArg::Receiver(receiver) => {
                return Err(syn::Error::new(
                    receiver.span(),
                    "`action` functions cannot take `self`",
                ));
            }
        };

        let binding = format_ident!("__anyedge_arg_{index}");
        let ty = &typed.ty;

        extraction.push(quote! {
            let #binding = match <#ty as ::anyedge_controller::FromRequest>::from_request(&mut __anyedge_parts) {
                ::core::result::Result::Ok(value) => value,
                ::core::result::Result::Err(rejection) => {
                    return ::anyedge_controller::Responder::into_response(rejection);
                }
            };
        });

        arg_idents.push(binding);
    }

    let inner_function = quote! { #inner_fn };

    let invocation = if is_async {
        quote! { #inner_name(#(#arg_idents),*).await }
    } else {
        quote! { #inner_name(#(#arg_idents),*) }
    };

    Ok(quote! {
        #(#attrs)*
        #vis fn #fn_name() -> ::anyedge_controller::ControllerHandler {
            #inner_function
            ::anyedge_controller::ControllerHandler::from_fn(|__anyedge_request: ::anyedge_core::Request| {
                async move {
                    let mut __anyedge_parts = ::anyedge_controller::RequestParts::new(__anyedge_request);
                    #(#extraction)*
                    let __anyedge_result = #invocation;
                    ::anyedge_controller::Responder::into_response(__anyedge_result)
                }
            })
        }
    })
}
