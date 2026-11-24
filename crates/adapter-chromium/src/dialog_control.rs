//! Browser-owned native dialogs, serviceable while a renderer command is blocked by its modal loop.
//! Retained native inventory is authoritative; DevTools events only prompt a refresh.
use crate::service_clock::{ServiceClock, ServiceHold};
use crate::session::Session;
use crate::ChromiumError;
use cdp::protocol::page;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

pub type DialogInventory = page::FrontcapGetDialogsReturns;
pub type DialogUpdate = Result<DialogInventory, String>;

/// Clones answer or inspect a native dialog independently of the operation holding the generation.
/// Dropping this handle never answers a dialog or discards a retained receipt.
#[derive(Clone)]
pub struct DialogControl {
    inner: Arc<Inner>,
}

struct Inner {
    caller: cdp::client::Caller,
    session: String,
    clock: ServiceClock,
    hold: Mutex<Option<ServiceHold>>,
    fault: Mutex<Option<String>>,
    serial: tokio::sync::Mutex<()>,
    updates: watch::Sender<DialogUpdate>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The browser no longer has the session: its page closed.
    gone: std::sync::atomic::AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().unwrap().take() {
            task.abort();
        }
    }
}

/// These commands are browser-side. They retain a host bound even when renderer service is parked.
async fn call<C: cdp::Command>(caller: &cdp::client::Caller, session: &str, params: &C) -> Result<C::Returns, ChromiumError> {
    tokio::time::timeout(Session::CALL_TIMEOUT, caller.call(Some(session), params))
        .await.map_err(|_| ChromiumError::CallTimeout(C::METHOD))?.map_err(Into::into)
}

impl Inner {
    fn fail(&self, reason: String) {
        {
            let mut fault = self.fault.lock().unwrap();
            *fault = Some(reason.clone());
            let _ = self.updates.send_replace(Err(reason));
        }
        self.park(false);
    }

    fn available(&self) -> Result<(), ChromiumError> {
        match self.fault.lock().unwrap().as_ref() {
            Some(reason) => Err(ChromiumError::Shape(format!("native dialog control failed: {reason}"))),
            None => Ok(()),
        }
    }

    fn park(&self, pending: bool) {
        let mut hold = self.hold.lock().unwrap();
        if pending && hold.is_none() && self.fault.lock().unwrap().is_none() {
            *hold = Some(self.clock.park());
        } else if !pending {
            *hold = None;
        }
    }

    /// Called under `serial`, so an older inventory cannot overwrite a newer answer or refresh.
    async fn refresh(&self) -> Result<DialogInventory, ChromiumError> {
        self.available()?;
        let result = call(&self.caller, &self.session, &page::FrontcapGetDialogsParams::default()).await;
        match result {
            Ok(inventory) => {
                self.park(!inventory.dialogs.is_empty());
                // An automatic-answer failure can arrive while this browser query is in flight.
                // Never overwrite that terminal fault with an older successful inventory.
                let fault = self.fault.lock().unwrap();
                if let Some(reason) = fault.as_ref() {
                    return Err(ChromiumError::Shape(format!("native dialog control failed: {reason}")));
                }
                let _ = self.updates.send_replace(Ok(inventory.clone()));
                Ok(inventory)
            }
            Err(error) => {
                if matches!(&error, ChromiumError::Cdp(cdp::CdpError::SessionDetached | cdp::CdpError::Protocol { code: -32001, .. })) {
                    self.gone.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                self.fail(error.to_string());
                Err(error)
            }
        }
    }
}

impl DialogControl {
    /// Attach before enabling renderer domains on recovery: Page.enable itself can block behind a
    /// retained modal. Subscriptions are installed before control or inventory is requested.
    pub async fn attach(session: &Session) -> Result<Self, ChromiumError> {
        Self::attach_with(session.client.caller(), &session.id, session.service_clock()).await
    }

    /// Controls the dialogs of the page `session` addresses, parking `clock` while one is open: every page
    /// of a generation shares one renderer, so a dialog in any of them blocks every page command.
    pub async fn attach_with(caller: cdp::client::Caller, session: &str, clock: ServiceClock) -> Result<Self, ChromiumError> {
        let mut opening = caller.subscribe::<page::JavascriptDialogOpeningEvent>(Some(session));
        let mut closed = caller.subscribe::<page::JavascriptDialogClosedEvent>(Some(session));
        call(&caller, session, &page::FrontcapSetDialogControlParams::new(true)).await?;
        let inventory = call(&caller, session, &page::FrontcapGetDialogsParams::default()).await?;
        let hold = (!inventory.dialogs.is_empty()).then(|| clock.park());
        let inner = Arc::new(Inner {
            caller, session: session.to_string(), clock, hold: Mutex::new(hold),
            fault: Default::default(), serial: Default::default(), updates: watch::channel(Ok(inventory)).0, task: Default::default(), gone: Default::default(),
        });
        let weak = Arc::downgrade(&inner);
        let task = tokio::spawn(async move {
            loop {
                let (is_opening, event) = tokio::select! {
                    event = opening.next() => (true, event.map(|_| ())),
                    event = closed.next() => (false, event.map(|_| ())),
                };
                let Some(inner) = weak.upgrade() else { break };
                if let Err(error) = event {
                    inner.fail(error.to_string());
                    break;
                }
                if is_opening {
                    // Exclude inventory lookup itself from a command already waiting in the modal.
                    inner.park(true);
                }
                let _serial = inner.serial.lock().await;
                if inner.refresh().await.is_err() {
                    break;
                }
            }
        });
        *inner.task.lock().unwrap() = Some(task);
        Ok(Self { inner })
    }

    pub(crate) fn report_fault(&self, reason: String) {
        self.inner.fail(reason);
    }

    /// Whether its page closed: the browser no longer has the session it addresses.
    pub fn is_gone(&self) -> bool {
        self.inner.gone.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> watch::Receiver<DialogUpdate> {
        self.inner.updates.subscribe()
    }

    pub async fn inspect(&self) -> Result<DialogInventory, ChromiumError> {
        let _serial = self.inner.serial.lock().await;
        self.inner.refresh().await
    }

    /// Native request correlation validates retries and rejects conflicting answers. A failed answer
    /// does not release the service hold; only a refreshed native inventory establishes closure.
    pub async fn answer(&self, params: &page::FrontcapAnswerDialogParams) -> Result<page::FrontcapDialogReceipt, ChromiumError> {
        let _serial = self.inner.serial.lock().await;
        self.inner.available()?;
        let answered = call(&self.inner.caller, &self.inner.session, params).await?;
        self.inner.refresh().await?;
        Ok(answered.receipt)
    }

    /// The owner calls this only after the corresponding receipts are durably recorded.
    pub async fn forget(&self, dialog_ids: Vec<String>) -> Result<(), ChromiumError> {
        let _serial = self.inner.serial.lock().await;
        self.inner.available()?;
        call(&self.inner.caller, &self.inner.session, &page::FrontcapForgetDialogReceiptsParams::new(dialog_ids)).await?;
        self.inner.refresh().await?;
        Ok(())
    }
}
