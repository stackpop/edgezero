mod action;
mod app;
mod manifest_definitions;

use proc_macro::TokenStream;

#[proc_macro_attribute]
#[inline]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    action::expand_action(attr, item)
}

#[proc_macro]
#[inline]
pub fn app(input: TokenStream) -> TokenStream {
    app::expand_app(input)
}
