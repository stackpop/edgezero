mod action;

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    action::expand_action(attr, item)
}
