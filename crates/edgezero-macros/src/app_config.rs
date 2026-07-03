//! `#[derive(AppConfig)]` derive.
//!
//! Scans the input struct for `#[secret]` / `#[secret(store_ref)]`
//! field annotations, enforces the compile-time constraints, and
//! emits `impl ::edgezero_core::app_config::AppConfigMeta` with the
//! `SECRET_FIELDS` array.

use std::collections::{HashMap, HashSet};

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, Attribute, Data, DeriveInput, Expr, ExprLit, Field, Fields, GenericArgument,
    Ident, Lit, Meta, MetaNameValue, Path, PathArguments, Type,
};

/// Recognised `#[secret(...)]` annotation kinds.
enum SecretAnnotation {
    /// Plain `#[secret]` — the field value is a key in the resolved
    /// default secret store.
    KeyInDefault,
    /// `#[secret(store_ref = "field")]` — the field value is a key in the
    /// named secret store identified by sibling field `store_ref_field`.
    KeyInNamedStore {
        /// Name of the sibling `#[secret(store_ref)]` field.
        store_ref_field: String,
    },
    /// `#[secret(store_ref)]` — the field value is a `[stores.secrets]`
    /// logical id.
    StoreRef,
}

/// Per-field annotation result captured during scanning.
struct FieldAnnotation {
    kind: SecretAnnotation,
    name: Ident,
    /// `true` when the annotated field is `Option<String>`.
    optional: bool,
}

/// A `#[app_config(nested)]` field to recurse into when emitting
/// `secret_fields()`.
struct NestedDescriptor<'field> {
    /// The element type whose `secret_fields()` are prepended: the field
    /// type for an object, or the `Vec`/slice element type for an array.
    child_ty: &'field Type,
    /// The Rust field name, emitted verbatim as a `Field` path segment.
    field_name: Ident,
    /// `true` when the field is `Vec<T>` / `[T]` (emit `Field` + `ArrayEach`).
    is_array: bool,
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
    let fields = struct_fields(input)?;

    // Enforce serde skip/flatten bans on EVERY field (not just secret ones).
    enforce_no_disallowed_serde_attrs_on_all_fields(fields)?;

    let (annotations, nested_descriptors) = classify_fields(fields)?;

    // secret_fields() emits the Rust field name verbatim. A container-
    // level `#[serde(rename_all = ...)]` would desync that metadata
    // from what `config validate` (and the Spin collision check) sees
    // on the wire — silently — so reject it whenever any secret field is
    // present, whether direct or reached through a nested child. Structs
    // with no secret paths are unaffected: secret_fields() is empty and
    // the validator never compares names.
    if !annotations.is_empty() || !nested_descriptors.is_empty() {
        enforce_no_container_rename_all(&input.attrs)?;
    }

    // Validate `KeyInNamedStore` sibling references. Build a map of
    // field-name → annotation so we can verify:
    //   (a) the named sibling exists,
    //   (b) the sibling is annotated `#[secret(store_ref)]`,
    //   (c) the sibling is `String` (already enforced by `scan_field`).
    {
        // Set of all struct field names, for a better "field not found" error
        // when a sibling exists but lacks `#[secret(store_ref)]`.
        let all_field_names: HashSet<String> = fields
            .iter()
            .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
            .collect();

        // Build a set of (name_string → kind_index) for O(1) lookup.
        let name_to_kind: HashMap<String, &SecretAnnotation> = annotations
            .iter()
            .map(|ann| (ann.name.to_string(), &ann.kind))
            .collect();

        for annotation in &annotations {
            if let SecretAnnotation::KeyInNamedStore { store_ref_field } = &annotation.kind {
                match name_to_kind.get(store_ref_field.as_str()) {
                    None if !all_field_names.contains(store_ref_field.as_str()) => {
                        return Err(syn::Error::new(
                            Span::call_site(),
                            format!(
                                "`#[secret(store_ref = \"{store_ref_field}\")]` names sibling \
                                 field `{store_ref_field}` which does not exist on this struct",
                            ),
                        ));
                    }
                    Some(SecretAnnotation::StoreRef) => {} // correct
                    None | Some(_) => {
                        // `None`: field exists in the struct but lacks `#[secret(store_ref)]`.
                        // `Some(_)`: field has a different `#[secret]` annotation, not `StoreRef`.
                        return Err(syn::Error::new(
                            Span::call_site(),
                            format!(
                                "`#[secret(store_ref = \"{store_ref_field}\")]` names sibling \
                                 field `{store_ref_field}` which must be annotated \
                                 `#[secret(store_ref)]`, but it is not",
                            ),
                        ));
                    }
                }
            }
        }
    }

    Ok(emit_impl(input, &annotations, &nested_descriptors))
}

