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
use syn::parse_macro_input;

#[proc_macro_derive(ExperimentalApi, attributes(experimental))]
pub fn derive_experimental_api(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_experimental_api(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_experimental_api(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    // Validate type-level annotations without changing the type's expansion.
    experimental_annotations(&input.attrs)?;
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
    let type_name_lit = LitStr::new(&name.to_string(), Span::call_site());

    let (checks, registrations) = match &data.fields {
        Fields::Named(named) => {
            let mut checks = Vec::new();
            let mut registrations = Vec::new();
            for field in &named.named {
                let (reason, nested) = experimental_annotations(&field.attrs)?;
                if let Some(reason) = reason {
                    let expr = experimental_presence_expr(field);
                    checks.push(quote! {
                        if #expr {
                            return Some(#reason);
                        }
                    });

                    if let Some(field_name) = field_serialized_name(field) {
                        let field_name_lit = LitStr::new(&field_name, Span::call_site());
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
                } else if nested {
                    let Some(ident) = field.ident.as_ref() else {
                        continue;
                    };
                    checks.push(quote! {
                        if let Some(reason) =
                            crate::experimental_api::ExperimentalApi::experimental_reason(&self.#ident)
                        {
                            return Some(reason);
                        }
                    });
                }
            }
            (checks, registrations)
        }
        Fields::Unnamed(unnamed) => {
            let mut checks = Vec::new();
            let mut registrations = Vec::new();
            for (index, field) in unnamed.unnamed.iter().enumerate() {
                let (reason, nested) = experimental_annotations(&field.attrs)?;
                if let Some(reason) = reason {
                    let expr = index_presence_expr(index, &field.ty);
                    checks.push(quote! {
                        if #expr {
                            return Some(#reason);
                        }
                    });

                    let field_name_lit = LitStr::new(&index.to_string(), Span::call_site());
                    registrations.push(quote! {
                        ::inventory::submit! {
                            crate::experimental_api::ExperimentalField {
                                type_name: #type_name_lit,
                                field_name: #field_name_lit,
                                reason: #reason,
                            }
                        }
                    });
                } else if nested {
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
            (checks, registrations)
        }
        Fields::Unit => (Vec::new(), Vec::new()),
    };

    let checks = if checks.is_empty() {
        quote! { None }
    } else {
        quote! {
            #(#checks)*
            None
        }
    };

    let expanded = quote! {
        #(#registrations)*

        impl crate::experimental_api::ExperimentalApi for #name {
            fn experimental_reason(&self) -> Option<&'static str> {
                #checks
            }
        }
    };
    Ok(expanded)
}

fn derive_for_enum(
    input: &DeriveInput,
    data: &DataEnum,
) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let mut match_arms = Vec::new();

    for variant in &data.variants {
        let variant_name = &variant.ident;
        let pattern = match &variant.fields {
            Fields::Named(_) => quote!(Self::#variant_name { .. }),
            Fields::Unnamed(_) => quote!(Self::#variant_name ( .. )),
            Fields::Unit => quote!(Self::#variant_name),
        };
        let (reason, _) = experimental_annotations(&variant.attrs)?;
        // The expansion uses only the variant's reason, not its payload fields.
        for field in &variant.fields {
            experimental_annotations(&field.attrs)?;
        }
        if let Some(reason) = reason {
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
        impl crate::experimental_api::ExperimentalApi for #name {
            fn experimental_reason(&self) -> Option<&'static str> {
                match self {
                    #(#match_arms)*
                }
            }
        }
    };
    Ok(expanded)
}

fn experimental_annotations(attrs: &[Attribute]) -> syn::Result<(Option<LitStr>, bool)> {
    let mut reason = None;
    let mut nested = false;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("experimental")) {
        if let Ok(literal) = attr.parse_args::<LitStr>() {
            // Preserve the first reason, but still validate later annotations.
            reason.get_or_insert(literal);
        } else if attr
            .parse_args::<Ident>()
            .is_ok_and(|ident| ident == "nested")
        {
            nested = true;
        } else {
            return Err(syn::Error::new_spanned(
                attr,
                "expected #[experimental(\"reason\")] or #[experimental(nested)]",
            ));
        }
    }
    Ok((reason, nested))
}

fn field_serialized_name(field: &Field) -> Option<String> {
    let ident = field.ident.as_ref()?;
    let name = ident.to_string();
    Some(snake_to_camel(&name))
}

fn snake_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for ch in s.chars() {
        if ch == '_' {
            upper = true;
            continue;
        }
        if upper {
            out.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn experimental_presence_expr(field: &Field) -> Option<proc_macro2::TokenStream> {
    let ident = field.ident.as_ref()?;
    Some(presence_expr_for_access(quote!(self.#ident), &field.ty))
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
