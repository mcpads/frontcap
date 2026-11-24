//! Compare caller-selected durable observations; never acquire or sample a target.
use frontcap_core::{
    ids::RequestId,
    journal::{Journal, RequestState},
    native::{
        webview::Completion,
        webview_progress::{Capture, Change, Comparison, Evidence, Progress, ValueSample},
    },
};
use frontcap_recorder_protocol::{
    Answer, Reply,
    native::{Outcome, Reply as NativeReply},
};
use serde_json::Value;
use std::{collections::BTreeSet, io};

fn outcome(journal: &Journal, id: &RequestId) -> io::Result<Outcome> {
    let Some(RequestState::Completed(value)) = journal.state(id) else {
        return Err(io::Error::other(
            "comparison requires completed evidence in this launch",
        ));
    };
    let answer: Answer = serde_json::from_value(value.clone())?;
    match answer {
        Answer::Ok(Reply::Native {
            body:
                NativeReply::Done {
                    request_id,
                    outcome,
                },
        }) if request_id == *id => Ok(*outcome),
        _ => Err(io::Error::other(
            "comparison evidence has no matching native result",
        )),
    }
}
fn completion(journal: &Journal, id: &RequestId) -> io::Result<Completion> {
    match outcome(journal, id)? {
        Outcome::WebviewCompleted { completion } => Ok(*completion),
        _ => Err(io::Error::other("comparison evidence is not a completed iOS WebView observation")),
    }
}
fn progress(journal: &Journal, id: &RequestId) -> io::Result<Progress> {
    match completion(journal, id)? {
        Completion::Progress { observation } => Ok(*observation),
        _ => Err(io::Error::other(
            "comparison requires inspect_webview_progress result IDs",
        )),
    }
}
fn capture(journal: &Journal, id: Option<&RequestId>) -> io::Result<Option<Capture>> {
    id.map(|id| match completion(journal, id)? {
        Completion::Captured {
            path,
            bytes,
            sha256,
            ..
        } => Ok(Capture {
            request_id: id.clone(),
            path,
            bytes,
            sha256,
        }),
        _ => Err(io::Error::other(
            "comparison capture ID is not a completed capture",
        )),
    })
    .transpose()
}
fn sample(value: Option<&Value>) -> ValueSample {
    let bytes = value.map(|v| serde_json::to_vec(v).expect("JSON value").len());
    let truncated = bytes.is_some_and(|n| n > 2048);
    ValueSample {
        present: value.is_some(),
        serialized_bytes: bytes,
        value: if truncated { None } else { value.cloned() },
        truncated,
    }
}
fn changes(
    path: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    out: &mut Vec<Change>,
    omitted: &mut usize,
) {
    if before == after {
        return;
    }
    if let (Some(Value::Object(a)), Some(Value::Object(b))) = (before, after) {
        let keys: BTreeSet<_> = a.keys().chain(b.keys()).collect();
        for key in keys {
            let escaped = key.replace('~', "~0").replace('/', "~1");
            changes(
                &format!("{path}/{escaped}"),
                a.get(key),
                b.get(key),
                out,
                omitted,
            );
        }
    } else if out.len() < 32 {
        out.push(Change {
            path: path.into(),
            before: sample(before),
            after: sample(after),
        });
    } else {
        *omitted += 1;
    }
}
pub(super) fn compare(
    journal: &Journal,
    before: &RequestId,
    after: &RequestId,
    before_capture: Option<&RequestId>,
    after_capture: Option<&RequestId>,
) -> io::Result<Comparison> {
    let a = progress(journal, before)?;
    let b = progress(journal, after)?;
    if a.owner != b.owner {
        return Err(io::Error::other("comparison owners differ"));
    }
    let mut result = Comparison {
        owner: a.owner.clone(),
        before: Evidence {
            progress: before.clone(),
            capture: capture(journal, before_capture)?,
        },
        after: Evidence {
            progress: after.clone(),
            capture: capture(journal, after_capture)?,
        },
        changes: Vec::new(),
        omitted_changes: 0,
        capture_content_changed: None,
        limitations: vec![
            "retained_receipts_not_live_state".into(),
            "caller_selected_order_not_causality".into(),
            "captures_are_historical_and_not_simultaneous".into(),
            "capture_files_not_revalidated".into(),
            "navigation_if_present_is_document_local_and_bounded".into(),
            "missing_navigation_is_unavailable_not_empty".into(),
            "unknown_producers_not_idle".into(),
        ],
    };
    result.capture_content_changed = result
        .before
        .capture
        .as_ref()
        .zip(result.after.capture.as_ref())
        .map(|(a, b)| a.sha256 != b.sha256 || a.bytes != b.bytes);
    changes(
        "",
        Some(&serde_json::to_value(a)?),
        Some(&serde_json::to_value(b)?),
        &mut result.changes,
        &mut result.omitted_changes,
    );
    Ok(result)
}

