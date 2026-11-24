use super::*;

#[test]
fn a_recorded_hit_without_ancestry_does_not_claim_a_complete_path() {
    let recorded = serde_json::json!({
        "point": [20, 30], "tag": "button", "canvas": false,
        "target": {
            "boundary": { "launch_id": "g-old", "state_revision": 1, "t_us": 0, "context_id": "page", "document_id": "document-1" },
            "ref": "e1-old", "values": {}, "border_quad": null,
            "click": { "ready": false, "point": null, "reason": null }
        }
    });
    let hit: Located = serde_json::from_value(recorded.clone()).unwrap();
    assert!(hit.hit_path.is_none());
    assert_eq!(serde_json::to_value(hit).unwrap(), recorded);
}

fn node(r: &str, role: &str, name: &str, depth: u32) -> SnapshotNode {
    SnapshotNode { geometry: None, r#ref: r.into(), role: role.into(), name: name.into(), value: None, depth, states: vec![], folded: None }
}

/// A heading, a table of `rows` rows (row > cell, cell > button) and a footer link.
fn page(rows: usize) -> Vec<SnapshotNode> {
    let mut nodes = vec![node("h", "heading", "Catalog", 0), node("t", "table", "", 0)];
    for i in 0..rows {
        nodes.push(node(&format!("r{i}"), "row", &format!("Product {i}"), 1));
        nodes.push(node(&format!("c{i}"), "cell", &format!("Product {i}"), 2));
        nodes.push(node(&format!("d{i}"), "cell", "", 2));
        nodes.push(node(&format!("b{i}"), "button", &format!("Add {i}"), 3));
    }
    nodes.push(node("f", "link", "Help", 0));
    nodes
}

#[test]
fn a_run_of_rows_keeps_its_first_rows_and_folds_the_rest_under_their_table() {
    let folded = fold_repeats(&page(200));
    let refs: Vec<&str> = folded.iter().map(|n| n.r#ref.as_str()).collect();
    assert_eq!(refs, ["h", "t", "r0", "c0", "d0", "b0", "r1", "c1", "d1", "b1", "r2", "c2", "d2", "b2", "r3", "f"]);
    assert_eq!(folded[14].folded, Some(Folded { count: 197, scope: Some("t".into()) }));
    assert!(folded.iter().filter(|n| n.r#ref != "r3").all(|n| n.folded.is_none()));
}

#[test]
fn a_short_run_and_siblings_of_other_shapes_stay_whole() {
    assert_eq!(fold_repeats(&page(5)), page(5));
    let mut mixed = page(8);
    // A row with one cell more breaks the run in two: three rows, then four.
    mixed.insert(14, node("x", "cell", "", 2));
    let folded = fold_repeats(&mixed);
    assert_eq!(folded.len(), mixed.len(), "neither run reaches the fold");
}

#[test]
fn rows_kept_whole_have_their_own_repeats_folded() {
    let mut nodes = vec![node("l", "list", "", 0)];
    for i in 0..6 {
        nodes.push(node(&format!("g{i}"), "listitem", "", 1));
        for j in 0..8 {
            nodes.push(node(&format!("g{i}-{j}"), "link", &format!("{i}.{j}"), 2));
        }
    }
    let folded = fold_repeats(&nodes);
    let first = &folded[1..folded.iter().position(|n| n.r#ref == "g1").unwrap()];
    assert_eq!(first.iter().map(|n| n.r#ref.as_str()).collect::<Vec<_>>(), ["g0", "g0-0", "g0-1", "g0-2", "g0-3"]);
    assert_eq!(first[4].folded, Some(Folded { count: 5, scope: Some("g0".into()) }));
    assert_eq!(folded.last().unwrap().folded, Some(Folded { count: 3, scope: Some("l".into()) }));
}

#[test]
fn a_run_of_changes_keeps_its_first_entries() {
    let changed: Vec<SnapshotNode> = (0..200).map(|i| node(&format!("p{i}"), "StaticText", "$1!", 3)).collect();
    let folded = fold_runs(changed);
    assert_eq!(folded.len(), 4);
    assert_eq!(folded[3].folded, Some(Folded { count: 197, scope: None }));
}

/// The page decides the tree's depth: a chain far deeper than any stack folds nothing and keeps
/// every link, and so does a chain whose every link also holds five leaves (a run one short).
#[test]
fn a_tree_of_any_depth_folds_in_one_pass() {
    let chain: Vec<SnapshotNode> = (0..100_000).map(|d| node(&format!("e{d}"), "group", "", d)).collect();
    assert_eq!(fold_repeats(&chain), chain);

    let mut combed = Vec::new();
    for d in 0..100_000u32 {
        combed.push(node(&format!("g{d}"), "group", "", d));
        for leaf in 0..5 {
            combed.push(node(&format!("b{d}-{leaf}"), "button", "", d + 1));
        }
    }
    assert_eq!(fold_repeats(&combed), combed);
}

/// A run with rows on screen keeps the first of those instead of its first rows, and each stretch
/// left out before and after them is one folded node counting it.
#[test]
fn a_scrolled_run_keeps_its_rows_on_screen() {
    let nodes = page(20);
    let runs = fold_candidates(&nodes);
    assert_eq!(runs.len(), 1);
    let rows = &runs[0];
    assert_eq!(rows.len(), 20);
    let on_screen: Vec<usize> = rows[10..15].to_vec();
    let folded = fold_repeats_keeping(&nodes, |i| on_screen.contains(&i));
    let markers: Vec<(String, u32)> = folded.iter().filter_map(|n| n.folded.as_ref().map(|f| (n.r#ref.clone(), f.count))).collect();
    assert_eq!(markers, [("r0".to_string(), 10), ("r13".to_string(), 7)]);
    for kept in ["r10", "r11", "r12", "c10", "b12"] {
        assert!(folded.iter().any(|n| n.r#ref == kept && n.folded.is_none()), "{kept}");
    }
    // With none on screen it folds as without.
    assert_eq!(fold_repeats_keeping(&nodes, |_| false), fold_repeats(&nodes));
}
