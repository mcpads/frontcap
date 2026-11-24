//! Same-process Android host transitions. Uncertain effects are retained, never replayed.
use super::*;
use frontcap_core::{ids::RequestId, state::load};

pub use frontcap_core::native::android_webview::{WebViewLifecycle, WebViewLifecycleReceipt};
pub(super) fn focused_component(raw: &str) -> io::Result<String> {
    let values: Vec<_> = raw.lines().filter_map(|line| line.trim().strip_prefix("mCurrentFocus=")).collect();
    if values.len()!=1 { return Err(io::Error::other("ambiguous focused window")); }
    let component=values[0].strip_prefix("Window{").and_then(|s|s.strip_suffix('}'))
        .and_then(|s|s.split_whitespace().last()).ok_or_else(||io::Error::other("focused window unavailable"))?;
    let (package,activity)=component.split_once('/').ok_or_else(||io::Error::other("invalid focused component"))?;
    let activity=if activity.starts_with('.') {format!("{package}{activity}")} else {activity.into()};
    if !frontcap_core::native::android_capture::valid_app(package,&activity) {
        return Err(io::Error::other("invalid focused application"));
    }
    Ok(format!("{package}/{activity}"))
}
impl Device {
    /// Retained evidence only: does not query ADB, settle a transition or authorize replay.
    pub(crate) fn inspect_lifecycle_progress(&self) -> Option<frontcap_core::native::android_progress::Lifecycle> {
        self.webview_debugger.as_ref()?;
        let mut progress = self.webview_lifecycle_progress.clone();
        progress.uncertain = self.webview_lifecycle_uncertain;
        Some(progress)
    }

    pub(crate) fn check_webview_lifecycle(&self)->io::Result<()> {
        if self.webview_lifecycle_uncertain { return Err(io::Error::other("host transition unresolved; retire without replay")); }
        Ok(())
    }
    /// Drop the page connection before a transition. Resume its original journal and explicitly
    /// pause/reconcile afterward. Completion establishes host focus and process identity only.
    pub fn transition_webview(&mut self,id:&RequestId,operation:WebViewLifecycle,
        deadline:Instant,cancelled:&AtomicBool)->io::Result<WebViewLifecycleReceipt> {
        let root=self.directory.join("webview-lifecycle");
        let attempt=root.join(id.as_str());
        if let Some(original)=load::<WebViewLifecycle>(&attempt.join("operation.json"))? {
            if original!=operation {return Err(io::Error::other("host transition request ID conflict"));}
            return load(&attempt.join("receipt.json"))?.ok_or_else(||io::Error::other("host transition unresolved; never replay"));
        }
        self.check_webview_lifecycle()?;
        if self.contact_poisoned {return Err(io::Error::other("contact journal unavailable"));}
        if let Some(state)=&self.contact_state {state.require_released()?;}
        let owner=self.webview_debugger.clone().ok_or_else(||io::Error::other("WebView host unavailable"))?;
        self.verify_webview_process(&owner,deadline,cancelled)?;
        let component=format!("{}/{}",owner.package,owner.activity);
        if operation==WebViewLifecycle::Home {
            let raw=self.adb(&["shell","dumpsys","window"],deadline,cancelled,false)?.unwrap();
            if focused_component(&raw)?!=component {return Err(io::Error::other("owned consumer is not foreground"));}
        }
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir(&attempt)?;
        save_atomic(&attempt.join("operation.json"),&operation)?;
        save_atomic(&attempt.join("owner.json"),&owner)?;
        self.invalidate_contact_capture();
        self.webview_lifecycle_uncertain=true;
        self.webview_lifecycle_progress.pending=Some(frontcap_core::native::android_progress::Transition {
            id:id.clone(), operation, owner:owner.clone(), receipt:None,
        });
        let result:io::Result<WebViewLifecycleReceipt>=(|| {
            match operation {
                WebViewLifecycle::Home=>{self.adb(&["shell","input","keyevent","KEYCODE_HOME"],deadline,cancelled,false)?;}
                WebViewLifecycle::Activate=>{self.adb(&["shell","am","start","-W","-n",&component],deadline,cancelled,false)?;}
            }
            loop {
                self.verify_webview_process(&owner,deadline,cancelled)?;
                let raw=self.adb(&["shell","dumpsys","window"],deadline,cancelled,false)?.unwrap();
                let focused=match focused_component(&raw) {
                    Ok(focused)=>focused,
                    // Android can briefly report no focused window during a transition.
                    Err(_)=>{std::thread::sleep(Duration::from_millis(25));continue;}
                };
                let reached=match operation {
                    WebViewLifecycle::Home=>focused.split('/').next()!=Some(owner.package.as_str()),
                    WebViewLifecycle::Activate=>focused==component,
                };
                if reached {
                    self.verify_webview_process(&owner,deadline,cancelled)?;
                    let receipt=WebViewLifecycleReceipt {operation,owner:owner.clone(),focused_component:focused};
                    save_atomic(&attempt.join("receipt.json"),&receipt)?;
                    return Ok(receipt);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })();
        match &result {
            Ok(receipt)=>{
                let mut completed=self.webview_lifecycle_progress.pending.take().unwrap();
                completed.receipt=Some(receipt.clone());
                self.webview_lifecycle_progress.last_completed=Some(completed);
                self.webview_lifecycle_uncertain=false;
            },
            Err(error)=>{save_atomic(&attempt.join("error.json"),&error.to_string())?;}
        }
        result
    }
}
