use super::*;

fn n(id: &str, parent: Option<&str>, role: &str, name: &str, backend: Option<i64>) -> AxInput {
    AxInput { id: id.into(), parent: parent.map(str::to_string), role: role.into(), name: name.into(), value: None, states: vec![], backend, ignored: false }
}

fn page() -> Vec<AxInput> {
    vec![
        n("1", None, "RootWebArea", "form", Some(1)),
        n("2", Some("1"), "heading", "Profile", Some(2)),
        n("3", Some("2"), "StaticText", "Profile", Some(3)),
        n("4", Some("1"), "generic", "", Some(4)),
        n("5", Some("4"), "button", "Save", Some(5)),
        n("6", Some("5"), "StaticText", "Save", Some(6)),
        n("7", Some("6"), "InlineTextBox", "Save", None),
        AxInput { value: Some("x".into()), ..n("8", Some("4"), "textbox", "Name", Some(8)) },
        n("9", Some("1"), "paragraph", "", Some(9)),
        n("10", Some("9"), "StaticText", "Hello there", Some(10)),
        AxInput { ignored: true, ..n("11", Some("1"), "button", "Hidden", Some(11)) },
    ]
}

#[test]
fn prune_keeps_interactive_structure_and_text() {
    let (kept, omitted) = prune(&page(), None, None, 100);
    let lines: Vec<String> = kept.iter().map(|k| format!("{}{} {:?} {:?}", "  ".repeat(k.depth as usize), k.role, k.name, k.value)).collect();
    assert_eq!(
        lines,
        vec![
            "heading \"Profile\" None".to_string(),
            "button \"Save\" None".to_string(),
            "textbox \"Name\" Some(\"x\")".to_string(),
            "text \"Hello there\" None".to_string(),
        ]
    );
    assert_eq!(omitted, 0);
}

#[test]
fn prune_truncates_with_an_omitted_count() {
    let mut nodes = vec![n("r", None, "RootWebArea", "", Some(1))];
    for i in 0..50 {
        nodes.push(n(&format!("b{i}"), Some("r"), "button", &format!("B{i}"), Some(100 + i)));
    }
    let (kept, omitted) = prune(&nodes, None, None, 10);
    assert_eq!((kept.len(), omitted), (10, 40));
}

#[test]
fn prune_from_a_scope_and_to_a_depth() {
    let (kept, _) = prune(&page(), Some(4), None, 100);
    assert_eq!(kept.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["Save", "Name"]);
    let (shallow, _) = prune(&page(), None, Some(0), 100);
    assert!(shallow.iter().all(|k| k.depth == 0));
}

