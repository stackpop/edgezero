//! `#[derive(AppConfig)]` derive.
//!
//! Scans the input struct for `#[secret]` / `#[secret(store_ref)]`
//! field annotations, enforces the compile-time constraints, and
//! emits `impl ::edgezero_core::app_config::AppConfigMeta` with the
//! `SECRET_FIELDS` array.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, Attribute, Data, DeriveInput, Field, Fields, Ident, Meta, Path, Type,
};

/// Recognised `#[secret(...)]` annotation kinds.
enum SecretAnnotation {
    /// Plain `#[secret]` — the field value is a key in the resolved
    /// default secret store.
    KeyInDefault,
    /// `#[secret(store_ref)]` — the field value is a `[stores.secrets]`
    /// logical id.
    StoreRef,
}

/// Per-field annotation result captured during scanning.
struct FieldAnnotation {
    kind: SecretAnnotation,
    name: Ident,
}

/// Inspect the input struct, emit `impl AppConfigMeta` with the
/// `SECRET_FIELDS` array. Errors surface as `compile_error!` tokens
/// substituted in place of the impl.
#[inline]
pub fn derive(tokens: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(tokens as DeriveInput);
    expand(&parsed)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn expand(input: &DeriveInput) -> Result<TokenStream2, syn::Error> {
    let struct_ident = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    let fields = struct_fields(input)?;
    let mut annotations: Vec<FieldAnnotation> = Vec::new();
    for field in fields {
        if let Some(annotation) = scan_field(field)? {
            annotations.push(annotation);
        }
    }

    // SECRET_FIELDS emits the Rust field name verbatim. A container-
    // level `#[serde(rename_all = ...)]` would desync that metadata
    // from what `config validate` (and the Spin collision check) sees
    // on the wire — silently — so reject it whenever any
    // secret field is present. Structs with no secret fields are
    // unaffected: SECRET_FIELDS is empty and the validator never
    // compares names.
    if !annotations.is_empty() {
        enforce_no_container_rename_all(&input.attrs)?;
    }

    let entries = annotations.iter().map(|annotation| {
        let name_lit = annotation.name.to_string();
        let kind_tokens = match annotation.kind {
            SecretAnnotation::KeyInDefault => {
                quote!(::edgezero_core::app_config::SecretKind::KeyInDefault)
            }
            SecretAnnotation::StoreRef => quote!(::edgezero_core::app_config::SecretKind::StoreRef),
        };
        quote! {
            ::edgezero_core::app_config::SecretField {
                name: #name_lit,
                kind: #kind_tokens,
            }
        }
    });

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::edgezero_core::app_config::AppConfigMeta
            for #struct_ident #type_generics #where_clause
        {
            const SECRET_FIELDS: &'static [::edgezero_core::app_config::SecretField] =
                &[#(#entries),*];
        }
    })
}

/// Borrow the struct's named fields, or error with a clear message.
fn struct_fields(input: &DeriveInput) -> Result<&Punctuated<Field, syn::Token![,]>, syn::Error> {
    let data = match &input.data {
        Data::Struct(data) => data,
        Data::Enum(_) | Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`#[derive(AppConfig)]` is only supported on structs",
            ));
        }
    };
    match &data.fields {
        Fields::Named(named) => Ok(&named.named),
        Fields::Unnamed(_) => Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(AppConfig)]` is only supported on structs with named fields",
        )),
        Fields::Unit => Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(AppConfig)]` is only supported on structs with named fields (this struct has no fields)",
        )),
    }
}

/// Inspect a single field. Returns `Ok(Some(...))` when the field
/// carries a recognised `#[secret]` annotation, `Ok(None)` when it
/// carries none, and `Err` for an invalid combination.
fn scan_field(field: &Field) -> Result<Option<FieldAnnotation>, syn::Error> {
    let Some(name) = field.ident.clone() else {
        return Ok(None);
    };

    let mut secret_attrs = field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("secret"));
    let Some(first) = secret_attrs.next() else {
        return Ok(None);
    };
    if let Some(duplicate) = secret_attrs.next() {
        return Err(syn::Error::new_spanned(
            duplicate,
            "duplicate `#[secret]` annotation on the same field",
        ));
    }
    let kind = parse_secret_kind(first)?;

    enforce_scalar_string_type(field)?;
    enforce_no_disallowed_serde_attrs(field)?;

    Ok(Some(FieldAnnotation { kind, name }))
}

