use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::Attribute;
use syn::Data;
use syn::DataEnum;
use syn::DataStruct;
use syn::DeriveInput;
use syn::Field;
use syn::Fields;
use syn::Ident;
use syn::LitStr;
use syn::Type;
use syn::ext::IdentExt;
use syn::parse_macro_input;

/// Marks struct fields or enum variants as experimental. Presence-sensitive
/// fields must spell Option, Vec, HashMap, BTreeMap, or bool directly; aliases
/// and other types are conservatively considered present. `nested` delegates
/// to the field's ExperimentalApi implementation and is only valid on structs.
#[proc_macro_derive(ExperimentalApi, attributes(experimental))]
pub fn derive_experimental_api(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    reject_experimental(&input.attrs, "types")?;
    match &input.data {
        Data::Struct(data) => derive_for_struct(input, data),
        Data::Enum(data) => derive_for_enum(input, data),
        Data::Union(_) => Err(syn::Error::new_spanned(
            &input.ident,
            "ExperimentalApi does not support unions",
        )),
    }
}

fn derive_for_struct(
    input: &DeriveInput,
    data: &DataStruct,
) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let type_name_lit = LitStr::new(&name.unraw().to_string(), Span::call_site());
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let (checks, experimental_fields, registrations) = match &data.fields {
        Fields::Named(named) => {
            let mut checks = Vec::new();
            let mut experimental_fields = Vec::new();
            let mut registrations = Vec::new();
            for field in &named.named {
                let annotation = experimental_annotation(&field.attrs)?;
                let Some(ident) = field.ident.as_ref() else {
                    continue;
                };
                if let Some(Experimental::Reason(reason)) = annotation {
                    let expr = presence_expr_for_access(quote!(self.#ident), &field.ty);
                    checks.push(quote! {
                        if #expr {
                            return Some(#reason);
                        }
                    });

                    if let Some(field_name) = field_serialized_name(field, &input.attrs)? {
                        let field_name_lit = LitStr::new(&field_name, Span::call_site());
                        experimental_fields.push(quote! {
                            crate::experimental_api::ExperimentalField {
                                type_name: #type_name_lit,
                                field_name: #field_name_lit,
                                reason: #reason,
                            }
                        });
                        registrations.push(quote! {
                            ::inventory::submit! {
                                crate::experimental_api::ExperimentalField {
                                    type_name: #type_name_lit,
                                    field_name: #field_name_lit,
                                    reason: #reason,
                                }
                            }
                        });
                    }
                } else if matches!(annotation, Some(Experimental::Nested)) {
                    checks.push(quote! {
                        if let Some(reason) =
                            crate::experimental_api::ExperimentalApi::experimental_reason(&self.#ident)
                        {
                            return Some(reason);
                        }
                    });
                }
            }
            (checks, experimental_fields, registrations)
        }
        Fields::Unnamed(unnamed) => {
            let mut checks = Vec::new();
            let mut experimental_fields = Vec::new();
            let mut registrations = Vec::new();
            for (index, field) in unnamed.unnamed.iter().enumerate() {
                let annotation = experimental_annotation(&field.attrs)?;
                if let Some(Experimental::Reason(reason)) = annotation {
                    let expr = index_presence_expr(index, &field.ty);
                    checks.push(quote! {
                        if #expr {
                            return Some(#reason);
                        }
                    });

                    let field_name_lit = LitStr::new(&index.to_string(), Span::call_site());
                    experimental_fields.push(quote! {
                        crate::experimental_api::ExperimentalField {
                            type_name: #type_name_lit,
                            field_name: #field_name_lit,
                            reason: #reason,
                        }
                    });
                    registrations.push(quote! {
                        ::inventory::submit! {
                            crate::experimental_api::ExperimentalField {
                                type_name: #type_name_lit,
                                field_name: #field_name_lit,
                                reason: #reason,
                            }
                        }
                    });
                } else if matches!(annotation, Some(Experimental::Nested)) {
                    let index = syn::Index::from(index);
                    checks.push(quote! {
                        if let Some(reason) =
                            crate::experimental_api::ExperimentalApi::experimental_reason(&self.#index)
                        {
                            return Some(reason);
                        }
                    });
                }
            }
            (checks, experimental_fields, registrations)
        }
        Fields::Unit => (Vec::new(), Vec::new(), Vec::new()),
    };

    let checks = if checks.is_empty() {
        quote! { None }
    } else {
        quote! {
            #(#checks)*
            None
        }
    };

    let experimental_fields = if experimental_fields.is_empty() {
        quote! { &[] }
    } else {
        quote! { &[ #(#experimental_fields,)* ] }
    };

    let expanded = quote! {
        #(#registrations)*

        impl #impl_generics #name #ty_generics #where_clause {
            pub(crate) const EXPERIMENTAL_FIELDS: &'static [crate::experimental_api::ExperimentalField] =
                #experimental_fields;
        }

        impl #impl_generics crate::experimental_api::ExperimentalApi for #name #ty_generics #where_clause {
            fn experimental_reason(&self) -> Option<&'static str> {
                #checks
            }
        }
    };
    Ok(expanded)
}

fn derive_for_enum(input: &DeriveInput, data: &DataEnum) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut match_arms = Vec::new();

    for variant in &data.variants {
        for field in &variant.fields {
            reject_experimental(&field.attrs, "enum payload fields")?;
        }
        let variant_name = &variant.ident;
        let pattern = match &variant.fields {
            Fields::Named(_) => quote!(Self::#variant_name { .. }),
            Fields::Unnamed(_) => quote!(Self::#variant_name ( .. )),
            Fields::Unit => quote!(Self::#variant_name),
        };
        let annotation = experimental_annotation(&variant.attrs)?;
        if matches!(annotation, Some(Experimental::Nested)) {
            return Err(syn::Error::new_spanned(
                variant,
                "experimental(nested) is only supported on struct fields",
            ));
        }
        if let Some(Experimental::Reason(reason)) = annotation {
            match_arms.push(quote! {
                #pattern => Some(#reason),
            });
        } else {
            match_arms.push(quote! {
                #pattern => None,
            });
        }
    }

    let expanded = quote! {
        impl #impl_generics crate::experimental_api::ExperimentalApi for #name #ty_generics #where_clause {
            fn experimental_reason(&self) -> Option<&'static str> {
                match self {
                    #(#match_arms)*
                }
            }
        }
    };
    Ok(expanded)
}

enum Experimental {
    Reason(LitStr),
    Nested,
}

fn experimental_annotation(attrs: &[Attribute]) -> syn::Result<Option<Experimental>> {
    let mut annotation = None;
    for attr in attrs
        .iter()
        .filter(|attr| attr.path().is_ident("experimental"))
    {
        if annotation.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "duplicate or conflicting experimental annotations",
            ));
        }
        annotation = Some(attr.parse_args_with(|input: syn::parse::ParseStream| {
            if input.peek(LitStr) {
                return input.parse().map(Experimental::Reason);
            }
            let ident: Ident = input.parse()?;
            if ident == "nested" {
                Ok(Experimental::Nested)
            } else {
                Err(syn::Error::new_spanned(
                    ident,
                    "expected a reason string or nested",
                ))
            }
        })?);
    }
    Ok(annotation)
}

