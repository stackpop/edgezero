mod action;
mod app;
mod app_config;
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

#[proc_macro_derive(AppConfig, attributes(secret, app_config))]
#[inline]
pub fn app_config_derive(input: TokenStream) -> TokenStream {
    app_config::derive(input)
}
