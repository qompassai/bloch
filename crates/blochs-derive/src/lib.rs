//! Derive tool declarations and refusal codes without owning the MCP transport.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::quote;
use std::collections::HashSet;
use syn::{parse_macro_input, Data, DeriveInput, Error, Expr, Fields, LitStr, Result, Variant};

/// Generate `definitions()` from an enum of named, documented tool declarations.
#[proc_macro_derive(ToolDefinitions, attributes(tool))]
pub fn tool_definitions(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_tools(&input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// Generate code and message accessors and conversion into a prefixed String.
#[proc_macro_derive(Refusal, attributes(refusal))]
pub fn refusal(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_refusal(&input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn variants(input: &DeriveInput) -> Result<impl Iterator<Item = &Variant>> {
    match &input.data {
        Data::Enum(data) if !data.variants.is_empty() && input.generics.params.is_empty() => {
            Ok(data.variants.iter())
        }
        _ => Err(Error::new_spanned(
            &input.ident,
            "expected a nonempty, non-generic enum",
        )),
    }
}

fn set<T>(slot: &mut Option<T>, value: T, meta: &syn::meta::ParseNestedMeta) -> Result<()> {
    if slot.replace(value).is_some() {
        return Err(meta.error("duplicate declaration field"));
    }
    Ok(())
}

fn expand_tools(input: &DeriveInput) -> Result<Tokens> {
    let mut entries = Vec::new();
    let mut names = HashSet::new();
    for variant in variants(input)? {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new_spanned(variant, "expected a unit tool variant"));
        }
        let mut name: Option<LitStr> = None;
        let mut schema: Option<Expr> = None;
        let mut annotations: Option<Expr> = None;
        let mut docs = Vec::new();
        for attr in &variant.attrs {
            if attr.path().is_ident("tool") {
                attr.parse_nested_meta(|meta| {
                    let value = meta.value()?;
                    if meta.path.is_ident("name") {
                        set(&mut name, value.parse()?, &meta)
                    } else if meta.path.is_ident("schema") {
                        set(&mut schema, value.parse()?, &meta)
                    } else if meta.path.is_ident("annotations") {
                        set(&mut annotations, value.parse()?, &meta)
                    } else {
                        Err(meta.error("expected name, schema or annotations"))
                    }
                })?;
            } else if attr.path().is_ident("doc") {
                if let syn::Meta::NameValue(doc) = &attr.meta {
                    if let Expr::Lit(expr) = &doc.value {
                        if let syn::Lit::Str(line) = &expr.lit {
                            docs.push(
                                line.value()
                                    .strip_prefix(' ')
                                    .unwrap_or(&line.value())
                                    .to_owned(),
                            );
                        }
                    }
                }
            }
        }
        let name = name
            .filter(|name| !name.value().trim().is_empty())
            .ok_or_else(|| Error::new_spanned(variant, "missing tool name"))?;
        if !names.insert(name.value()) {
            return Err(Error::new_spanned(name, "duplicate tool name"));
        }
        let schema = schema.ok_or_else(|| Error::new_spanned(variant, "missing tool schema"))?;
        let description = docs.join("\n");
        if description.trim().is_empty() {
            return Err(Error::new_spanned(
                variant,
                "missing tool description doc comment",
            ));
        }
        let annotations = annotations.map(|expr| quote! { , "annotations": (#expr) });
        entries.push(quote! {
            { "name": #name, "description": #description, "inputSchema": (#schema) #annotations }
        });
    }
    let ident = &input.ident;
    Ok(quote! {
        impl #ident {
            #[doc = "Build this declared tools/list array in declaration order."]
            pub fn definitions() -> ::serde_json::Value {
                ::serde_json::json!([#(#entries),*])
            }
        }
    })
}

fn expand_refusal(input: &DeriveInput) -> Result<Tokens> {
    let mut codes = Vec::new();
    let mut names = Vec::new();
    for variant in variants(input)? {
        if !matches!(&variant.fields, Fields::Unnamed(fields) if fields.unnamed.len() == 1) {
            return Err(Error::new_spanned(
                variant,
                "expected one String message field",
            ));
        }
        let mut code: Option<LitStr> = None;
        for attr in &variant.attrs {
            if attr.path().is_ident("refusal") {
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("code") {
                        set(&mut code, meta.value()?.parse()?, &meta)
                    } else {
                        Err(meta.error("expected code"))
                    }
                })?;
            }
        }
        let code = code
            .filter(|code| !code.value().trim().is_empty())
            .ok_or_else(|| Error::new_spanned(variant, "missing refusal code"))?;
        codes.push(code);
        names.push(&variant.ident);
    }
    let ident = &input.ident;
    Ok(quote! {
        impl #ident {
            #[doc = "Return the declared refusal code."]
            pub fn code(&self) -> &'static str {
                match self { #(Self::#names(_) => #codes),* }
            }
            #[doc = "Return the refusal message without its code prefix."]
            pub fn message(&self) -> &str {
                match self { #(Self::#names(message) => message.as_str()),* }
            }
        }
        impl ::core::convert::From<#ident> for ::std::string::String {
            fn from(refusal: #ident) -> Self {
                ::std::format!("{}: {}", refusal.code(), refusal.message())
            }
        }
    })
}
