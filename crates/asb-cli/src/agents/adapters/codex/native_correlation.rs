//! Adapter-owned provider and native tool correlation.
type ProviderCall = (String, Option<String>, Option<String>);
/// Codex serializes native command results into its provider protocol. Keep
/// that adapter-specific envelope with the sealed native exchange: its replay
/// shim extracts the completed-shell payload and exit status before Codex
/// renders a fresh volatile envelope.
pub(super) fn bind_provider_tool_outputs(
    sidecar: &[u8],
    capture: &std::path::Path,
) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(capture)
        .map_err(|_| "Codex provider capture is unavailable".to_owned())?;
    if metadata.len() > 256 * 1024 * 1024 {
        return Err("Codex provider capture exceeds its bound".to_owned());
    }
    let capture =
        std::fs::read(capture).map_err(|_| "Codex provider capture is unavailable".to_owned())?;
    let capture = std::str::from_utf8(&capture)
        .map_err(|_| "Codex provider capture is not UTF-8".to_owned())?;
    // Codex's Responses input is a cumulative history.  A `call_id` can be
    // retained for multiple different historical tool outputs, so it is not a
    // global output key.  Derive the newly appended result suffix from each
    // successive snapshot and bind it to the native hook's causal order.
    #[derive(Default)]
    struct History {
        prior: Vec<(String, Vec<u8>)>,
        calls: Vec<ProviderCall>,
        streamed: bool,
    }
    let mut histories = std::collections::BTreeMap::<String, History>::new();
    let mut outputs = std::collections::BTreeMap::<
        (String, String),
        std::collections::VecDeque<(Vec<u8>, Option<u64>, Option<String>)>,
    >::new();
    let mut polls = std::collections::BTreeMap::<(String, String), Vec<serde_json::Value>>::new();
    for line in capture.lines() {
        let row: serde_json::Value = serde_json::from_str(line)
            .map_err(|_| "Codex provider capture is invalid".to_owned())?;
        let Some(body) = row
            .pointer("/request/body")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let body: serde_json::Value = serde_json::from_str(body)
            .map_err(|_| "Codex provider request body is invalid".to_owned())?;
        let metadata = row.get("agent_record_metadata");
        let conversation = metadata
            .and_then(|value| value.get("conversation"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let raw_ids = metadata
            .and_then(|value| value.get("output_call_ids"))
            .and_then(serde_json::Value::as_array);
        if metadata.is_some() && raw_ids.is_none() {
            return Err("Codex provider correlation metadata is invalid".into());
        }
        let waits = metadata
            .and_then(|value| value.get("running_wait_ms"))
            .and_then(serde_json::Value::as_array);
        if waits.is_some_and(|waits| Some(waits.len()) != raw_ids.map(Vec::len)) {
            return Err("Codex provider timing metadata does not match the request".into());
        }
        let sessions = metadata
            .and_then(|value| value.get("running_session_ids"))
            .and_then(serde_json::Value::as_array);
        if sessions.is_some_and(|values| Some(values.len()) != raw_ids.map(Vec::len)) {
            return Err("Codex provider process metadata does not match the request".into());
        }
        let mut current = body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some((
                    item.get("call_id")
                        .and_then(serde_json::Value::as_str)?
                        .to_owned(),
                    item.get("output")
                        .and_then(serde_json::Value::as_str)?
                        .as_bytes()
                        .to_vec(),
                ))
            })
            .collect::<Vec<_>>();
        if let Some(ids) = raw_ids {
            if ids.len() != current.len() {
                return Err(
                    "Codex provider correlation metadata does not match the request".into(),
                );
            }
            for (entry, id) in current.iter_mut().zip(ids) {
                entry.0 = id
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or("Codex provider correlation ID is invalid")?
                    .to_owned();
            }
        }
        let history = histories.entry(conversation.clone()).or_default();
        if !current.starts_with(&history.prior) {
            return Err("Codex provider tool result history diverges".into());
        }
        let appended = &current[history.prior.len()..];
        if history.streamed && appended.len() != history.calls.len() {
            return Err("Codex provider call/result sequence diverges".into());
        }
        if history.streamed {
            let identified = history.calls.iter().all(|(_, id, _)| id.is_some());
            if raw_ids.is_some() && !identified && !history.calls.is_empty() {
                return Err("Codex provider call identity is unavailable".into());
            }
            if identified {
                let unique = history
                    .calls
                    .iter()
                    .filter_map(|(_, id, _)| id.as_deref())
                    .collect::<std::collections::BTreeSet<_>>();
                if unique.len() != history.calls.len() {
                    return Err("Codex provider call identities are duplicated".into());
                }
            }
            let mut pending = history.calls.clone();
            for (index, (id, output)) in appended.iter().enumerate() {
                let position = if identified {
                    pending
                        .iter()
                        .position(|(_, call_id, _)| call_id.as_deref() == Some(id.as_str()))
                        .ok_or("Codex provider call/result identities differ")?
                } else {
                    0
                };
                let (name, _, process) = pending.remove(position);
                let position = history.prior.len() + index;
                let wait = waits
                    .and_then(|values| values.get(position))
                    .and_then(serde_json::Value::as_u64);
                let session = sessions
                    .and_then(|values| values.get(position))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                match name.as_str() {
                    // Codex presents its native Bash lifecycle hook as the
                    // Responses-compatible exec command. The native sidecar
                    // owns only that causal stream.
                    "exec_command" => {
                        outputs
                            .entry((conversation.clone(), id.clone()))
                            .or_default()
                            .push_back((output.clone(), wait, session));
                    }
                    // This provider operation resumes an already-owned
                    // process; it has no PreToolUse native shell boundary.
                    "write_stdin" => {
                        if let Some(process) = process {
                            let text = String::from_utf8(output.clone())
                                .map_err(|_| "Codex provider tool result is not UTF-8")?;
                            polls
                                .entry((conversation.clone(), process))
                                .or_default()
                                .push(serde_json::json!({"output":text,"running_wait_ms":wait}));
                        }
                    }
                    // Other operations are outside the shell hook's scope.
                    // Leave their handling to the agent instead of rejecting
                    // the entire recording merely because they were used.
                    _ => {}
                }
            }
        } else {
            // Fixture/legacy captures without a streamed Responses envelope
            // retain the historical conservative one-result-per-hook route.
            for (index, (id, output)) in appended.iter().enumerate() {
                outputs
                    .entry((conversation.clone(), id.clone()))
                    .or_default()
                    .push_back((
                        output.clone(),
                        waits
                            .and_then(|waits| waits.get(history.prior.len() + index))
                            .and_then(serde_json::Value::as_u64),
                        sessions
                            .and_then(|values| values.get(history.prior.len() + index))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    ));
            }
        }
        history.prior = current;
        let calls = provider_function_calls(&row)?;
        history.streamed |= calls.1;
        history.calls = calls.0;
    }
    if histories
        .values()
        .any(|history| history.streamed && !history.calls.is_empty())
    {
        return Err("Codex provider call has no following result".into());
    }
    let mut rows = sidecar
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| {
            serde_json::from_slice::<serde_json::Value>(line)
                .map_err(|_| "Codex native sidecar is invalid".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut pending = None;
    for row in &mut rows {
        if row.get("event").and_then(serde_json::Value::as_str) == Some("write") {
            let request = row
                .get("request_b64")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "Codex native sidecar request is missing".to_owned())?;
            let request = decode_base64(request)
                .map_err(|_| "Codex native sidecar request is invalid".to_owned())?;
            let request: serde_json::Value = serde_json::from_slice(&request)
                .map_err(|_| "Codex native sidecar request is invalid".to_owned())?;
            pending = Some(request);
        } else if row.get("event").and_then(serde_json::Value::as_str) == Some("output") {
            let request = pending
                .take()
                .ok_or_else(|| "Codex native sidecar output has no request".to_owned())?;
            let broker = request.get("ordinal").is_some();
            let identity = if broker {
                let encoded = row
                    .get("output_b64")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("Codex native broker outcome is missing")?;
                let bytes =
                    decode_base64(encoded).map_err(|_| "Codex native broker outcome is invalid")?;
                let outcome: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|_| "Codex native broker outcome is invalid")?;
                if outcome.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                    return Err("Codex native broker outcome is invalid".into());
                }
                outcome
                    .get("value")
                    .cloned()
                    .ok_or("Codex native broker identity is missing")?
            } else {
                request
            };
            let session = identity
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let hook_id = identity
                .get("tool_use_id")
                .and_then(serde_json::Value::as_str)
                .ok_or("Codex native sidecar has no tool identity")?
                .to_owned();
            let key = if let Some(session) = session {
                (session, hook_id.clone())
            } else {
                let matching = outputs
                    .keys()
                    .filter(|(_, id)| id == &hook_id)
                    .collect::<Vec<_>>();
                if matching.is_empty() {
                    return Err("Codex provider capture has no matching native tool result".into());
                }
                if matching.len() != 1 {
                    return Err("Codex provider call identity is missing or ambiguous".into());
                }
                matching[0].clone()
            };
            let (output, running_wait_ms, process) = outputs
                .get_mut(&key)
                .and_then(|queue| queue.pop_front())
                .ok_or_else(|| {
                    "Codex provider capture has no matching native tool result".to_owned()
                })?;
            let text = String::from_utf8(output)
                .map_err(|_| "Codex provider tool result is not UTF-8".to_owned())?;
            let result = if broker {
                if text
                    .lines()
                    .nth(2)
                    .is_some_and(|line| line.starts_with("Process running with session ID "))
                    && running_wait_ms.is_none()
                {
                    return Err("Codex running tool result has no recorded response timing".into());
                }
                let followups = process
                    .and_then(|id| polls.remove(&(key.0.clone(), id)))
                    .unwrap_or_default();
                serde_json::json!({"ok":true,"value":{"output":text,"running_wait_ms":running_wait_ms,"followups":followups}})
            } else {
                serde_json::json!(text)
            };
            let output = serde_json::to_vec(&result)
                .map_err(|_| "cannot encode Codex provider tool result".to_owned())?;
            row["output_b64"] = serde_json::Value::String(encode_base64(&output));
            row["response_sha256"] = serde_json::Value::String(crate::cassette::digest(&output));
        }
    }
    if pending.is_some() || outputs.values().any(|queue| !queue.is_empty()) {
        return Err("Codex native/provider tool result inventory does not match".into());
    }
    let final_event = rows
        .last()
        .and_then(|row| row.get("event"))
        .and_then(serde_json::Value::as_str);
    if final_event == Some("workspace_effect") {
        let prefix = rows[..rows.len() - 1]
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "cannot encode Codex native sidecar".to_owned())?
            .join("\n");
        rows.last_mut().expect("checked sidecar effect")["sha256"] =
            serde_json::Value::String(crate::cassette::digest(format!("{prefix}\n").as_bytes()));
    } else if final_event != Some("exit") {
        return Err("Codex native sidecar has no workspace effect".into());
    }
    let encoded = rows
        .into_iter()
        .map(|row| serde_json::to_string(&row))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "cannot encode Codex native sidecar".to_owned())?
        .join("\n")
        + "\n";
    Ok(encoded.into_bytes())
}