/// Both paths read only this launch's completed journal entries. Android currently has one
/// immutable consumer owner per launch; a future replacement operation must retain epoch bindings.
pub(super) fn compare_for_ready(
    ready: &frontcap_recorder_protocol::native::Ready,
    journal: &Journal,
    before: &RequestId,
    after: &RequestId,
    before_capture: Option<&RequestId>,
    after_capture: Option<&RequestId>,
) -> io::Result<Outcome> {
    use frontcap_recorder_protocol::native::Ready;
    match ready {
        Ready::Webview { .. } => Ok(Outcome::WebviewCompared {
            comparison: compare(journal, before, after, before_capture, after_capture)?,
        }),
        Ready::AndroidWebview { owner, .. } => {
            let a = android_observation(journal, before)?;
            let b = android_observation(journal, after)?;
            let mut comparison = Comparison {
                owner: owner.clone(),
                before: Evidence { progress: before.clone(), capture: android_capture(journal, before_capture)? },
                after: Evidence { progress: after.clone(), capture: android_capture(journal, after_capture)? },
                changes: Vec::new(), omitted_changes: 0, capture_content_changed: None,
                limitations: vec![
                    "retained_observations_not_live_state".into(),
                    "caller_selected_order_not_causality".into(),
                    "captures_are_historical_and_not_simultaneous".into(),
                    "capture_files_not_revalidated".into(),
                    "navigation_if_present_is_document_local_and_bounded".into(),
                    "missing_navigation_history_does_not_invalidate_sampled_url_and_focus".into(),
                    "timer_activity_if_present_is_bounded_bookkeeping_not_complete_pending_work".into(),
                    "unknown_producers_not_idle".into(),
                ],
            };
            comparison.capture_content_changed = comparison.before.capture.as_ref()
                .zip(comparison.after.capture.as_ref())
                .map(|(a, b)| a.sha256 != b.sha256 || a.bytes != b.bytes);
            changes("", Some(&a), Some(&b), &mut comparison.changes, &mut comparison.omitted_changes);
            Ok(Outcome::AndroidWebviewCompared { comparison })
        }
        _ => Err(io::Error::other("comparison requires a WebView launch")),
    }
}

fn android_observation(journal: &Journal, id: &RequestId) -> io::Result<Value> {
    use frontcap_core::native::android_webview::Completion as A;
    match outcome(journal, id)? {
        Outcome::AndroidWebviewCompleted { completion: A::Progress { observation } } =>
            Ok(serde_json::json!({"progress": observation})),
        Outcome::AndroidWebviewCompleted { completion: A::Read { snapshot } } =>
            Ok(serde_json::json!({"snapshot": snapshot})),
        Outcome::AndroidWebviewCompleted { completion: A::Observed { snapshot, pointers, .. } } =>
            Ok(serde_json::json!({"snapshot": snapshot, "pointers": pointers})),
        _ => Err(io::Error::other("Android comparison requires completed read, observe or progress result IDs")),
    }
}

fn android_capture(journal: &Journal, id: Option<&RequestId>) -> io::Result<Option<Capture>> {
    use frontcap_core::native::android_webview::Completion as A;
    id.map(|id| match outcome(journal, id)? {
        Outcome::AndroidWebviewCompleted { completion: A::Observed { screen, bytes, sha256, .. } } =>
            Ok(Capture { request_id: id.clone(), path: screen, bytes, sha256 }),
        _ => Err(io::Error::other("Android comparison capture requires a completed observe result")),
    }).transpose()
}

#[cfg(test)]
#[path = "webview_comparison_tests.rs"]
mod tests;
