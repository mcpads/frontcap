use super::*;
use serde_json::json;

#[test]
fn pages_reconstruct_unicode_and_escaped_json_and_select_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let launch = LaunchId::new("generation").unwrap();
    let value = json!({"value": ["한글🙂\n\"\\".repeat(5000), {"a/b": [17, 23]}]});
    let first = bound(dir.path(), &launch, value.clone()).unwrap();
    let id = first["result_id"].as_str().unwrap();
    let mut current = first.clone();
    let mut full = String::new();
    loop {
        assert!(current.to_string().len() <= INLINE_BYTES);
        assert_eq!(current["offset"].as_u64().unwrap() as usize, full.len());
        full.push_str(current["json_fragment"].as_str().unwrap());
        let Some(offset) = current["next_offset"].as_u64() else { break };
        current = read(dir.path(), &launch, id, "", offset as usize).unwrap();
    }
    assert_eq!(serde_json::from_str::<Value>(&full).unwrap(), value);
    assert_eq!(full.len(), first["total_bytes"].as_u64().unwrap() as usize);
    assert_eq!(read(dir.path(), &launch, id, "/value/1/a~1b/1", 0).unwrap()["json_fragment"], "23");
    assert!(read(dir.path(), &launch, id, "/missing", 0).is_err());
    assert!(read(dir.path(), &launch, id, "", full.len() + 1).is_err());
    let inside_unicode = full.find('한').unwrap() + 1;
    assert!(read(dir.path(), &launch, id, "", inside_unicode).is_err());
    assert!(read(dir.path(), &LaunchId::new("other").unwrap(), id, "", 0).is_err());
    assert!(read(dir.path(), &launch, "../../file", "", 0).is_err());
    std::fs::write(dir.path().join("results/generation").join(format!("{id}.json")), "{}").unwrap();
    assert_eq!(read(dir.path(), &launch, id, "", 0).unwrap_err().code, "result_unavailable");
}

#[test]
fn small_outputs_keep_their_shape_and_do_not_write_storage() {
    let dir = tempfile::tempdir().unwrap();
    let value = json!({"value": [1, 2, 3]});
    assert_eq!(bound(dir.path(), &LaunchId::new("small").unwrap(), value.clone()).unwrap(), value);
    assert!(!dir.path().join("results").exists());
}
