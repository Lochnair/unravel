// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use proc_macro::TokenStream;
use quote::quote;
use syn::ext::IdentExt;
use syn::{Data, DeriveInput, Field, Fields, FieldsNamed, LitStr, Token, parse_macro_input};

/// Generates `UciSectionSchema` from a settings struct.
///
/// Every named field without `#[uci(skip)]` is mapped one-to-one:
///
/// ```text
/// Rust field name -> UCI option name
/// Rust field name -> config-rs field name
/// ```
///
/// `#[uci(rename = "...")]` on a field changes the UCI option name only.
///
/// `#[uci(sections = "...")]` on a `Vec<T>` field collects every section of
/// that type owned by the selected instance into the field. `T` must derive
/// `UciSection` too.
///
/// Container attributes:
///
/// ```ignore
/// // Options read by something else (the init script, LuCI); repeatable.
/// #[uci(external = "use_syslog")]
///
/// // The named field's value selects a sub-table, and every option the
/// // schema does not know is passed through to it.
/// #[uci(passthrough_key = "name")]
/// ```
#[proc_macro_derive(UciSection, attributes(uci))]
pub fn derive_uci_section(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match expand_uci_section(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Generates `UciConfigSchema` from the root settings struct.
///
/// Required container attribute:
///
/// ```ignore
/// #[uci(package = "unravel")]
/// ```
///
/// Section fields derive `UciSection`. One section is the named instance.
/// Other sections belong to it through their `instance` option.
/// Root settings stored directly on the instance use `instance_option`.
/// ```ignore
/// #[uci(section = "instance", instance)]
/// network: NetworkSettings,
/// #[uci(instance_option)]
/// dry_run: bool,
/// #[uci(section)]
/// law: LawSettings,
/// ```
#[proc_macro_derive(UciConfig, attributes(uci))]
pub fn derive_uci_config(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match expand_uci_config(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_uci_section(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let struct_name = &input.ident;
    let fields = require_named_fields(input)?;

    let container = parse_section_container(input)?;
    let mut mappings = Vec::with_capacity(fields.named.len());
    let mut repeated = Vec::new();
    let mut field_names = Vec::with_capacity(fields.named.len());

    for field in &fields.named {
        let attributes = parse_field(field)?;
        if attributes.skip {
            continue;
        }

        let field_ident = field
            .ident
            .as_ref()
            .expect("named fields always have identifiers");

        let field_name = field_ident.unraw().to_string();

        if let Some(uci_section) = attributes.sections {
            let element = vec_element(&field.ty).ok_or_else(|| {
                syn::Error::new_spanned(&field.ty, "`sections` requires a `Vec<T>` field")
            })?;
            let config_field = LitStr::new(&field_name, field_ident.span());
            repeated.push(quote! {
                crate::settings::uci_schema::UciRepeatedSectionMapping {
                    uci_section: #uci_section,
                    field: #config_field,
                    options: <
                        #element as
                        crate::settings::uci_schema::UciSectionSchema
                    >::UCI_OPTIONS,
                }
            });
            continue;
        }

        let uci_option = attributes
            .rename
            .unwrap_or_else(|| LitStr::new(&field_name, field_ident.span()));
        let config_field = LitStr::new(&field_name, field_ident.span());
        field_names.push(field_name);

        mappings.push(quote! {
            crate::settings::uci_schema::UciOptionMapping {
                uci_option: #uci_option,
                config_field: #config_field,
            }
        });
    }

    let external = &container.external;
    let passthrough_key = match &container.passthrough_key {
        Some(key) if field_names.contains(&key.value()) => quote! { Some(#key) },
        Some(key) => {
            return Err(syn::Error::new(
                key.span(),
                "`passthrough_key` must name a field of this struct that is not skipped",
            ));
        }
        None => quote! { None },
    };

    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        #[cfg(all(feature = "uci", unix))]
        #[automatically_derived]
        impl #impl_generics
            crate::settings::uci_schema::UciSectionSchema
            for #struct_name #type_generics
            #where_clause
        {
            const UCI_OPTIONS: &'static [
                crate::settings::uci_schema::UciOptionMapping
            ] = &[
                #(#mappings),*
            ];

            const UCI_EXTERNAL_OPTIONS: &'static [&'static str] = &[
                #(#external),*
            ];

            const UCI_PASSTHROUGH_KEY: Option<&'static str> = #passthrough_key;

            const UCI_REPEATED: &'static [
                crate::settings::uci_schema::UciRepeatedSectionMapping
            ] = &[
                #(#repeated),*
            ];
        }
    })
}

#[derive(Default)]
struct FieldAttributes {
    skip: bool,
    rename: Option<LitStr>,
    sections: Option<LitStr>,
}

/// `T` of a `Vec<T>` type, as written.
fn vec_element(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Vec" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        syn::GenericArgument::Type(element) if arguments.args.len() == 1 => Some(element),
        _ => None,
    }
}

