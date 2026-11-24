use sha2::{Digest, Sha256};

/// JSON text with object keys sorted at every depth and no insignificant whitespace. Written by hand so
/// the result does not depend on serde_json's `preserve_order` feature.
pub fn to_canonical_string(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String((*k).clone()).to_string());
                out.push(':');
                write(&map[*k], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

pub fn canonical_of<T: serde::Serialize>(value: &T) -> String {
    to_canonical_string(&serde_json::to_value(value).expect("serializable value"))
}

pub fn sha256_hex(text: &str) -> String {
    sha256_bytes_hex(text.as_bytes())
}

pub fn sha256_bytes_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[path = "canonical_tests.rs"]
mod tests;
