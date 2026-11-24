//! Connection-local navigation observation survives document replacement without evaluating the app.
use cdp::protocol::page::{FrameNavigatedEvent, NavigationType};
use frontcap_core::native::android_navigation::{Entry, History, CAPACITY, URL_LIMIT};
use std::sync::{Arc, Mutex};

fn bound(mut text: String) -> (String, bool) {
    let truncated=text.len()>URL_LIMIT;
    if truncated {
        let mut end=URL_LIMIT;
        while !text.is_char_boundary(end) {end-=1;}
        text.truncate(end);
    }
    (text,truncated)
}
pub(crate) struct Observer {
    history: Arc<Mutex<History>>,
    task: tokio::task::JoinHandle<()>,
}
impl Observer {
    pub fn new(client: &cdp::Client) -> Self {
        let mut subscription=client.subscribe::<FrameNavigatedEvent>(None);
        let history=Arc::new(Mutex::new(History {
            connection:frontcap_core::ids::RequestId::generate().as_str().to_owned(),
            received:0,dropped:0,closed:false,failure:None,entries:Vec::new(),
        }));
        let collected=history.clone();
        let task=tokio::spawn(async move {
            loop {
                match subscription.next().await {
                    Ok(event) => {
                        let mut history=collected.lock().unwrap();
                        let frame=event.frame;
                        let identity=|s: &str|!s.is_empty() && s.len()<=256;
                        if !identity(&frame.id.0) || !identity(&frame.loader_id.0)
                            || frame.parent_id.as_ref().is_some_and(|p|!identity(&p.0)) {
                            history.closed=true;
                            history.failure=Some("navigation frame or loader identity exceeds observation bounds".into());
                            break;
                        }
                        let (url,url_truncated)=bound(frame.url+frame.url_fragment.as_deref().unwrap_or(""));
                        history.received+=1;
                        let entry=Entry {sequence:history.received,frame:frame.id.0,
                            parent:frame.parent_id.map(|p|p.0),loader:frame.loader_id.0,url,url_truncated,
                            restored_from_cache:event.r#type==NavigationType::BackForwardCacheRestore};
                        if history.entries.len()==CAPACITY {history.entries.remove(0);history.dropped+=1;}
                        history.entries.push(entry);
                    }
                    Err(error) => {
                        let mut history=collected.lock().unwrap();history.closed=true;
                        if !matches!(error,cdp::CdpError::Closed) {history.failure=Some(bound(error.to_string()).0);}
                        break;
                    }
                }
            }
        });
        Self {history,task}
    }
    pub fn snapshot(&self) -> History {self.history.lock().unwrap().clone()}
}
impl Drop for Observer {
    fn drop(&mut self) {self.task.abort();}
}

#[cfg(test)]
#[path = "android_navigation_tests.rs"]
mod tests;
