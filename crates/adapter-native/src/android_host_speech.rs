//! Original speech receipts are recoverable without a live app or another attachment.
use super::*;
use frontcap_core::native::{android_speech, android_webview::AppProcess};

pub(super) fn attachment(directory: &Path, id: &RequestId, owner: &AppProcess) -> io::Result<host::SpeechAttachment> {
    let request = directory.join("requests").join(id.as_str());
    let value: host::SpeechAttachment = load(&request.join("speech-attached.json"))?
        .ok_or_else(|| io::Error::other("original speech attachment unavailable"))?;
    let operation: Operation = load(&request.join("operation.json"))?
        .ok_or_else(|| io::Error::other("original speech operation unavailable"))?;
    let Operation::Host { request: host::Operation::AttachSpeech { owner: requested, sha256, .. } } = operation else {
        return Err(io::Error::other("speech reference is not an attachment request"));
    };
    if value.id != *id || value.owner != *owner || requested.as_ref() != owner
        || value.library_sha256 != sha256 || value.identity.start_ticks != owner.process_start_ticks
        || !frontcap_core::native::request::canonical_uuid(&value.identity.id)
        || !value.directory.ends_with(&format!("/files/frontcap-speech-{}", value.identity.id)) {
        return Err(io::Error::other("speech attachment differs from original owner or request"));
    }
    Ok(value)
}

#[cfg(test)]
#[path = "android_host_speech_tests.rs"]
mod tests;

pub(super) fn completion(directory: &Path, id: &RequestId, operation: &host::Operation,
    owner: &AppProcess) -> io::Result<Option<host::Completion>> {
    let evidence = directory.join("requests").join(id.as_str());
    let Some((recorded, history)) = load::<(host::SpeechAttachment, android_speech::History)>(&evidence.join("speech-read.json"))? else {
        return Ok(None);
    };
    let (reference, attaching) = match operation {
        host::Operation::AttachSpeech { owner: requested, .. } if requested.as_ref() == owner => (id, true),
        host::Operation::ReadSpeech { attachment } => (attachment, false),
        _ => return Err(io::Error::other("speech receipt differs from requested operation")),
    };
    if attaching && !evidence.join("speech-attached.json").exists() { return Ok(None); }
    let original = attachment(directory, reference, owner)?;
    let raw = std::fs::read_to_string(evidence.join("speech-read.jsonl"))?;
    let mut decoded = android_speech::History::from_bound_jsonl(&raw, owner.pid, &original.identity)?;
    if decoded.sampling {
        let sample = std::fs::read_to_string(evidence.join("speech-state.json"))?;
        let key = frontcap_core::canonical::sha256_bytes_hex(id.as_str().as_bytes());
        let (recorded_key,index): (String,u64) = load(&evidence.join("speech-sample-request.json"))?
            .ok_or_else(|| io::Error::other("original speech sampling request unavailable"))?;
        if recorded_key != key[..32] { return Err(io::Error::other("speech sampling request identity differs")); }
        decoded.apply_sample(&sample, &recorded_key, index)?;
    }
    if recorded != original || decoded != history {
        return Err(io::Error::other("speech receipt differs from original producer evidence"));
    }
    if attaching {
        let bytes = std::fs::read(evidence.join("speech-observer.so"))?;
        if bytes.len() > 16 * 1024 * 1024 || frontcap_core::canonical::sha256_bytes_hex(&bytes) != original.library_sha256 {
            return Err(io::Error::other("retained speech library differs"));
        }
        Ok(Some(host::Completion::SpeechAttached { attachment: Box::new(original), history: Box::new(history) }))
    } else {
        Ok(Some(host::Completion::SpeechRead { attachment: reference.clone(), history: Box::new(history) }))
    }
}
