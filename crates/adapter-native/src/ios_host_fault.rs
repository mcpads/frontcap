//! Test-only publication interruption. Propagate through both host and worker result envelopes so
//! original native evidence remains the only recovery source; normal ownership still retires.
use frontcap_core::ids::RequestId;
use std::io;

#[cfg(feature = "fault-injection")]
#[derive(Debug)]
struct PublicationFailure { request: RequestId, stage: &'static str }
#[cfg(feature = "fault-injection")]
impl std::fmt::Display for PublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "injected iOS host publication failure: {}@{}", self.request, self.stage)
    }
}
#[cfg(feature = "fault-injection")]
impl std::error::Error for PublicationFailure {}

pub(crate) fn point(id: &RequestId, stage: &'static str) -> io::Result<()> {
    #[cfg(feature = "fault-injection")]
    if std::env::var("FRONTCAP_IOS_HOST_LOSE_WORKER").ok().as_deref() == Some(format!("{id}@{stage}").as_str()) {
        eprintln!("injected iOS worker loss: {id}@{stage}");
        // Fault injection targets this live process only; it is never cleanup via a recorded PID.
        unsafe { libc::raise(libc::SIGKILL); }
        return Err(io::Error::other("injected worker loss unexpectedly returned"));
    }
    #[cfg(feature = "fault-injection")]
    if std::env::var("FRONTCAP_IOS_HOST_FAULT").ok().as_deref() == Some(format!("{id}@{stage}").as_str()) {
        return Err(io::Error::other(PublicationFailure { request:id.clone(), stage }));
    }
    let _ = (id, stage);
    Ok(())
}

pub(crate) fn is_injected(error: &io::Error) -> bool {
    #[cfg(feature = "fault-injection")]
    { error.get_ref().is_some_and(|cause| cause.is::<PublicationFailure>()) }
    #[cfg(not(feature = "fault-injection"))]
    { let _ = error; false }
}