#[test]
fn diff_by_reference() {
    let node = |r: &str, name: &str| SnapshotNode { r#ref: r.into(), role: "button".into(), name: name.into(), value: None, depth: 0, states: vec![], geometry: None, folded: None };
    let before = vec![node("e1", "Save"), node("e2", "Cancel")];
    let after = vec![node("e1", "Saved"), node("e3", "Undo")];
    let NodeDiff { added, removed, changed, replaced } = diff_nodes(&before, &after);
    assert_eq!(added.iter().map(|n| n.r#ref.as_str()).collect::<Vec<_>>(), ["e3"]);
    assert_eq!(removed, vec!["e2".to_string()]);
    assert_eq!(changed.iter().map(|n| (n.r#ref.as_str(), n.name.as_str())).collect::<Vec<_>>(), [("e1", "Saved")]);
    assert!(replaced.is_empty());
}

/// Consumer runs: a list re-rendered with the same items read as every item removed and added.
#[test]
fn re_rendered_nodes_are_replaced_not_removed_and_added() {
    let node = |r: &str, name: &str| SnapshotNode { r#ref: r.into(), role: "listitem".into(), name: name.into(), value: None, depth: 1, states: vec![], geometry: None, folded: None };
    let before = vec![node("e1", "Milk"), node("e2", "Eggs"), node("e3", "Tea")];
    let after = vec![node("e4", "Milk"), node("e5", "Eggs"), node("e6", "Bread")];
    let NodeDiff { added, removed, changed, replaced } = diff_nodes(&before, &after);
    assert_eq!(replaced, [("e1".to_string(), "e4".to_string()), ("e2".to_string(), "e5".to_string())]);
    assert_eq!(added.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["Bread"]);
    assert_eq!(removed, vec!["e3".to_string()]);
    assert!(changed.is_empty());
}

#[test]
fn the_largest_depth_is_unbounded_not_an_overflow() {
    assert_eq!(prune(&page(), None, Some(u32::MAX), 100), prune(&page(), None, None, 100));
}

/// Fresh-consumer run 4: a heading "To do (2)" read as "To do (", "2", ")", and a paragraph's run of
/// text as separate pieces.
#[test]
fn text_pieces_read_as_one_text() {
    let nodes = vec![
        n("1", None, "RootWebArea", "board", Some(1)),
        n("2", Some("1"), "heading", "To do (2)", Some(2)),
        n("3", Some("2"), "StaticText", "To do (", Some(3)),
        n("4", Some("2"), "StaticText", "2", Some(4)),
        n("5", Some("2"), "StaticText", ")", Some(5)),
        n("6", Some("1"), "paragraph", "", Some(6)),
        n("7", Some("6"), "StaticText", "Volume: ", Some(7)),
        n("8", Some("6"), "StaticText", "100", Some(8)),
        n("9", Some("1"), "button", "Save", Some(9)),
        n("10", Some("1"), "StaticText", "after", Some(10)),
    ];
    let (kept, omitted) = prune(&nodes, None, None, 100);
    let lines: Vec<String> = kept.iter().map(|k| format!("{} {:?}", k.role, k.name)).collect();
    assert_eq!(lines, ["heading \"To do (2)\"", "text \"Volume: 100\"", "button \"Save\"", "text \"after\""]);
    assert_eq!(omitted, 0);
    let (separate, _) = prune_with_text_merging(&nodes, None, None, 100, false);
    let text: Vec<_> = separate.iter().filter(|n| n.role == "text").map(|n| (n.backend, n.name.as_str())).collect();
    assert_eq!(text, [(Some(7), "Volume: "), (Some(8), "100"), (Some(10), "after")], "geometry must not assign one text node's box to its siblings");
}

#[test]
fn stitch_places_each_frame_under_the_element_that_shows_it() {
    let top = vec![
        n("1", None, "RootWebArea", "page", Some(1)),
        n("2", Some("1"), "button", "Before", Some(2)),
        n("3", Some("1"), "Iframe", "", Some(3)),
        n("4", Some("1"), "button", "After", Some(4)),
    ];
    // The frame's ids repeat the top frame's; its root has no parent in its own tree.
    let child = vec![n("1", None, "RootWebArea", "widget", Some(20)), n("2", Some("1"), "button", "Inside", Some(21))];
    let lost = vec![n("1", None, "RootWebArea", "lost", Some(30)), n("2", Some("1"), "button", "Elsewhere", Some(31))];
    let all = stitch(vec![(None, top), (Some(3), child), (Some(99), lost)]);
    let (kept, _) = prune(&all, None, None, 100);
    let lines: Vec<String> = kept.iter().map(|k| format!("{} {} {:?}", k.depth, k.role, k.name)).collect();
    assert_eq!(lines, ["0 button \"Before\"", "0 iframe \"\"", "1 button \"Inside\"", "0 button \"After\"", "0 button \"Elsewhere\""]);
    // Scoped to the frame element, the frame and its content.
    let (kept, _) = prune(&all, Some(3), None, 100);
    assert_eq!(kept.iter().map(|k| k.role.as_str()).collect::<Vec<_>>(), vec!["iframe", "button"]);
}

#[test]
fn a_frame_that_shows_nothing_is_kept_and_its_title_does_not_hide_its_text() {
    let top = vec![
        n("1", None, "RootWebArea", "page", Some(1)),
        n("2", Some("1"), "Iframe", "Signing pad", Some(2)),
        n("3", Some("1"), "Iframe", "Attachments", Some(3)),
    ];
    let attachments = vec![n("1", None, "RootWebArea", "Attachments", Some(20)), n("2", Some("1"), "StaticText", "Attachments", Some(21))];
    let all = stitch(vec![(None, top), (Some(3), attachments)]);
    let (kept, _) = prune(&all, None, None, 100);
    let lines: Vec<String> = kept.iter().map(|k| format!("{} {} {:?}", k.depth, k.role, k.name)).collect();
    assert_eq!(lines, ["0 iframe \"Signing pad\"", "0 iframe \"Attachments\"", "1 text \"Attachments\""]);
}
