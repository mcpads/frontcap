use super::*;
use crate::bundle::Bundle;
use crate::ids::RunId;

#[test]
fn reference_paths_preserve_numeric_paths_and_reject_unknown_boundaries() {
    let path: Vec<PathStep> = serde_json::from_str(r#"[1,0,"shadow_root",2]"#).unwrap();
    assert_eq!(path, vec![1.into(),0.into(),PathStep::Boundary(PathBoundary::ShadowRoot),2.into()]);
    assert_eq!(serde_json::to_string(&path).unwrap(), r#"[1,0,"shadow_root",2]"#);
    let old: Resolution = serde_json::from_str(r#"{"document":1,"path":[1,0],"frames":[[1,2]]}"#).unwrap();
    assert_eq!(old.path,vec![1.into(),0.into()]);
    assert!(serde_json::from_str::<Vec<PathStep>>(r#"[1,"other_root"]"#).is_err());
}

#[test]
fn launch_inputs_and_commands_round_trip() {
    let runs = tempfile::tempdir().unwrap();
    let b = Bundle::create(runs.path(), &RunId::new("r").unwrap()).unwrap();
    let launch = LaunchInputs { url: "http://127.0.0.1:1/a.html".into(), profile: DeterminismProfile::default(), rules: RuleSet::default(), passthrough_origins: None };
    write_launch(b.root(), &launch).unwrap();
    assert_eq!(read_launch(b.root()).unwrap(), launch);
    assert_eq!(read_commands(b.root()).unwrap(), vec![]);
    let mut log = CommandLog::open(b.root()).unwrap();
    let mut refs = BTreeMap::new();
    refs.insert("e3".to_string(), Resolution { document: 1, frames: vec![], path: vec![1.into(), 0.into(), 2.into()], role: Some("button".into()), name: Some("Save".into()), context: None });
    let line = CommandLine { request_id: RequestId::new("c-1").unwrap(), t_us: VirtualTime(250_000), op: serde_json::json!({"op": "advance", "by_us": 1}), outcome: "advanced".into(), refs, dialogs: Vec::new(), answers: Vec::new(), cancelled_at_us: Some(VirtualTime(250_000)) };
    log.append(&line).unwrap();
    let plain = CommandLine { refs: BTreeMap::new(), request_id: RequestId::new("c-2").unwrap(), ..line.clone() };
    log.append(&plain).unwrap();
    assert_eq!(read_commands(b.root()).unwrap(), vec![line, plain]);
}

#[test]
fn a_text_reference_ends_its_path_with_its_child_node() {
    let path: Vec<PathStep> = serde_json::from_str(r#"[1, 0, {"text": 2}]"#).unwrap();
    assert_eq!(path, vec![1.into(), 0.into(), PathStep::Text { text: 2 }]);
    assert_eq!(serde_json::to_string(&path).unwrap(), r#"[1,0,{"text":2}]"#);
}
