//! A predicate wait owns its temporary watch through completion and recorder recovery.
use crate::{execute, fault, server::Recorder, RecorderError};
use frontcap_chromium::{generation::Generation, ChromiumError};
use frontcap_core::{ids::{RequestId, WatchName}, predicate::Predicate, time::VirtualTime};
use frontcap_recorder_protocol::{Answer, ErrorCode, ErrorReply, Op, Outcome};

fn name(id: &RequestId) -> WatchName {
    let digest = frontcap_core::canonical::sha256_hex(id.as_str());
    WatchName::new(format!("mcp-wait-{digest}")).expect("bounded digest identifier")
}

pub(crate) async fn validate(g: &mut Generation, id: &RequestId, predicate: &Predicate, context: Option<&str>) -> Result<(), ChromiumError> {
    g.validate_watch_predicate_in(context, predicate).await?;
    if g.watches().get(&name(id)).is_some() {
        return Err(ChromiumError::InvalidArgument("the predicate wait's temporary watch name is already in use".into()));
    }
    Ok(())
}

async fn install(rec: &Recorder, g: &mut Generation, id: &RequestId, predicate: &Predicate, context: Option<&str>) -> Result<WatchName, RecorderError> {
    let name = name(id);
    let owned = rec.control.lock().unwrap().predicate_watches.get(id).cloned();
    if let Some(owned) = owned {
        if owned != name || g.watches().get(&name) != Some(predicate) || g.watches().context(&name) != context {
            return Err(RecorderError::Missing("predicate wait ownership does not match the adopted watch".into()));
        }
        g.sync_page_watch_in(context, &name, Some(predicate)).await?;
    } else {
        // Persist desired state before the first page effect; reconnect installs the future-document
        // script and syncs the current observer even if the old process never reached watch_add.
        let mut watches = g.watches().clone();
        watches.add_in(name.clone(), predicate.clone(), context.map(str::to_string)).map_err(ChromiumError::from)?;
        rec.update_control(|c| {
            c.watches = watches;
            c.predicate_watches.insert(id.clone(), name.clone());
        })?;
        fault::point(id.as_str(), "before_watch_install");
        g.watch_add_in(context, name.clone(), predicate.clone()).await?;
    }
    fault::point(id.as_str(), "after_watch_install");
    Ok(name)
}

pub(crate) async fn cleanup(rec: &Recorder, g: &mut Generation, id: &RequestId) -> Result<(), RecorderError> {
    let name = rec.control.lock().unwrap().predicate_watches.get(id).cloned();
    let Some(name) = name else { return Ok(()) };
    if g.watches().get(&name).is_some() {
        g.watch_remove(&name).await?;
    } else {
        g.drop_page_watch(&name).await?;
    }
    fault::point(id.as_str(), "after_watch_remove");
    rec.update_control(|c| {
        c.watches = g.watches().clone();
        c.predicate_watches.remove(id);
    })?;
    fault::point(id.as_str(), "after_watch_cleanup");
    Ok(())
}

pub(crate) async fn run(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op, predicate: &Predicate, context: Option<&str>, goal: VirtualTime) -> Answer {
    let begun = rec.control.lock().unwrap().inflight.as_ref().and_then(|i| i.start).unwrap_or_else(|| execute::boundary(g));
    let start_us = begun.t_us;
    let answer = match install(rec, g, id, predicate, context).await {
        Ok(name) => match g.until(&name, goal.since(g.now().min(goal))).await {
            Ok(result) => execute::done(id, Outcome::Clock { result, start_us, t_us: g.now(), start: Some(begun), end: Some(execute::boundary(g)) }),
            Err(e) => execute::failed(e),
        },
        Err(e) => Answer::Err(ErrorReply::new(ErrorCode::Internal, e.to_string())),
    };
    // A crash during removal must return this answer, not evaluate the condition again at a later
    // instant or replenish the original budget.
    if let Err(e) = execute::save_result(rec, id, op, &answer) {
        return fail(rec, g, id, e.to_string()).await;
    }
    fault::point(id.as_str(), "before_watch_cleanup");
    if let Err(e) = cleanup(rec, g, id).await {
        return fail(rec, g, id, e.to_string()).await;
    }
    answer
}

async fn fail(rec: &Recorder, g: &Generation, id: &RequestId, reason: String) -> Answer {
    let detail = rec.fail_operation(id, g.now(), format!("predicate wait cleanup could not be established: {reason}")).await;
    Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, detail))
}