/// Return the function calls completed by one streamed Responses response.
/// Their results appear as the newly appended cumulative input items of the
/// next request, so this adapter-local sequence identifies native shell calls
/// without interpreting generic tape data.
pub(super) fn provider_function_calls(
    row: &serde_json::Value,
) -> Result<(Vec<ProviderCall>, bool), String> {
    let Some(body) = row
        .pointer("/response/body")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok((Vec::new(), false));
    };
    let mut calls = Vec::new();
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        // OpenRouter terminates streamed Responses exchanges with the standard
        // SSE sentinel. It carries no provider item, so it is not a malformed
        // Codex response and must not invalidate an otherwise sealed capture.
        if payload == "[DONE]" {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|_| "Codex provider response is invalid".to_owned())?;
        if value.get("type").and_then(serde_json::Value::as_str)
            != Some("response.output_item.done")
        {
            continue;
        }
        let Some(item) = value.get("item") else {
            return Err("Codex provider response is invalid".into());
        };
        if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call") {
            calls.push((
                item.get("name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| "Codex provider function call is invalid".to_owned())?
                    .to_owned(),
                item.get("call_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                item.get("arguments")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                    .and_then(|value| {
                        value.get("session_id").map(|id| {
                            id.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| id.to_string())
                        })
                    }),
            ));
        }
    }
    Ok((calls, true))
}

pub(super) fn decode_base64(value: &str) -> Result<Vec<u8>, ()> {
    fn digit(byte: u8) -> Result<u8, ()> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(()),
        }
    }
    if !value.len().is_multiple_of(4) {
        return Err(());
    }
    let mut bytes = Vec::with_capacity(value.len() / 4 * 3);
    for chunk in value.as_bytes().chunks_exact(4) {
        let (a, b) = (digit(chunk[0])?, digit(chunk[1])?);
        let (pad_c, pad_d) = (chunk[2] == b'=', chunk[3] == b'=');
        if pad_c && !pad_d {
            return Err(());
        }
        let c = if pad_c { 0 } else { digit(chunk[2])? };
        let d = if pad_d { 0 } else { digit(chunk[3])? };
        bytes.push((a << 2) | (b >> 4));
        if !pad_c {
            bytes.push((b << 4) | (c >> 2));
        }
        if !pad_d {
            bytes.push((c << 6) | d);
        }
    }
    Ok(bytes)
}

pub(super) fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(ALPHABET[(a >> 2) as usize] as char);
        output.push(ALPHABET[((a & 3) << 4 | b >> 4) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((b & 15) << 2 | c >> 6) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}