fn parse_field(field: &Field) -> syn::Result<FieldAttributes> {
    let mut attributes = FieldAttributes::default();

    for attribute in &field.attrs {
        if !attribute.path().is_ident("uci") {
            continue;
        }

        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                if attributes.skip {
                    return Err(meta.error("duplicate `skip` attribute"));
                }
                attributes.skip = true;
                return Ok(());
            }

            if meta.path.is_ident("rename") {
                if attributes.rename.is_some() {
                    return Err(meta.error("duplicate `rename` attribute"));
                }
                attributes.rename = Some(parse_non_empty(&meta, "rename")?);
                return Ok(());
            }

            if meta.path.is_ident("sections") {
                if attributes.sections.is_some() {
                    return Err(meta.error("duplicate `sections` attribute"));
                }
                attributes.sections = Some(parse_non_empty(&meta, "sections")?);
                return Ok(());
            }

            Err(meta.error("expected `skip`, `rename = \"...\"` or `sections = \"...\"`"))
        })?;
    }

    let set = [
        attributes.skip,
        attributes.rename.is_some(),
        attributes.sections.is_some(),
    ];
    if set.iter().filter(|&&set| set).count() > 1 {
        return Err(syn::Error::new_spanned(
            field,
            "`skip`, `rename` and `sections` cannot be combined",
        ));
    }

    Ok(attributes)
}

#[derive(Default)]
struct SectionContainerAttributes {
    external: Vec<LitStr>,
    passthrough_key: Option<LitStr>,
}

fn parse_section_container(input: &DeriveInput) -> syn::Result<SectionContainerAttributes> {
    let mut attributes = SectionContainerAttributes::default();

    for attribute in &input.attrs {
        if !attribute.path().is_ident("uci") {
            continue;
        }

        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("external") {
                attributes
                    .external
                    .push(parse_non_empty(&meta, "external")?);
                return Ok(());
            }

            if meta.path.is_ident("passthrough_key") {
                if attributes.passthrough_key.is_some() {
                    return Err(meta.error("duplicate `passthrough_key` attribute"));
                }
                attributes.passthrough_key = Some(parse_non_empty(&meta, "passthrough_key")?);
                return Ok(());
            }

            Err(meta.error("expected `external = \"...\"` or `passthrough_key = \"...\"`"))
        })?;
    }

    Ok(attributes)
}

fn parse_non_empty(meta: &syn::meta::ParseNestedMeta<'_>, name: &str) -> syn::Result<LitStr> {
    let value: LitStr = meta.value()?.parse()?;

    if value.value().is_empty() {
        return Err(syn::Error::new(
            value.span(),
            format!("`{name}` cannot be empty"),
        ));
    }

    Ok(value)
}

