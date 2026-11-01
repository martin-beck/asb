//! Test-only recorder used to qualify non-llmtape replay through a real agent.
use crate::proxy::{ProxyAdapter, ProxySession, RecordingRequest};
use crate::{Error, usage::TokenUsage};
use std::any::Any;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(super) struct Synthetic;
pub(super) static ADAPTER: Synthetic = Synthetic;
struct Settings {
    path: PathBuf,
    mode: crate::tape::Mode,
}
struct Session {
    url: String,
    stop: Arc<AtomicBool>,
    served: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    path: PathBuf,
    mode: crate::tape::Mode,
    capture: Option<crate::proxy::capture::CaptureEvidence>,
}

impl ProxyAdapter for Synthetic {
    fn name(&self) -> &'static str {
        "synthetic-recorder"
    }
    fn default_recording(&self) -> bool {
        std::env::var_os("ASB_SYNTHETIC_RECORDER_CHILD").is_some()
    }
    fn supports_offline_replay(&self) -> bool {
        true
    }
    fn input_files(&self, _: &Path) -> Result<Vec<PathBuf>, Error> {
        Ok(Vec::new())
    }
    fn recording_settings(&self, request: &RecordingRequest<'_>) -> Result<Box<dyn Any>, Error> {
        Ok(Box::new(Settings {
            path: request.cassette.to_owned(),
            mode: request.mode,
        }))
    }

    fn validate_replay_model(&self, path: &Path, model: &str) -> Result<(), Error> {
        let text = std::fs::read_to_string(path).map_err(crate::io_error)?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| Error::Input("invalid synthetic cassette".into()))?;
        if value["fixture_model"] == model {
            Ok(())
        } else {
            Err(Error::Input("synthetic model mismatch".into()))
        }
    }
    fn replay_interface(&self, _: &Path) -> Result<Option<crate::endpoint::Interface>, Error> {
        Ok(Some(crate::endpoint::Interface::OpenAiChat))
    }
    fn recorded_usage(&self, text: &str, _: bool) -> TokenUsage {
        if serde_json::from_str::<serde_json::Value>(text).is_err() {
            return TokenUsage::default();
        }
        TokenUsage {
            input_tokens: Some(11),
            output_tokens: Some(7),
            cached_tokens: Some(2),
            total_tokens: Some(18),
            ..Default::default()
        }
    }
    fn recorded_tool_calls(&self, _: &Path) -> Option<Vec<crate::proxy::tool_calls::ToolCall>> {
        Some(vec![crate::proxy::tool_calls::ToolCall {
            ordinal: 1,
            source: "synthetic.provider_response",
            kind: "function".into(),
            name: Some("fixture".into()),
            id: None,
            arguments_present: false,
            response_observed_unix_ms: None,
            execution_duration_ms: None,
            classes: Vec::new(),
        }])
    }
    fn start(
        &self,
        upstream: Option<&str>,
        settings: Option<&dyn Any>,
    ) -> Result<Box<dyn ProxySession>, Error> {
        let settings = settings
            .and_then(|s| s.downcast_ref::<Settings>())
            .ok_or_else(|| Error::Input("missing synthetic settings".into()))?;
        if settings.mode == crate::tape::Mode::Replay {
            if upstream.is_some() {
                return Err(Error::Input("synthetic replay rejects upstream".into()));
            }
            self.validate_replay_model(&settings.path, "fixture")?;
        }
        let listener = TcpListener::bind("127.0.0.1:0").map_err(crate::io_error)?;
        listener.set_nonblocking(true).map_err(crate::io_error)?;
        let url = format!("http://{}", listener.local_addr().map_err(crate::io_error)?);
        let stop = Arc::new(AtomicBool::new(false));
        let served = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_served = served.clone();
        let thread = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut input = [0u8; 8192];
                        if stream.read(&mut input).unwrap_or(0) == 0 {
                            continue;
                        }
                        let body = b"{\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}";
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(body);
                        worker_served.store(true, Ordering::Relaxed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Box::new(Session {
            url,
            stop,
            served,
            thread: Some(thread),
            path: settings.path.clone(),
            mode: settings.mode,
            capture: None,
        }))
    }
}
impl ProxySession for Session {
    fn url(&self) -> &str {
        &self.url
    }
    fn cleanup_confirmed(&self) -> bool {
        self.thread.is_none()
    }
    fn output_files(&self) -> Vec<PathBuf> {
        if self.mode == crate::tape::Mode::Record {
            vec![self.path.clone()]
        } else {
            Vec::new()
        }
    }
    fn capture_evidence(&self) -> Option<crate::proxy::capture::CaptureEvidence> {
        self.capture.clone()
    }
    fn finish(&mut self) -> Result<(), Error> {
        self.abort();
        if !self.served.load(Ordering::Relaxed) {
            return Err(Error::Run("synthetic recorder saw no request".into()));
        }
        if self.mode == crate::tape::Mode::Record {
            let bytes = b"{\"fixture_model\":\"fixture\"}\n";
            std::fs::write(&self.path, bytes).map_err(crate::io_error)?;
            self.capture = Some(crate::proxy::capture::CaptureEvidence {
                adapter: "synthetic-recorder".into(),
                output: self.path.clone(),
                complete: true,
                accepted: 1,
                completed: 1,
                failed: 0,
                recorded: 1,
                inference_requests: 1,
                bytes: bytes.len() as u64,
                sha256: crate::cassette::digest(bytes),
            });
        }
        Ok(())
    }
    fn abort(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
