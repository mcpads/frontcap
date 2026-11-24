use crate::model::{Domain, Items, Property, Protocol};
use crate::names::*;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::BTreeSet;

pub struct Ctx<'a> {
    pub domain: &'a str,
    pub boxed: &'a BTreeSet<(String, String, String)>,
}

fn doc(text: &Option<String>) -> TokenStream {
    match text {
        Some(t) => quote! { #[doc = #t] },
        None => quote! {},
    }
}

fn reference_path(domain: &str, reference: &str) -> TokenStream {
    match reference.split_once('.') {
        Some((d, t)) if d != domain => {
            let m = format_ident!("{}", module_name(d));
            let t = type_ident(t);
            quote! { super::#m::#t }
        }
        Some((_, t)) => {
            let t = type_ident(t);
            quote! { #t }
        }
        None => {
            let t = type_ident(reference);
            quote! { #t }
        }
    }
}

fn primitive(kind: &str) -> TokenStream {
    match kind {
        "string" => quote! { String },
        "integer" => quote! { i64 },
        "number" => quote! { f64 },
        "boolean" => quote! { bool },
        "binary" => quote! { crate::Binary },
        "any" => quote! { serde_json::Value },
        "object" => quote! { serde_json::Map<String, serde_json::Value> },
        other => panic!("unknown primitive type {other}"),
    }
}

fn item_type(ctx: &Ctx, items: &Items, inline_enum: &str, extra: &mut Vec<TokenStream>) -> TokenStream {
    if let Some(r) = &items.reference {
        return reference_path(ctx.domain, r);
    }
    if let Some(values) = &items.enum_values {
        extra.push(enum_tokens(inline_enum, values, &None));
        let i = type_ident(inline_enum);
        return quote! { #i };
    }
    primitive(items.kind.as_deref().unwrap_or("any"))
}

/// Field type without Option/Box wrapping. Inline enums are appended to `extra`.
fn field_base_type(ctx: &Ctx, owner: &str, p: &Property, extra: &mut Vec<TokenStream>) -> TokenStream {
    if let Some(r) = &p.reference {
        return reference_path(ctx.domain, r);
    }
    if let Some(values) = &p.enum_values {
        let name = inline_enum_name(owner, &p.name);
        extra.push(enum_tokens(&name, values, &p.description));
        let i = type_ident(&name);
        return quote! { #i };
    }
    match p.kind.as_deref() {
        Some("array") => {
            let items = p.items.as_ref().expect("array without items");
            let inner = item_type(ctx, items, &inline_enum_name(owner, &p.name), extra);
            quote! { Vec<#inner> }
        }
        Some(kind) => primitive(kind),
        None => quote! { serde_json::Value },
    }
}

pub fn enum_tokens(name: &str, values: &[String], description: &Option<String>) -> TokenStream {
    let ident = type_ident(name);
    let d = doc(description);
    let variants = enum_variants(values).into_iter().map(|(v, wire)| quote! { #[serde(rename = #wire)] #v, });
    quote! {
        #d
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub enum #ident { #(#variants)* }
    }
}

pub struct StructSpec<'a> {
    pub name: String,
    pub description: &'a Option<String>,
    pub fields: &'a [Property],
    pub constructor: bool,
}

pub fn struct_tokens(ctx: &Ctx, spec: &StructSpec, extra: &mut Vec<TokenStream>) -> TokenStream {
    let ident = type_ident(&spec.name);
    let d = doc(spec.description);
    let mut fields = Vec::new();
    let mut required_args = Vec::new();
    let mut init = Vec::new();
    for p in spec.fields {
        let fid = field_ident(&p.name);
        let wire = &p.name;
        let fd = doc(&p.description);
        let mut ty = field_base_type(ctx, &spec.name, p, extra);
        if ctx.boxed.contains(&(ctx.domain.to_string(), spec.name.clone(), p.name.clone())) {
            ty = quote! { Box<#ty> };
        }
        if p.optional {
            fields.push(quote! {
                #fd
                #[serde(rename = #wire, default, skip_serializing_if = "Option::is_none")]
                pub #fid: Option<#ty>,
            });
            init.push(quote! { #fid: None, });
        } else {
            fields.push(quote! { #fd #[serde(rename = #wire)] pub #fid: #ty, });
            required_args.push(quote! { #fid: #ty });
            init.push(quote! { #fid, });
        }
    }
    let all_optional = spec.fields.iter().all(|p| p.optional);
    let derive_default = if all_optional { quote! { Default, } } else { quote! {} };
    let ctor = if spec.constructor {
        quote! {
            impl #ident {
                #[allow(clippy::too_many_arguments)]
                pub fn new(#(#required_args),*) -> Self { Self { #(#init)* } }
            }
        }
    } else {
        quote! {}
    };
    quote! {
        #d
        #[derive(Debug, Clone, PartialEq, #derive_default serde::Serialize, serde::Deserialize)]
        pub struct #ident { #(#fields)* }
        #ctor
    }
}

fn domain_tokens(domain: &Domain, boxed: &BTreeSet<(String, String, String)>) -> TokenStream {
    let ctx = Ctx { domain: &domain.domain, boxed };
    let mut items = Vec::new();
    let mut extra = Vec::new();
    for t in &domain.types {
        match (t.kind.as_str(), &t.enum_values, &t.properties) {
            ("string", Some(values), _) => items.push(enum_tokens(&t.id, values, &t.description)),
            ("object", _, Some(props)) => items.push(struct_tokens(
                &ctx,
                &StructSpec { name: t.id.clone(), description: &t.description, fields: props, constructor: false },
                &mut extra,
            )),
            ("array", _, _) => {
                let ident = type_ident(&t.id);
                let d = doc(&t.description);
                let inner = item_type(&ctx, t.items.as_ref().expect("array typedef without items"),
                                      &format!("{}Item", t.id), &mut extra);
                items.push(quote! {
                    #d
                    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
                    #[serde(transparent)]
                    pub struct #ident(pub Vec<#inner>);
                });
            }
            (kind, _, _) => {
                let ident = type_ident(&t.id);
                let d = doc(&t.description);
                let inner = primitive(kind);
                items.push(quote! {
                    #d
                    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
                    #[serde(transparent)]
                    pub struct #ident(pub #inner);
                });
            }
        }
    }
    for c in &domain.commands {
        let params = params_name(&c.name);
        let returns = returns_name(&c.name);
        let method = format!("{}.{}", domain.domain, c.name);
        items.push(struct_tokens(
            &ctx,
            &StructSpec { name: params.clone(), description: &c.description, fields: &c.parameters, constructor: true },
            &mut extra,
        ));
        items.push(struct_tokens(
            &ctx,
            &StructSpec { name: returns.clone(), description: &None, fields: &c.returns, constructor: false },
            &mut extra,
        ));
        let p = type_ident(&params);
        let r = type_ident(&returns);
        items.push(quote! {
            impl crate::Command for #p {
                const METHOD: &'static str = #method;
                type Returns = #r;
            }
        });
    }
    for e in &domain.events {
        let name = event_name(&e.name);
        let wire = format!("{}.{}", domain.domain, e.name);
        items.push(struct_tokens(
            &ctx,
            &StructSpec { name: name.clone(), description: &e.description, fields: &e.parameters, constructor: false },
            &mut extra,
        ));
        let i = type_ident(&name);
        items.push(quote! { impl crate::Event for #i { const NAME: &'static str = #wire; } });
    }
    let m = format_ident!("{}", module_name(&domain.domain));
    let d = doc(&domain.description);
    quote! {
        #d
        #[allow(non_camel_case_types, clippy::all)]
        pub mod #m {
            #(#items)*
            #(#extra)*
        }
    }
}

pub fn protocol_tokens(protocol: &Protocol) -> TokenStream {
    let boxed = crate::recursion::boxed_fields(protocol);
    let domains = protocol.domains.iter().map(|d| domain_tokens(d, &boxed));
    quote! { #(#domains)* }
}

#[cfg(test)]
#[path = "emit_tests.rs"]
mod tests;
