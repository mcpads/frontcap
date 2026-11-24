//! Passive speech attachment in the owned app; never retry an uncertain ART dispatch.
use super::*;
use frontcap_core::{ids::RequestId, native::{android_host::SpeechAttachment, android_speech, android_webview::AppProcess}};
use std::io::Read;

impl Device {
    pub(crate) fn attach_speech(&mut self, request: &RequestId, owner: &AppProcess,
        library: (&Path, &str), evidence: &Path, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<(SpeechAttachment, android_speech::History)> {
        let (library, digest) = library;
        self.verify_app_process(owner, deadline, cancelled)?;
        let marker = self.host_evidence_directory().join("speech-attachment-intent.json");
        if marker.exists() { return Err(io::Error::other("speech attachment already attempted for this process")); }
        let mut bytes = Vec::new();
        File::open(library)?.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 * 1024 || bytes.is_empty()
            || frontcap_core::canonical::sha256_bytes_hex(&bytes) != digest {
            return Err(io::Error::other("speech library exceeds bound or digest differs"));
        }
        let library_copy = evidence.join("speech-observer.so");
        std::fs::write(&library_copy, bytes)?;
        let id = self.adb(&["shell", "cat", "/proc/sys/kernel/random/uuid"], deadline, cancelled, false)?.unwrap().trim().to_owned();
        if !frontcap_core::native::request::canonical_uuid(&id) { return Err(io::Error::other("invalid attachment UUID")); }
        let data = self.adb(&["shell", "run-as", &owner.package, "pwd"], deadline, cancelled, false)?.unwrap().trim().to_owned();
        if !data.starts_with('/') || data.contains("..") || !data.bytes().all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)) {
            return Err(io::Error::other("invalid app-private speech directory"));
        }
        let attachment = SpeechAttachment { id: request.clone(), owner: owner.clone(),
            identity: android_speech::Attachment { id: id.clone(), start_ticks: owner.process_start_ticks },
            directory: format!("{data}/files/frontcap-speech-{id}"), library_sha256: digest.into() };
        // Durable process-wide intent precedes every guest mutation. No error authorizes reattach.
        save_atomic(&marker, &attachment)?;
        save_atomic(&evidence.join("speech-intent.json"), &attachment)?;
        self.adb(&["shell", "run-as", &owner.package, "mkdir", "-p", "files"], deadline, cancelled, false)?;
        self.adb(&["shell", "run-as", &owner.package, "mkdir", &attachment.directory], deadline, cancelled, false)?;
        let uploaded = format!("/data/local/tmp/frontcap-speech-{id}.so");
        self.adb(&["push", library_copy.to_str().ok_or_else(|| io::Error::other("non-UTF8 speech library path"))?, &uploaded], deadline, cancelled, false)?;
        let installed = format!("{}/observer.so", attachment.directory);
        self.adb(&["shell", "run-as", &owner.package, "cp", &uploaded, &installed], deadline, cancelled, false)?;
        let observed = self.adb(&["shell", "run-as", &owner.package, "sha256sum", &installed], deadline, cancelled, false)?.unwrap();
        if observed.split_whitespace().collect::<Vec<_>>() != [digest, installed.as_str()] {
            return Err(io::Error::other("installed speech library digest differs"));
        }
        self.adb(&["shell", "rm", &uploaded], deadline, cancelled, false)?;
        self.verify_app_process(owner, deadline, cancelled)?;
        let options = format!("{installed}={}|{}|{}|{}", attachment.directory, owner.pid, owner.process_start_ticks, id);
        // Android's shell joins adb arguments: quote the literal pipe-bearing options.
        let quoted = format!("'{options}'");
        self.adb(&["shell", "am", "attach-agent", &owner.package, &quoted], deadline, cancelled, false)?;
        let history = self.read_speech(request, &attachment, evidence, deadline, cancelled)?;
        save_atomic(&evidence.join("speech-attached.json"), &attachment)?;
        Ok((attachment, history))
    }

    pub(crate) fn read_speech(&mut self, request: &RequestId, attachment: &SpeechAttachment, evidence: &Path,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<android_speech::History> {
        self.verify_app_process(&attachment.owner, deadline, cancelled)?;
        let original: SpeechAttachment = frontcap_core::state::load(&self.host_evidence_directory().join("speech-attachment-intent.json"))?
            .ok_or_else(|| io::Error::other("speech attachment intent unavailable"))?;
        if original != *attachment { return Err(io::Error::other("speech attachment belongs to another owner")); }
        let path = format!("{}/speech-observer.jsonl", attachment.directory);
        let mut raw = self.adb(&["exec-out", "run-as", &attachment.owner.package, "head", "-c", "262145", &path], deadline, cancelled, false)?.unwrap();
        std::fs::write(evidence.join("speech-read.jsonl"), &raw)?;
        let mut history = android_speech::History::from_bound_jsonl(&raw, attachment.owner.pid, &attachment.identity)?;
        if history.sampling {
            let key = frontcap_core::canonical::sha256_bytes_hex(request.as_str().as_bytes())[..32].to_owned();
            let index_path = self.host_evidence_directory().join("speech-sample-index.json");
            let index = frontcap_core::state::load::<u64>(&index_path)?.unwrap_or(0).checked_add(1)
                .ok_or_else(|| io::Error::other("speech sample request index exhausted"))?;
            save_atomic(&index_path, &index)?;
            save_atomic(&evidence.join("speech-sample-request.json"), &(key.clone(), index))?;
            let directory = &attachment.directory;
            if !directory.bytes().all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)) || directory.contains("..") {
                return Err(io::Error::other("invalid speech sampling path"));
            }
            let command = format!("'printf \"{index}|{key}\\n\" > {directory}/state-request.tmp && mv {directory}/state-request.tmp {directory}/state-request'");
            self.adb(&["shell", "run-as", &attachment.owner.package, "sh", "-c", &command], deadline, cancelled, false)?;
            let sample_path = format!("{directory}/state-response.json");
            let sample_deadline = deadline.min(Instant::now() + Duration::from_secs(5));
            let sampled = loop {
                if Instant::now() >= sample_deadline {
                    return Err(io::Error::other("speech state sample unavailable within the request budget"));
                }
                if let Some(value) = self.adb(&["exec-out", "run-as", &attachment.owner.package, "head", "-c", "8193", &sample_path], sample_deadline, cancelled, true)? {
                    if value.len() > 8192 { return Err(io::Error::other("speech state sample exceeds bound")); }
                    let sample: android_speech::Sample = serde_json::from_str(&value).map_err(io::Error::other)?;
                    if sample.request == key && sample.request_index == index { break value; }
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            // Read history after the sample so its sequence window has an interpretation.
            std::fs::write(evidence.join("speech-state.json"), &sampled)?;
            raw = self.adb(&["exec-out", "run-as", &attachment.owner.package, "head", "-c", "262145", &path], deadline, cancelled, false)?.unwrap();
            std::fs::write(evidence.join("speech-read.jsonl"), &raw)?;
            history = android_speech::History::from_bound_jsonl(&raw, attachment.owner.pid, &attachment.identity)?;
            history.apply_sample(&sampled, &key, index)?;
        }
        self.verify_app_process(&attachment.owner, deadline, cancelled)?;
        save_atomic(&evidence.join("speech-read.json"), &(attachment, &history))?;
        Ok(history)
    }
}
