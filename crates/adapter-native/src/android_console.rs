//! Passive CDP console collection. No console replacement, object evaluation or application hooks.
use cdp::protocol::runtime::ConsoleApiCalledEvent;
use frontcap_core::native::android_console::{Entry, History, CAPACITY, TEXT_LIMIT};
use std::sync::{Arc, Mutex};

fn bound(mut text: String) -> (String, bool) {
    let truncated = text.len() > TEXT_LIMIT;
    if truncated {
        let mut end = TEXT_LIMIT;
        while !text.is_char_boundary(end) { end -= 1; }
        text.truncate(end);
    }
    (text, truncated)
}
fn retain(history: &mut History, mut entry: Entry) {
    history.received += 1;
    entry.sequence = history.received;
    let (text, truncated) = bound(entry.text);
    entry.text = text;
    entry.truncated |= truncated;
    if history.entries.len() == CAPACITY {
        history.entries.remove(0);
        history.dropped += 1;
    }
    history.entries.push(entry);
}
pub(crate) struct Observer {
    history: Arc<Mutex<History>>,
    task: tokio::task::JoinHandle<()>,
}
impl Observer {
    pub fn new(client: &cdp::Client) -> Self {
        let mut subscription = client.subscribe::<ConsoleApiCalledEvent>(None);
        let history = Arc::new(Mutex::new(History {
            connection: frontcap_core::ids::RequestId::generate().as_str().to_owned(), received: 0, dropped: 0,
            closed: false, failure: None, entries: Vec::new(),
        }));
        let collected = history.clone();
        let task = tokio::spawn(async move {
            loop {
                match subscription.next().await {
                    Ok(event) => {
                        // Primitive values and CDP descriptions only; never invoke getters/toString
                        // in the consumer or expand remote objects.
                        let mut text = String::new();
                        let mut truncated = false;
                        for arg in &event.args {
                            if !text.is_empty() { text.push(' '); }
                            let part = arg.value.as_ref().map(|v| v.as_str().map(str::to_owned)
                                .unwrap_or_else(|| v.to_string()))
                                .or_else(|| arg.description.clone()).unwrap_or_default();
                            text.push_str(&part);
                            if text.len() > TEXT_LIMIT { truncated = true; break; }
                        }
                        let level = serde_json::to_value(event.r#type).ok()
                            .and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "unknown".into());
                        retain(&mut collected.lock().unwrap(), Entry {
                            sequence: 0, context: event.execution_context_id.0,
                            timestamp_ms: event.timestamp.0, level, text, truncated,
                        });
                    }
                    Err(error) => {
                        let mut history = collected.lock().unwrap();
                        history.closed = true;
                        if !matches!(error, cdp::CdpError::Closed) { history.failure = Some(bound(error.to_string()).0); }
                        break;
                    }
                }
            }
        });
        Self { history, task }
    }
    pub fn snapshot(&self) -> History { self.history.lock().unwrap().clone() }
}
impl Drop for Observer {
    fn drop(&mut self) { self.task.abort(); }
}

#[cfg(test)]
#[path = "android_console_tests.rs"]
mod tests;