/// Classify every field as a direct `#[secret]` annotation or a
/// `#[app_config(nested)]` recursion descriptor. A field may not be both.
fn classify_fields(
    fields: &Punctuated<Field, syn::Token![,]>,
) -> syn::Result<(Vec<FieldAnnotation>, Vec<NestedDescriptor<'_>>)> {
    let mut annotations: Vec<FieldAnnotation> = Vec::new();
    let mut nested_descriptors: Vec<NestedDescriptor> = Vec::new();
    for field in fields {
        let is_nested = nested_optin(field)?;
        match scan_field(field)? {
            Some(_) if is_nested => {
                return Err(syn::Error::new_spanned(
                    field,
                    "a field may not be both `#[secret]` and `#[app_config(nested)]`",
                ));
            }
            Some(annotation) => annotations.push(annotation),
            None if is_nested => {
                // The emitter writes `Field(field_name)` verbatim, so a
                // `#[serde(rename/flatten/skip*)]` on the nested parent would
                // desync the path segment from the serialized key — banned on
                // any secret path.
                enforce_no_disallowed_serde_attrs(field)?;
                let Some(field_name) = field.ident.clone() else {
                    return Err(syn::Error::new_spanned(
                        field,
                        "`#[app_config(nested)]` requires a named field",
                    ));
                };
                let (child_ty, is_array) = nested_child_type(&field.ty);
                nested_descriptors.push(NestedDescriptor {
                    child_ty,
                    field_name,
                    is_array,
                });
            }
            None => {}
        }
    }
    Ok((annotations, nested_descriptors))
}

