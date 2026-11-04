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
}

/// Adapter-owned dispatch: the upstream hook supplies the resolved call and
/// original future without changing tool schemas or Goose's result processing.
pub async fn intercept<Fut>(
    scope: &str,
    call: Value,
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
