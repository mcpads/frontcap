use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::{Ident, Span};

const RAW_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "override", "pub", "ref",
    "return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where", "while", "abstract",
    "become", "box", "do", "final", "macro", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];
const SUFFIXED: &[&str] = &["self", "Self", "super", "crate"];

pub fn module_name(domain: &str) -> String {
    domain.to_snake_case()
}

pub fn field_ident(name: &str) -> Ident {
    let snake = name.to_snake_case();
    if SUFFIXED.contains(&snake.as_str()) {
        Ident::new(&format!("{snake}_"), Span::call_site())
    } else if RAW_KEYWORDS.contains(&snake.as_str()) {
        Ident::new_raw(&snake, Span::call_site())
    } else {
        Ident::new(&snake, Span::call_site())
    }
}

pub fn type_ident(name: &str) -> Ident {
    Ident::new(name, Span::call_site())
}

pub fn params_name(command: &str) -> String {
    format!("{}Params", command.to_upper_camel_case())
}

pub fn returns_name(command: &str) -> String {
    format!("{}Returns", command.to_upper_camel_case())
}

pub fn event_name(event: &str) -> String {
    format!("{}Event", event.to_upper_camel_case())
}

pub fn inline_enum_name(owner: &str, field: &str) -> String {
    format!("{owner}{}", field.to_upper_camel_case())
}

/// Variant identifiers paired with the original wire strings. Duplicates after case conversion get a
/// numeric suffix in declaration order; a leading digit gets a `V` prefix.
pub fn enum_variants(values: &[String]) -> Vec<(Ident, String)> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    values
        .iter()
        .map(|v| {
            let mut base = v.to_upper_camel_case();
            if base.is_empty() {
                base = "Empty".into();
            }
            if base.starts_with(|c: char| c.is_ascii_digit()) {
                base = format!("V{base}");
            }
            if base == "Self" {
                base = "Self_".into();
            }
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            let name = if *count == 1 { base } else { format!("{base}{count}") };
            (Ident::new(&name, Span::call_site()), v.clone())
        })
        .collect()
}

#[cfg(test)]
#[path = "names_tests.rs"]
mod tests;