/// Emit `impl AppConfigMeta` (with the `secret_fields()` body), the
/// `AppConfigRoot` marker impl, and a per-child `AppConfigRoot` bound
/// assertion.
fn emit_impl(
    input: &DeriveInput,
    annotations: &[FieldAnnotation],
    nested_descriptors: &[NestedDescriptor<'_>],
) -> TokenStream2 {
    let struct_ident = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    // Direct `#[secret]` leaves: length-1 `Field` path, `optional` set from
    // `Option<String>`.
    let direct_entries = annotations.iter().map(|annotation| {
        let name_lit = annotation.name.to_string();
        let optional = annotation.optional;
        let kind_tokens = match &annotation.kind {
            SecretAnnotation::KeyInDefault => {
                quote!(::edgezero_core::app_config::SecretKind::KeyInDefault)
            }
            SecretAnnotation::StoreRef => {
                quote!(::edgezero_core::app_config::SecretKind::StoreRef)
            }
            SecretAnnotation::KeyInNamedStore { store_ref_field } => {
                let lit = syn::LitStr::new(store_ref_field, Span::call_site());
                quote!(::edgezero_core::app_config::SecretKind::KeyInNamedStore {
                    store_ref_field: #lit
                })
            }
        };
        quote! {
            ::edgezero_core::app_config::SecretField {
                kind: #kind_tokens,
                path: ::std::vec![::edgezero_core::app_config::SecretPathSegment::Field(
                    ::std::borrow::Cow::Borrowed(#name_lit)
                )],
                optional: #optional,
            }
        }
    });

    // Nested children: prepend `Field(field)` (object) or `Field(field)` +
    // `ArrayEach` (`Vec`/slice) onto every leaf the child reports.
    let nested_pushes = nested_descriptors.iter().map(|descriptor| {
        let field_lit = descriptor.field_name.to_string();
        let child_ty = descriptor.child_ty;
        let prefix = if descriptor.is_array {
            quote! {
                ::std::vec![
                    ::edgezero_core::app_config::SecretPathSegment::Field(
                        ::std::borrow::Cow::Borrowed(#field_lit)
                    ),
                    ::edgezero_core::app_config::SecretPathSegment::ArrayEach,
                ]
            }
        } else {
            quote! {
                ::std::vec![
                    ::edgezero_core::app_config::SecretPathSegment::Field(
                        ::std::borrow::Cow::Borrowed(#field_lit)
                    ),
                ]
            }
        };
        quote! {
            for mut __f in <#child_ty as ::edgezero_core::app_config::AppConfigMeta>::secret_fields() {
                let mut __p = #prefix;
                __p.append(&mut __f.path);
                __f.path = __p;
                __out.push(__f);
            }
        }
    });

    let secret_fields_body = if nested_descriptors.is_empty() {
        quote! { ::std::vec![#(#direct_entries),*] }
    } else {
        quote! {
            let mut __out: ::std::vec::Vec<::edgezero_core::app_config::SecretField> =
                ::std::vec![#(#direct_entries),*];
            #(#nested_pushes)*
            __out
        }
    };

    // A nested child must go through `#[derive(AppConfig)]` — the
    // `AppConfigRoot` marker — not merely impl `AppConfigMeta` by hand.
    // The closure is never called, but coercing it to `fn()` type-checks
    // its body, enforcing the bound with a clear error span per child.
    let nested_child_tys: Vec<&Type> = nested_descriptors
        .iter()
        .map(|descriptor| descriptor.child_ty)
        .collect();
    let root_assertion = if nested_child_tys.is_empty() {
        quote! {}
    } else {
        quote! {
            const _: fn() = || {
                fn __assert_app_config_root<__T: ::edgezero_core::app_config::AppConfigRoot>() {}
                #( __assert_app_config_root::<#nested_child_tys>(); )*
            };
        }
    };

    quote! {
        #root_assertion

        #[automatically_derived]
        impl #impl_generics ::edgezero_core::app_config::AppConfigMeta
            for #struct_ident #type_generics #where_clause
        {
            fn secret_fields() -> ::std::vec::Vec<::edgezero_core::app_config::SecretField> {
                #secret_fields_body
            }
        }

        #[automatically_derived]
        impl #impl_generics ::edgezero_core::app_config::AppConfigRoot
            for #struct_ident #type_generics #where_clause
        {}
    }
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

    let optional = secret_string_optionality(&field.ty).ok_or_else(|| {
        syn::Error::new_spanned(
            &field.ty,
            "`#[secret]` may only annotate `String` or `Option<String>`",
        )
    })?;
    // A `#[secret(store_ref)]` value is a store id — structural, always
    // present. `Option<String>` there is undefined (an absent store cannot
    // resolve its dependent `KeyInNamedStore` sibling), so reject it.
    if optional && matches!(kind, SecretAnnotation::StoreRef) {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "`#[secret(store_ref)]` may not be `Option<String>`: a store id is structural and must always be present",
        ));
    }
    enforce_no_disallowed_serde_attrs(field)?;

    Ok(Some(FieldAnnotation {
        kind,
        name,
        optional,
    }))
}

/// Whether `field` carries `#[app_config(nested)]`. Returns `Err` (not
/// `false`) on a malformed `#[app_config(...)]` such as `#[app_config(bogus)]`
/// or an empty `#[app_config()]`, so a typo is a hard compile error rather
/// than a silently-ignored non-recursion (which would drop the child's
/// secrets).
fn nested_optin(field: &Field) -> syn::Result<bool> {
    let mut found = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("app_config") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("nested") {
                found = true;
                Ok(())
            } else {
                Err(meta.error("`#[app_config(...)]` only accepts `nested`"))
            }
        })?;
    }
    Ok(found)
}

/// The child element type to recurse into and whether it is an array element.
/// `Vec<T>` / `[T]` -> (T, true); otherwise (`field_ty`, false).
fn nested_child_type(ty: &Type) -> (&Type, bool) {
    if let Type::Path(type_path) = ty {
        if let Some(last) = type_path.path.segments.last() {
            if last.ident == "Vec" {
                if let PathArguments::AngleBracketed(bracketed) = &last.arguments {
                    if let Some(GenericArgument::Type(inner)) = bracketed.args.first() {
                        return (inner, true);
                    }
                }
            }
        }
    }
    if let Type::Slice(slice) = ty {
        return (&slice.elem, true);
    }
    (ty, false)
}

/// Decode `#[secret]` (`KeyInDefault`), `#[secret(store_ref)]`
/// (`StoreRef`), and `#[secret(store_ref = "field")]`
/// (`KeyInNamedStore`). Any other form is a compile error.
fn parse_secret_kind(attr: &Attribute) -> Result<SecretAnnotation, syn::Error> {
    match &attr.meta {
        Meta::Path(_) => Ok(SecretAnnotation::KeyInDefault),
        Meta::List(list) => {
            // Try `store_ref = "field"` first (name-value form).
            if let Ok(nv) = syn::parse2::<MetaNameValue>(list.tokens.clone()) {
                if nv.path.is_ident("store_ref") {
                    if let Expr::Lit(ExprLit {
                        lit: Lit::Str(str_lit),
                        ..
                    }) = nv.value
                    {
                        return Ok(SecretAnnotation::KeyInNamedStore {
                            store_ref_field: str_lit.value(),
                        });
                    }
                }
            }
            // Try bare `store_ref` path.
            if let Ok(path) = syn::parse2::<Path>(list.tokens.clone()) {
                if path.is_ident("store_ref") {
                    return Ok(SecretAnnotation::StoreRef);
                }
            }
            Err(syn::Error::new_spanned(
                &list.tokens,
                "`#[secret(...)]` accepts `store_ref` or `store_ref = \"field\"` \
                 (e.g. `#[secret(store_ref)]` or `#[secret(store_ref = \"vault\")]`)",
            ))
        }
        Meta::NameValue(_) => Err(syn::Error::new_spanned(
            attr,
            "`#[secret = \"...\"]` form is not supported; use `#[secret]` or `#[secret(store_ref)]`",
        )),
    }
}

/// Classify a `#[secret]` field's type: `String` -> `Some(false)`,
/// `Option<String>` -> `Some(true)`, anything else (e.g. `Vec<String>`,
/// `Cow<'_, str>`, non-string scalars) -> `None`.
fn secret_string_optionality(ty: &Type) -> Option<bool> {
    if is_scalar_string_type(ty) {
        return Some(false);
    }
    if let Type::Path(type_path) = ty {
        if let Some(last) = type_path.path.segments.last() {
            if last.ident == "Option" {
                if let PathArguments::AngleBracketed(bracketed) = &last.arguments {
                    if let Some(GenericArgument::Type(inner)) = bracketed.args.first() {
                        if is_scalar_string_type(inner) {
                            return Some(true);
                        }
                    }
                }
            }
        }
    }
    None
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

/// Walk ALL fields of an `AppConfig`-derived struct and reject
/// `#[serde(skip_serializing)]`, `#[serde(skip_serializing_if = "...")]`,
/// and `#[serde(flatten)]`. These attributes desync the canonical-form
/// rules (4.2) from the serde JSON shape regardless of whether the
/// field is annotated `#[secret]`.
fn enforce_no_disallowed_serde_attrs_on_all_fields(
    fields: &Punctuated<Field, syn::Token![,]>,
) -> Result<(), syn::Error> {
    for field in fields {
        for attr in &field.attrs {
            if !attr.path().is_ident("serde") {
                continue;
            }
            let mut offending: Option<String> = None;
            let _parse_result: syn::Result<()> = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("skip_serializing")
                    || meta.path.is_ident("skip_serializing_if")
                    || meta.path.is_ident("flatten")
                {
                    offending = Some(
                        meta.path
                            .get_ident()
                            .map_or_else(String::new, ToString::to_string),
                    );
                }
                Ok(())
            });
            if let Some(name) = offending {
                return Err(syn::Error::new_spanned(
                    attr,
                    format!(
                        "`#[serde({name})]` is not allowed on fields of an \
                         `AppConfig`-derived struct (it would desync the \
                         canonical-form rules in 4.2 from the serde JSON shape). \
                         If you need a flat layout, define it explicitly.",
                    ),
                ));
            }
        }
    }
    Ok(())
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
