// Experimental Goose hook owned by ASB; not a production replay implementation.
use serde_json::Value;
use std::{io, path::Path};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct ProbeConfig {
    pub socket: std::path::PathBuf,
    pub mode: String,
    pub marker: Option<std::path::PathBuf>,
}

fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, ProbeConfig>> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, ProbeConfig>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub fn configure(scope: &str, config: ProbeConfig) {
    registry().lock().unwrap().insert(scope.to_string(), config);
}

pub fn configuration(scope: &str) -> Option<ProbeConfig> {
    if let Some(config) = registry().lock().unwrap().get(scope).cloned() {
        return Some(config);
    }
    #[cfg(feature = "asb-native-probe")]
    if let (Some(socket), Ok(mode)) = (
        std::env::var_os("ASB_GOOSE_PROBE_SOCKET"),
        std::env::var("ASB_GOOSE_PROBE_MODE"),
    ) {
        return Some(ProbeConfig {
            socket: socket.into(),
            mode,
            marker: std::env::var_os("ASB_GOOSE_PROBE_MARKER").map(Into::into),
        });
    }
    None
}

pub fn unconfigure(scope: &str) {
    registry().lock().unwrap().remove(scope);
    identities().lock().unwrap().remove(scope);
}

fn identities() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, std::collections::HashMap<String, String>>,
> {
    static IDS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<String, std::collections::HashMap<String, String>>,
        >,
    > = std::sync::OnceLock::new();
    IDS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn publish_identities(
    all: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
) -> io::Result<()> {
    let Some(path) = std::env::var_os("ASB_GOOSE_ID_MAP_FILE") else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, all).map_err(io::Error::other)?;
        file.flush()?;
        std::fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

async fn publish_proxy_identity(live: &str, recorded: &str) -> io::Result<()> {
    let Ok(url) = std::env::var("ASB_GOOSE_ID_MAP_URL") else {
        return Ok(());
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(io::Error::other)?;
    let response = client
        .post(url)
        .json(&serde_json::json!({"live": live, "recorded": recorded}))
        .send()
        .await
        .map_err(io::Error::other)?;
    if response.status() != reqwest::StatusCode::NO_CONTENT {
        return Err(io::Error::other(
            "replay proxy rejected the session mapping",
        ));
    }
    Ok(())
}

fn canonicalize_delegation(value: &mut Value, live: &str, recorded: &str) {
    let Some(result) = value.get_mut("Ok") else {
        return;
    };
    if let Some(meta) = result.get_mut("_meta").and_then(Value::as_object_mut) {
        if meta.get("subagent_session_id").and_then(Value::as_str) == Some(live) {
            meta.insert("subagent_session_id".into(), Value::String(recorded.into()));
            if meta.get("duration_secs").is_some_and(Value::is_number) {
                meta.insert("duration_secs".into(), Value::from(0));
            }
        }
    }
    let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for item in content {
        let Some(Value::String(text)) = item.get_mut("text") else {
            continue;
        };
        let task_prefix = format!("Task {live} started in background:");
        let result_prefix = format!("# Background Task Result: {live}\n");
        if text.starts_with(&task_prefix) {
            *text = text.replacen(
                &task_prefix,
                &format!("Task {recorded} started in background:"),
                1,
            );
            let suffix = format!("use load(source: \"{live}\").");
            if text.ends_with(&suffix) {
                text.truncate(text.len() - suffix.len());
                text.push_str(&format!("use load(source: \"{recorded}\")."));
            }
        } else if text.starts_with(&result_prefix) {
            *text = text.replacen(
                &result_prefix,
                &format!("# Background Task Result: {recorded}\n"),
                1,
            );
            if let Some((header, output)) = text.split_once("\n\n## Output\n\n") {
                static DURATION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
                let duration = DURATION.get_or_init(|| {
                    regex::Regex::new(r"(?m)^(\*\*Duration:\*\* )\d+s").expect("static regex")
                });
                *text = format!(
                    "{}\n\n## Output\n\n{}",
                    duration.replace(header, "${1}<duration>s"),
                    output
                );
            }
        }
    }
}

/// Resolve a recorded background-task handle to the live Goose child before
/// executing its `load` call. The broker still receives the original request.
pub fn rewrite_arguments(scope: &str, call: &Value, args: &mut Option<rmcp::model::JsonObject>) {
    if configuration(scope).is_none_or(|probe| probe.mode != "replay")
        || call.get("extension").and_then(Value::as_str) != Some("summon")
        || call.get("tool").and_then(Value::as_str) != Some("load")
    {
        return;
    }
    let Some(source) = args
        .as_ref()
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let live = identities().lock().unwrap().get(scope).and_then(|ids| {
        ids.iter()
            .find_map(|(live, recorded)| (recorded == source).then(|| live.clone()))
    });
    if let (Some(live), Some(args)) = (live, args.as_mut()) {
        args.insert("source".into(), Value::String(live));
    }
}

/// Adapter-owned dispatch: the upstream hook supplies the resolved call and
/// original future without changing tool schemas or Goose's result processing.
pub async fn intercept<Fut>(
    scope: &str,
    mut call: Value,
    cancel: &CancellationToken,
    invoke: Fut,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData>
where
    Fut: std::future::Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>>,
{
    let Some(probe) = configuration(scope) else {
        return invoke.await;
    };
    let internal = |message: String| {
        rmcp::ErrorData::new(rmcp::model::ErrorCode::INTERNAL_ERROR, message, None)
    };
    // Delegation must execute on replay so the child can consume its own LLM
    // and tool exchanges. Submit the parent exchange concurrently: the
    // broker preserves recorded dispatch/completion order around the child.
    let delegated = matches!(
        call.get("extension").and_then(Value::as_str),
        Some("summon" | "orchestrator")
    );
    if probe.mode == "replay"
        && call.get("extension").and_then(Value::as_str) == Some("summon")
        && call.get("tool").and_then(Value::as_str) == Some("load")
    {
        if let Some(source) = call.pointer("/args/source").and_then(Value::as_str) {
            let mapped = identities()
                .lock()
                .unwrap()
                .get(scope)
                .and_then(|ids| ids.get(source))
                .cloned();
            if let Some(recorded) = mapped {
                call["args"]["source"] = Value::String(recorded);
            }
        }
    }
    if delegated && probe.mode == "replay" {
        let recorded = dispatch(
            &probe.socket,
            &probe.mode,
            call.clone(),
            cancel,
            probe.marker.as_deref(),
            || async {
                Err(io::Error::other(
                    "delegation replay must not invoke broker callback",
                ))
            },
        );
        let (recorded, actual) = tokio::join!(recorded, invoke);
        let value = recorded.map_err(|error| internal(error.to_string()))?;
        let sealed =
            serde_json::from_value::<Result<rmcp::model::CallToolResult, rmcp::ErrorData>>(value)
                .map_err(|error| internal(error.to_string()))?;
        let mut sealed_value =
            serde_json::to_value(&sealed).map_err(|error| internal(error.to_string()))?;
        let mut actual_value =
            serde_json::to_value(&actual).map_err(|error| internal(error.to_string()))?;
        if call.get("extension").and_then(Value::as_str) == Some("summon") {
            let live = actual_value
                .pointer("/Ok/_meta/subagent_session_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let recorded_id = sealed_value
                .pointer("/Ok/_meta/subagent_session_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let (Some(live), Some(recorded_id)) = (live, recorded_id) {
                {
                    let mut all = identities().lock().unwrap();
                    let ids = all.entry(scope.to_string()).or_default();
                    if ids
                        .get(&live)
                        .is_some_and(|existing| existing != &recorded_id)
                        || ids
                            .iter()
                            .any(|(other, value)| other != &live && value == &recorded_id)
                    {
                        return Err(internal(
                            "delegated session mapping conflicts with recording".into(),
                        ));
                    }
                    ids.insert(live.clone(), recorded_id.clone());
                    publish_identities(&all)
                        .map_err(|_| internal("cannot publish delegated session mapping".into()))?;
                }
                publish_proxy_identity(&live, &recorded_id)
                    .await
                    .map_err(|_| {
                        internal(
                            "cannot synchronize delegated session mapping with replay proxy".into(),
                        )
                    })?;
                canonicalize_delegation(&mut actual_value, &live, &recorded_id);
                canonicalize_delegation(&mut sealed_value, &recorded_id, &recorded_id);
            }
        }
        if sealed_value != actual_value {
            return Err(internal(
                "delegated tool result differs from recording".into(),
            ));
        }
        return actual;
    }
    let value = dispatch(
        &probe.socket,
        &probe.mode,
        call,
        cancel,
        probe.marker.as_deref(),
        || async { serde_json::to_value(invoke.await).map_err(std::io::Error::other) },
    )
    .await
    .map_err(|error| internal(error.to_string()))?;
    serde_json::from_value::<Result<rmcp::model::CallToolResult, rmcp::ErrorData>>(value)
        .map_err(|error| internal(error.to_string()))?
}

/// Exchange one bounded newline-delimited broker request without losing a
/// cancellation signal. Callers must invalidate any already-started exchange
/// if this operation is cancelled or fails.
pub async fn request(
    path: &Path,
    message: &Value,
    cancel: &CancellationToken,
) -> io::Result<Value> {
    let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() >= MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker request too large",
        ));
    }
    bytes.push(b'\n');
    let operation = async {
        let mut socket = UnixStream::connect(path).await?;
        socket.write_all(&bytes).await?;
        let mut reader = BufReader::new(socket).take((MAX_FRAME + 1) as u64);
        let mut response = Vec::new();
        reader.read_until(b'\n', &mut response).await?;
        if response.len() > MAX_FRAME || !response.ends_with(b"\n") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid broker response",
            ));
        }
        serde_json::from_slice(&response).map_err(io::Error::other)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "broker request cancelled")),
        result = operation => result,
    }
}