/// Decode `#[secret]` (`KeyInDefault`) and `#[secret(store_ref)]`
/// (`StoreRef`). Any other token list is a compile error.
fn parse_secret_kind(attr: &Attribute) -> Result<SecretAnnotation, syn::Error> {
    match &attr.meta {
        Meta::Path(_) => Ok(SecretAnnotation::KeyInDefault),
        Meta::List(list) => {
            let inner: Path = syn::parse2(list.tokens.clone()).map_err(|_unused| {
                syn::Error::new_spanned(
                    &list.tokens,
                    "`#[secret(...)]` accepts only `store_ref` (e.g. `#[secret(store_ref)]`)",
                )
            })?;
            if inner.is_ident("store_ref") {
                Ok(SecretAnnotation::StoreRef)
            } else {
                Err(syn::Error::new_spanned(
                    &list.tokens,
                    "`#[secret(...)]` accepts only `store_ref` (e.g. `#[secret(store_ref)]`)",
                ))
            }
        }
        Meta::NameValue(_) => Err(syn::Error::new_spanned(
            attr,
            "`#[secret = \"...\"]` form is not supported; use `#[secret]` or `#[secret(store_ref)]`",
        )),
    }
}

/// `#[secret]` may only annotate a scalar string field. Per we
/// accept bare `String` only — generic or qualified forms (e.g.
/// `Option<String>`, `Cow<'_, str>`) are intentionally rejected so
/// `cfg.api_token` resolves to a value at every call site.
fn enforce_scalar_string_type(field: &Field) -> Result<(), syn::Error> {
    if !is_scalar_string_type(&field.ty) {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "`#[secret]` / `#[secret(store_ref)]` may only annotate a scalar string field (e.g. `String`)",
        ));
    }
    Ok(())
}

fn is_scalar_string_type(ty: &Type) -> bool {
    if let Type::Path(type_path) = ty {
        if type_path.qself.is_none() {
            if let Some(last) = type_path.path.segments.last() {
                return last.ident == "String" && last.arguments.is_empty();
            }
        }
    }
    false
}

/// Container-level guard: a struct that carries any `#[secret]` field
/// must not also carry `#[serde(rename_all = ...)]`. The derive emits
/// `SECRET_FIELDS` with Rust field names verbatim, but `rename_all`
/// would translate the on-the-wire key name (e.g. `kebab-case` →
/// `api-token`), silently desyncing the typed `config validate` secret
/// checks from what the deserialiser actually accepts. Reject this at
/// compile time so the desync can't ship.
fn enforce_no_container_rename_all(attrs: &[Attribute]) -> Result<(), syn::Error> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let mut offending = false;
        let _parse_result: syn::Result<()> = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                offending = true;
            }
            Ok(())
        });
        if offending {
            return Err(syn::Error::new_spanned(
                attr,
                "`#[derive(AppConfig)]` rejects `#[serde(rename_all = ...)]` on structs with `#[secret]` fields: SECRET_FIELDS uses Rust field names verbatim, so a container rename would silently desync `config validate` from runtime deserialisation",
            ));
        }
    }
    Ok(())
}

/// `#[secret]` cannot coexist with `#[serde(flatten)]` /
/// `#[serde(rename)]` / `#[serde(skip*)]` because the derive emits the
/// Rust field name verbatim and downstream tooling (config validate /
/// config push) expects that name to round-trip via TOML serde without
/// translation or omission.
fn enforce_no_disallowed_serde_attrs(field: &Field) -> Result<(), syn::Error> {
    for attr in &field.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let mut offending: Option<&'static str> = None;
        // `parse_nested_meta` walks each comma-separated entry in the
        // `#[serde(...)]` list. We swallow its own parse errors — those
        // belong to the user's serde macros, not ours — and only react
        // when a disallowed key is observed.
        let _parse_result: syn::Result<()> = attr.parse_nested_meta(|meta| {
            if let Some(ident) = meta.path.get_ident() {
                offending = match ident.to_string().as_str() {
                    "flatten" => Some("flatten"),
                    "rename" => Some("rename"),
                    "skip" => Some("skip"),
                    "skip_deserializing" => Some("skip_deserializing"),
                    "skip_serializing" => Some("skip_serializing"),
                    // `skip_serializing_if = "..."` also omits the
                    // field from round-trips (config push reads
                    // SECRET_FIELDS, then serialises the typed
                    // struct), so reject it alongside the
                    // unconditional skip family.
                    "skip_serializing_if" => Some("skip_serializing_if"),
                    _ => offending,
                };
            }
            Ok(())
        });
        if let Some(name) = offending {
            return Err(syn::Error::new_spanned(
                attr,
                format!(
                    "`#[secret]` is incompatible with `#[serde({name})]` — the derive emits the Rust field name verbatim and config validate / push round-trip it via TOML",
                ),
            ));
        }
    }
    Ok(())
}