fn expand_uci_config(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let struct_name = &input.ident;
    let fields = require_named_fields(input)?;
    let package = parse_package_name(input)?;

    let mut sections = Vec::with_capacity(fields.named.len());
    let mut instance_options = Vec::new();
    let mut instance_section_seen = false;

    for field in &fields.named {
        let field_ident = field
            .ident
            .as_ref()
            .expect("named fields always have identifiers");

        let field_type = &field.ty;

        let config_section_name = field_ident.unraw().to_string();
        let config_section_name = LitStr::new(&config_section_name, field_ident.span());

        let attributes = parse_config_field(field, &config_section_name)?;
        let (uci_section_name, instance) = match attributes {
            ConfigField::InstanceOption => {
                instance_options.push(quote! {
                    crate::settings::uci_schema::UciOptionMapping {
                        uci_option: #config_section_name,
                        config_field: #config_section_name,
                    }
                });
                continue;
            }
            ConfigField::Section { name, instance } => (name, instance),
        };

        if instance {
            if instance_section_seen {
                return Err(syn::Error::new_spanned(
                    field,
                    "only one section can be the instance section",
                ));
            }
            instance_section_seen = true;
        }

        sections.push(quote! {
            crate::settings::uci_schema::UciSectionMapping {
                uci_section: #uci_section_name,
                config_section: #config_section_name,
                instance: #instance,
                options: <
                    #field_type as
                    crate::settings::uci_schema::UciSectionSchema
                >::UCI_OPTIONS,
                external: <
                    #field_type as
                    crate::settings::uci_schema::UciSectionSchema
                >::UCI_EXTERNAL_OPTIONS,
                passthrough_key: <
                    #field_type as
                    crate::settings::uci_schema::UciSectionSchema
                >::UCI_PASSTHROUGH_KEY,
                repeated: <
                    #field_type as
                    crate::settings::uci_schema::UciSectionSchema
                >::UCI_REPEATED,
            }
        });
    }

    if !instance_section_seen {
        return Err(syn::Error::new_spanned(
            struct_name,
            "UciConfig requires an instance section",
        ));
    }

    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        #[cfg(all(feature = "uci", unix))]
        #[automatically_derived]
        impl #impl_generics
            crate::settings::uci_schema::UciConfigSchema
            for #struct_name #type_generics
            #where_clause
        {
            const UCI_PACKAGE: &'static str = #package;

            const UCI_INSTANCE_OPTIONS: &'static [
                crate::settings::uci_schema::UciOptionMapping
            ] = &[
                #(#instance_options),*
            ];

            const UCI_SECTIONS: &'static [
                crate::settings::uci_schema::UciSectionMapping
            ] = &[
                #(#sections),*
            ];
        }
    })
}

fn require_named_fields(input: &DeriveInput) -> syn::Result<&FieldsNamed> {
    match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => Ok(fields),

            Fields::Unnamed(_) | Fields::Unit => Err(syn::Error::new_spanned(
                input,
                "this derive only supports structs with named fields",
            )),
        },

        Data::Enum(_) | Data::Union(_) => Err(syn::Error::new_spanned(
            input,
            "this derive only supports structs",
        )),
    }
}

fn parse_package_name(input: &DeriveInput) -> syn::Result<LitStr> {
    let mut package = None;

    for attribute in &input.attrs {
        if !attribute.path().is_ident("uci") {
            continue;
        }

        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("package") {
                if package.is_some() {
                    return Err(meta.error("duplicate `package` attribute"));
                }

                let value: LitStr = meta.value()?.parse()?;

                if value.value().is_empty() {
                    return Err(syn::Error::new(value.span(), "`package` cannot be empty"));
                }

                package = Some(value);
                return Ok(());
            }

            Err(meta.error("expected `package = \"...\"`"))
        })?;
    }

    package.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "UciConfig requires `#[uci(package = \"...\")]`",
        )
    })
}

enum ConfigField {
    Section { name: LitStr, instance: bool },
    InstanceOption,
}

fn parse_config_field(field: &Field, default_name: &LitStr) -> syn::Result<ConfigField> {
    let mut section = None;
    let mut instance = false;
    let mut instance_option = false;

    for attribute in &field.attrs {
        if !attribute.path().is_ident("uci") {
            continue;
        }

        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("section") {
                if section.is_some() {
                    return Err(meta.error("duplicate `section` attribute"));
                }
                section = Some(if meta.input.peek(Token![=]) {
                    parse_non_empty(&meta, "section")?
                } else {
                    default_name.clone()
                });
                return Ok(());
            }

            if meta.path.is_ident("instance") {
                if instance {
                    return Err(meta.error("duplicate `instance` attribute"));
                }
                instance = true;
                return Ok(());
            }

            if meta.path.is_ident("instance_option") {
                if instance_option {
                    return Err(meta.error("duplicate `instance_option` attribute"));
                }
                instance_option = true;
                return Ok(());
            }

            Err(meta.error("expected `section`, `instance` or `instance_option`"))
        })?;
    }

    match (section, instance_option, instance) {
        (Some(name), false, instance) => Ok(ConfigField::Section { name, instance }),
        (None, true, false) => Ok(ConfigField::InstanceOption),
        _ => Err(syn::Error::new_spanned(
            field,
            "expected `#[uci(section)]`, optionally with `instance`, or `#[uci(instance_option)]`",
        )),
    }
}