async fn boundary(
    path: &Path,
    action: &str,
    token: u64,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let operation = async {
        let mut socket = UnixStream::connect(path).await?;
        let message = format!(
            "tool-{action}:goose-{token}:{}:{}\n",
            std::process::id(),
            unsafe { libc::syscall(libc::SYS_gettid) }
        );
        socket.write_all(message.as_bytes()).await?;
        let mut reader = BufReader::new(socket).take(33);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if !line.ends_with('\n') || line.trim().parse::<u64>().is_err() {
            return Err(io::Error::other("snapshot boundary rejected"));
        }
        Ok(())
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "snapshot boundary cancelled")),
        result = operation => result,
    }
}

async fn invalidate(path: &Path, token: u64) -> io::Result<()> {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        request(
            path,
            &serde_json::json!({"op":"invalidate","token":token}),
            &CancellationToken::new(),
        ),
    )
    .await
    .map_err(|_| io::Error::other("broker invalidation timed out"))??;
    if response["ok"] != true {
        return Err(io::Error::other("broker invalidation rejected"));
    }
    Ok(())
}

/// Test-only native broker protocol probe. Notifications remain unhandled.
pub async fn dispatch<F, Fut>(
    path: &Path,
    mode: &str,
    call: Value,
    cancel: &CancellationToken,
    marker: Option<&Path>,
    invoke: F,
) -> io::Result<Value>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = io::Result<Value>>,
{
    use serde_json::json;
    if mode == "replay" {
        let result = request(path, &json!({"op":"invoke","request":call}), cancel).await?;
        if result["ok"] != true {
            return Err(io::Error::other("broker rejected replay"));
        }
        return result
            .get("value")
            .cloned()
            .ok_or_else(|| io::Error::other("broker response missing value"));
    }
    if mode != "record" {
        return Err(io::Error::other("unknown broker mode"));
    }
    let start = request(path, &json!({"op":"start","request":call}), cancel).await?;
    let token = start
        .get("token")
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::other("broker response missing token"))?;
    if start["ok"] != true {
        return Err(io::Error::other("broker rejected recording"));
    }
    if let Some(marker) = marker {
        if let Err(error) = boundary(marker, "begin", token, cancel).await {
            invalidate(path, token).await?;
            return Err(error);
        }
    }
    let result = invoke().await;
    if cancel.is_cancelled() || result.is_err() {
        invalidate(path, token).await?;
        return if cancel.is_cancelled() {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "native tool cancelled",
            ))
        } else {
            result
        };
    }
    if let Some(marker) = marker {
        if let Err(error) = boundary(marker, "end", token, cancel).await {
            invalidate(path, token).await?;
            return Err(error);
        }
    }
    let outcome = match &result {
        Ok(value) => json!({"op":"finish","token":token,"outcome":{"ok":true,"value":value}}),
        Err(_) => json!({"op":"invalidate","token":token}),
    };
    let finish = match request(path, &outcome, cancel).await {
        Ok(value) => value,
        Err(error) => {
            invalidate(path, token).await?;
            return Err(error);
        }
    };
    if finish["ok"] != true {
        return Err(io::Error::other("broker rejected completion"));
    }
    result
}
