//! External app process acquisition and continuity, independent of WebView existence.
use super::*;
use frontcap_core::native::android_webview::AppProcess;

impl Device {
    pub(super) fn discover_app_process(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let app = self.config.webview_app.as_ref().or(self.config.host_app.as_ref())
            .ok_or_else(|| io::Error::other("external app profile unavailable"))?.clone();
        loop {
            let raw = self.adb(&["shell", "pidof", &app.package], deadline, cancelled, true)?;
            if let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) {
                let pid: u32 = raw.trim().parse().map_err(|_| io::Error::other("ambiguous Android app process"))?;
                if pid == 0 || pid > i32::MAX as u32 { return Err(io::Error::other("invalid Android app PID")); }
                let path = format!("/proc/{pid}/stat");
                let before = self.adb(&["shell", "cat", &path], deadline, cancelled, false)?.unwrap();
                let ticks = crate::android_webview::start_ticks(&before, pid)?;
                let after = self.adb(&["shell", "cat", &path], deadline, cancelled, false)?.unwrap();
                let current = self.adb(&["shell", "pidof", &app.package], deadline, cancelled, false)?.unwrap();
                if crate::android_webview::start_ticks(&after, pid)? != ticks || current.trim() != pid.to_string() {
                    return Err(io::Error::other("Android app process changed during discovery"));
                }
                let process = AppProcess {
                    emulator: ProcessIdentity::of(self.process.as_ref().unwrap().id())?
                        .ok_or_else(|| io::Error::other("emulator identity disappeared"))?,
                    package: app.package, activity: app.activity, serial: format!("emulator-{}", self.config.port),
                    pid, process_start_ticks: ticks, apk_sha256: self.config.apk_sha256.clone(),
                    source_sha256: self.config.source_sha256.clone(),
                };
                save_atomic(&self.host_evidence_directory().join("app-process.json"), &process)?;
                self.app_process = Some(process);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Acquisition evidence only; every subsequent operation verifies live process continuity.
    pub fn app_process(&self) -> Option<&AppProcess> {
        if self.retired.is_none() { self.app_process.as_ref() } else { None }
    }

    pub(crate) fn verify_app_process(&mut self, process: &AppProcess, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<()> {
        if self.retired.is_some() || self.app_process.as_ref() != Some(process) {
            return Err(io::Error::other("Android app identity is not the acquired owner"));
        }
        let pid = self.adb(&["shell", "pidof", &process.package], deadline, cancelled, false)?.unwrap();
        let stat = self.adb(&["shell", "cat", &format!("/proc/{}/stat", process.pid)], deadline, cancelled, false)?.unwrap();
        if pid.trim() != process.pid.to_string()
            || crate::android_webview::start_ticks(&stat, process.pid)? != process.process_start_ticks {
            self.invalidate_contact_capture();
            return Err(io::Error::other("Android app process changed before transport"));
        }
        Ok(())
    }
}

impl Device {
    /// Reuse only the original process's debugger forwarding; never rebind a lost listener.
    pub(crate) fn enumerate_host_pages(&mut self, evidence: &Path, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<Vec<crate::android_webview::Page>> {
        if self.webview_debugger.is_none() { self.discover_webview_debugger(deadline,cancelled)?; }
        if let Some(port) = self.webview_forward {
            let debugger = self.webview_debugger.clone().unwrap();
            self.verify_webview_process(&debugger,deadline,cancelled)?;
            let inventory = self.adb(&["forward","--list"],deadline,cancelled,false)?.unwrap();
            if !crate::android_webview::has_forward(&inventory,&debugger,port) {
                return Err(io::Error::other("original host debugger forwarding disappeared"));
            }
        } else {
            self.connect_webview_debugger(deadline,cancelled).map_err(io::Error::other)?;
        }
        self.inspect_webview_pages(Path::new("/usr/bin/curl"),evidence,deadline,cancelled)
    }
}
