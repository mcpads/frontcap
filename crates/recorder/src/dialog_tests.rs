//! Cleanup must not invent an answer when neither a saved result nor a native receipt proves it.
use super::*;
use crate::{control::ControlState, runtime::LaunchSpec};
use frontcap_chromium::generation::{Generation, LaunchOptions};
use frontcap_core::{ids::{LaunchId, RunId}, journal::{Journal, RequestState}, profile::DeterminismProfile, rules::RuleSet};
use frontcap_recorder_protocol::Outcome;
use std::{path::PathBuf, time::Duration};

fn id(value: &str) -> RequestId { RequestId::new(value).unwrap() }

#[tokio::test]
async fn cleanup_reconciles_proven_answers_and_preserves_an_unknown_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let spec = LaunchSpec {
        request_id: id("launch"), launch_id: LaunchId::generate(), run_id: RunId::new("cleanup-receipts").unwrap(),
        url: "about:blank".into(), profile: DeterminismProfile {
            host_class: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH), ..Default::default()
        }, rules: RuleSet::default(), passthrough_origins: None, watchdog_ms: 2_000,
        repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap(),
        runs_root: temp.path().join("runs"),
        origin: None,
        restore: None,
    };
    let mut g = Generation::launch(LaunchOptions {
        repo_root: spec.repo_root.clone(), runs_root: spec.runs_root.clone(), run: spec.run_id.clone(),
        url: spec.url.clone(), profile: spec.profile.clone(), rules: spec.rules.clone(), passthrough_origins: None,
        watchdog: Duration::from_secs(2), browser_profile: None, launch_id: Some(spec.launch_id.clone()),
        on_browser: None, proxy: None, upstream: None, cell: None,
    }).await.unwrap();
    g.set_dialog_rules(serde_json::from_str(r#"[{"answer":"hold"}]"#).unwrap()).await.unwrap();
    let dialogs = g.dialog_session().unwrap();
    let evaluation = tokio::spawn(async move {
        let result = g.eval_main("confirm('receipt evidence')").await;
        (g, result)
    });
    let dialog = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(dialog) = dialogs.pending().unwrap() { break dialog }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let native_id = id("native-only");
    let receipt = dialogs.answer(&dialog.id, &native_id, false, None).await.unwrap();
    let (g, value) = tokio::time::timeout(Duration::from_secs(5), evaluation).await.unwrap().unwrap();
    assert_eq!(value.unwrap(), serde_json::json!(false));
    // Leave the manual native receipt unacknowledged, as after effect but before result persistence.
    let dir = temp.path().join("runtime");
    std::fs::create_dir_all(&dir).unwrap();
    let mut journal = Journal::open(&crate::runtime::journal_path(&dir)).unwrap();
    let parent = id("parent");
    journal.begin(&parent, "parent-args").unwrap();
    journal.dispatched(&parent).unwrap();
    let intent = id("intent-only");
    let saved = id("saved-result");
    let unknown = id("unknown-result");
    let expected = Answer::Ok(Reply::DialogAnswered { receipt: Box::new(receipt.clone()) });
    let mut saved_receipt = receipt;
    saved_receipt.request_id = saved.clone();
    saved_receipt.dialog.id = "dialog-saved".into();
    let saved_dialog = saved_receipt.dialog.clone();
    let saved_answer = Answer::Ok(Reply::DialogAnswered { receipt: Box::new(saved_receipt) });
    let mut control = ControlState::default();
    for child in [&intent, &saved, &native_id, &unknown] {
        let child_dialog = if child == &saved { saved_dialog.clone() } else { dialog.clone() };
        let response = DialogResponse { parent_request_id: parent.clone(), dialog_id: child_dialog.id.clone(), accept: false, prompt_text: None };
        let hash = frontcap_core::journal::args_sha256(&serde_json::to_value((DialogAnswerOrigin::Agent, &response)).unwrap());
        journal.begin_dependent(&parent, child, &hash).unwrap();
        if child == &intent { continue }
        journal.dispatched(child).unwrap();
        control.dialog_answers.insert(child.clone(), DialogAnswerPlan {
            origin: DialogAnswerOrigin::Agent,
            response,
            dialog: child_dialog, result: if child == &saved { Some(saved_answer.clone()) } else { None },
        });
    }
    let rec = Recorder::new(dir, spec, g, journal, control);
    let error = rec.reconcile_dialog_dependents(&parent).await.unwrap_err();
    assert!(error.contains("unknown-result has no definite native answer receipt"), "{error}");
    let mut journal = rec.journal.lock().unwrap();
    let Some(RequestState::Completed(value)) = journal.state(&intent) else { panic!("undispatched child unresolved") };
    assert_eq!(serde_json::from_value::<Answer>(value.clone()).unwrap(), crate::execute::done(&intent, Outcome::NotApplied));
    for child in [&saved, &native_id] {
        let Some(RequestState::Completed(value)) = journal.state(child) else { panic!("proven child unresolved: {child}") };
        assert_eq!(serde_json::from_value::<Answer>(value.clone()).unwrap(), if child == &saved { saved_answer.clone() } else { expected.clone() });
    }
    assert_eq!(journal.state(&unknown), Some(&RequestState::Dispatched));
    assert_eq!(journal.pending_dependents(&parent), vec![unknown.clone()]);
    assert!(journal.validate_completion(&parent).is_err(), "unknown child must block parent publication");
    assert!(rec.dialog_inputs(&journal, &parent).unwrap_err().contains("unresolved"));
    // Only the journal's definite non-application decision can make this child publishable.
    journal.complete(&unknown, serde_json::to_value(crate::execute::done(&unknown, Outcome::NotApplied)).unwrap()).unwrap();
    let inputs = rec.dialog_inputs(&journal, &parent).unwrap();
    assert_eq!(inputs.iter().map(|d| d.request_id.clone()).collect::<Vec<_>>(), [&intent, &saved, &native_id, &unknown].map(Clone::clone));
    assert!(inputs[0].attempt.is_none(), "intent without a saved plan must remain explicit");
    assert_eq!(inputs[0].args_sha256, journal.argument_hash(&intent).unwrap());
    assert_eq!(inputs[0].result, frontcap_core::inputs::DialogInputOutcome::NotApplied);
    rec.control.lock().unwrap().dialog_answers.get_mut(&saved).unwrap().response.accept = true;
    assert!(rec.dialog_inputs(&journal, &parent).unwrap_err().contains("conflicting argument fingerprint"));
    rec.control.lock().unwrap().dialog_answers.get_mut(&saved).unwrap().response.accept = false;
    rec.control.lock().unwrap().dialog_answers.get_mut(&saved).unwrap().dialog.message = "different dialog".into();
    assert!(rec.dialog_inputs(&journal, &parent).unwrap_err().contains("conflicting answer receipt"));
    rec.control.lock().unwrap().dialog_answers.get_mut(&saved).unwrap().dialog.message = saved_dialog.message;
    // A correlated id is insufficient: native source and delivered prompt text must also agree.
    for corrupt_source in [false, true] {
        let child = id(if corrupt_source { "external-receipt" } else { "wrong-prompt" });
        let root = id(if corrupt_source { "external-parent" } else { "prompt-parent" });
        journal.begin(&root, "parent-args").unwrap();
        journal.dispatched(&root).unwrap();
        let mut plan = rec.control.lock().unwrap().dialog_answers.get(&saved).unwrap().clone();
        plan.response.parent_request_id = root.clone();
        let hash = frontcap_core::journal::args_sha256(&serde_json::to_value((plan.origin, &plan.response)).unwrap());
        journal.begin_dependent(&root, &child, &hash).unwrap();
        journal.dispatched(&child).unwrap();
        let Answer::Ok(Reply::DialogAnswered { receipt }) = plan.result.as_mut().unwrap() else { panic!() };
        receipt.request_id = child.clone();
        if corrupt_source { receipt.source = frontcap_core::event::NativeDialogSource::External; }
        else { receipt.prompt_text = "not delivered by this confirm".into(); }
        journal.complete(&child, serde_json::to_value(plan.result.as_ref().unwrap()).unwrap()).unwrap();
        rec.control.lock().unwrap().dialog_answers.insert(child.clone(), plan);
        assert!(rec.dialog_inputs(&journal, &root).unwrap_err().contains(&format!("{child} has a conflicting answer receipt")));
    }
}
