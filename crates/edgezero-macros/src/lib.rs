mod action;
mod app;

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    action::expand_action(attr, item)
}

#[proc_macro]
pub fn app(input: TokenStream) -> TokenStream {
    app::expand_app(input)
}