fn reject_experimental(attrs: &[Attribute], location: &str) -> syn::Result<()> {
    if let Some(attr) = attrs
        .iter()
        .find(|attr| attr.path().is_ident("experimental"))
    {
        return Err(syn::Error::new_spanned(
            attr,
            format!("experimental annotations are not supported on {location}"),
        ));
    }
    Ok(())
}

// Read only the naming forms used by the API, leaving unrelated Serde options
// to Serde. Direction-specific naming cannot be represented by one registry key.
fn serde_name(attrs: &[Attribute], key: &str) -> syn::Result<Option<LitStr>> {
    let mut name = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
        let metas = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        )?;
        for meta in metas {
            if !meta.path().is_ident(key) {
                continue;
            }
            if name.is_some() {
                return Err(syn::Error::new_spanned(
                    meta,
                    "duplicate Serde naming attribute",
                ));
            }
            if let syn::Meta::NameValue(value) = &meta
                && let syn::Expr::Lit(lit) = &value.value
                && let syn::Lit::Str(lit) = &lit.lit
            {
                name = Some(lit.clone());
            } else {
                return Err(syn::Error::new_spanned(
                    meta,
                    "ExperimentalApi requires a single Serde name string",
                ));
            }
        }
    }
    Ok(name)
}

fn field_serialized_name(field: &Field, attrs: &[Attribute]) -> syn::Result<Option<String>> {
    let Some(ident) = field.ident.as_ref() else {
        return Ok(None);
    };
    if let Some(name) = serde_name(&field.attrs, "rename")? {
        return Ok(Some(name.value()));
    }
    let name = ident.unraw().to_string();
    let rename_all = serde_name(attrs, "rename_all")?;
    match rename_all.as_ref().map(LitStr::value).as_deref() {
        None | Some("snake_case") => Ok(Some(name)),
        Some("camelCase") => Ok(Some(snake_to_camel(&name))),
        _ => Err(syn::Error::new_spanned(
            rename_all,
            "ExperimentalApi supports snake_case or camelCase fields; use serde(rename) for other names",
        )),
    }
}

