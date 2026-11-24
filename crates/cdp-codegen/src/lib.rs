mod emit;
mod model;
mod names;
mod recursion;

#[derive(Debug, thiserror::Error)]
pub enum CodegenError {
    #[error("invalid protocol JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("generated code does not parse: {0}")]
    Syntax(#[from] syn::Error),
    #[error("duplicate item {name} in module {module}")]
    Duplicate { module: String, name: String },
}

pub fn generate(protocol_json: &str) -> Result<String, CodegenError> {
    let protocol: model::Protocol = serde_json::from_str(protocol_json)?;
    let tokens = emit::protocol_tokens(&protocol);
    let file: syn::File = syn::parse2(tokens)?;
    check_duplicates(&file)?;
    Ok(prettyplease::unparse(&file))
}

fn check_duplicates(file: &syn::File) -> Result<(), CodegenError> {
    for item in &file.items {
        if let syn::Item::Mod(m) = item {
            let mut seen = std::collections::HashSet::new();
            for inner in m.content.iter().flat_map(|(_, items)| items) {
                let name = match inner {
                    syn::Item::Struct(s) => s.ident.to_string(),
                    syn::Item::Enum(e) => e.ident.to_string(),
                    _ => continue,
                };
                if !seen.insert(name.clone()) {
                    return Err(CodegenError::Duplicate { module: m.ident.to_string(), name });
                }
            }
        }
    }
    Ok(())
}
