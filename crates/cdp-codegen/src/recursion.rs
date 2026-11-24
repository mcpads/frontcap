use crate::model::Protocol;
use std::collections::{BTreeSet, HashMap, HashSet};

type Key = (String, String); // (domain, type id)

fn resolve(domain: &str, reference: &str) -> Key {
    match reference.split_once('.') {
        Some((d, t)) => (d.to_string(), t.to_string()),
        None => (domain.to_string(), reference.to_string()),
    }
}

/// Fields (domain, type, field) whose non-`Vec` reference can reach the owning struct again.
pub fn boxed_fields(protocol: &Protocol) -> BTreeSet<(String, String, String)> {
    let mut objects: HashSet<Key> = HashSet::new();
    let mut edges: HashMap<Key, Vec<(String, Key)>> = HashMap::new();
    for d in &protocol.domains {
        for t in &d.types {
            if t.kind == "object" && t.properties.is_some() {
                objects.insert((d.domain.clone(), t.id.clone()));
            }
        }
    }
    for d in &protocol.domains {
        for t in &d.types {
            let owner = (d.domain.clone(), t.id.clone());
            for p in t.properties.iter().flatten() {
                if let Some(r) = &p.reference {
                    let target = resolve(&d.domain, r);
                    if objects.contains(&target) {
                        edges.entry(owner.clone()).or_default().push((p.name.clone(), target));
                    }
                }
            }
        }
    }
    let reaches = |from: &Key, to: &Key| -> bool {
        let mut stack = vec![from.clone()];
        let mut seen = HashSet::new();
        while let Some(k) = stack.pop() {
            if &k == to {
                return true;
            }
            if !seen.insert(k.clone()) {
                continue;
            }
            for (_, next) in edges.get(&k).into_iter().flatten() {
                stack.push(next.clone());
            }
        }
        false
    };
    let mut out = BTreeSet::new();
    for (owner, fields) in &edges {
        for (field, target) in fields {
            if reaches(target, owner) {
                out.insert((owner.0.clone(), owner.1.clone(), field.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "recursion_tests.rs"]
mod tests;