fn snake_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for ch in s.chars() {
        if ch == '_' {
            upper = true;
            continue;
        }
        if out.is_empty() {
            out.push(ch.to_ascii_lowercase());
            upper = false;
        } else if upper {
            out.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn index_presence_expr(index: usize, ty: &Type) -> proc_macro2::TokenStream {
    let index = syn::Index::from(index);
    presence_expr_for_access(quote!(self.#index), ty)
}

fn presence_expr_for_access(
    access: proc_macro2::TokenStream,
    ty: &Type,
) -> proc_macro2::TokenStream {
    if option_inner(ty).is_some() {
        return quote! { #access.is_some() };
    }
    if is_vec_like(ty) || is_map_like(ty) {
        return quote! { !#access.is_empty() };
    }
    if is_bool(ty) {
        return quote! { #access };
    }
    quote! { true }
}

fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

fn is_vec_like(ty: &Type) -> bool {
    type_last_ident(ty).is_some_and(|ident| ident == "Vec")
}

fn is_map_like(ty: &Type) -> bool {
    type_last_ident(ty).is_some_and(|ident| ident == "HashMap" || ident == "BTreeMap")
}

fn is_bool(ty: &Type) -> bool {
    type_last_ident(ty).is_some_and(|ident| ident == "bool")
}

fn type_last_ident(ty: &Type) -> Option<Ident> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    type_path.path.segments.last().map(|seg| seg.ident.clone())
}

#[cfg(test)]
mod tests {
    use super::expand;
    use syn::DeriveInput;

    #[test]
    fn rejects_malformed_or_conflicting_annotations() {
        for annotation in [
            "#[experimental(netsed)]",
            "#[experimental]",
            "#[experimental()]",
            "#[experimental(42)]",
            "#[experimental(\"reason\", nested)]",
            "#[experimental(nested,)]",
            "#[experimental(\"reason\")] #[experimental(nested)]",
            "#[experimental(nested)] #[experimental(\"reason\")]",
            "#[experimental(\"reason\")] #[experimental(\"reason\")]",
        ] {
            let input: DeriveInput =
                syn::parse_str(&format!("struct Params {{ {annotation} flag: bool }}")).unwrap();
            assert!(
                expand(&input).is_err(),
                "accepted invalid annotation: {annotation}"
            );
        }
    }

    #[test]
    fn rejects_annotations_on_unsupported_locations() {
        for (source, expected) in [
            (
                "#[experimental(\"reason\")] struct Params;",
                "experimental annotations are not supported on types",
            ),
            (
                "enum Params { Variant { #[experimental(\"reason\")] flag: bool } }",
                "experimental annotations are not supported on enum payload fields",
            ),
            (
                "enum Params { Variant(#[experimental(nested)] bool) }",
                "experimental annotations are not supported on enum payload fields",
            ),
            (
                "enum Params { #[experimental(nested)] Variant }",
                "experimental(nested) is only supported on struct fields",
            ),
        ] {
            let input: DeriveInput = syn::parse_str(source).unwrap();
            assert_eq!(expand(&input).unwrap_err().to_string(), expected);
        }
    }

    #[test]
    fn rejects_unsupported_serde_naming_instead_of_guessing() {
        for (source, expected) in [
            (
                "#[serde(rename_all = \"UPPERCASE\")] struct Params { #[experimental(\"reason\")] flag: bool }",
                "ExperimentalApi supports snake_case or camelCase fields; use serde(rename) for other names",
            ),
            (
                "struct Params { #[serde(rename(serialize = \"out\", deserialize = \"in\"))] #[experimental(\"reason\")] flag: bool }",
                "ExperimentalApi requires a single Serde name string",
            ),
        ] {
            let input: DeriveInput = syn::parse_str(source).unwrap();
            assert_eq!(expand(&input).unwrap_err().to_string(), expected);
        }
    }
}
