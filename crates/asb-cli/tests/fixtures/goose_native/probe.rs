include!(env!("ASB_GOOSE_HOOK_SOURCE"));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{
        extension::PlatformExtensionContext, mcp_client::McpClientTrait,
        platform_extensions::developer::DeveloperClient, ToolCallContext,
    };
    use crate::session::SessionManager;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn broker_round_trip_preserves_native_result_shape() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        let path = root.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"op":"invoke","request":{"tool":"shell","args":{"command":"printf hello"}}})
            );
            reader.get_mut().write_all(b"{\"ok\":true,\"value\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}],\"isError\":false}}\n").await.unwrap();
        });
        let result = request(
            &path,
            &json!({"op":"invoke","request":{"tool":"shell","args":{"command":"printf hello"}}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["value"]["content"][0]["text"], "hello");
        assert_eq!(result["value"]["isError"], false);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_waiting_broker_exchange() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        let path = root.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let token = CancellationToken::new();
        let child_token = token.clone();
        let client_path = path.clone();
        let client = tokio::spawn(async move {
            request(&client_path, &json!({"op":"invoke"}), &child_token).await
        });
        let (_socket, _) = listener.accept().await.unwrap();
        token.cancel();
        assert_eq!(
            client.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[tokio::test]
    async fn delegated_replay_executes_child_and_checks_parent_result() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        for mismatch in [false, true] {
            let socket = root.path().join(if mismatch {
                "different.sock"
            } else {
                "same.sock"
            });
            let listener = UnixListener::bind(&socket).unwrap();
            let scope = if mismatch {
                "asb-delegate-mismatch"
            } else {
                "asb-delegate-match"
            };
            configure(
                scope,
                ProbeConfig {
                    socket,
                    mode: "replay".into(),
                    marker: None,
                },
            );
            let called = Arc::new(tokio::sync::Notify::new());
            let observed = Arc::clone(&called);
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["request"]["extension"], "summon");
                observed.notified().await;
                let value =
                    rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                        if mismatch {
                            "different"
                        } else {
                            "child result"
                        },
                    )]);
                let response = json!({"ok":true,"value":serde_json::to_value(Ok::<_,rmcp::ErrorData>(value)).unwrap()});
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            });
            let actual = async move {
                called.notify_one();
                Ok(rmcp::model::CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text("child result"),
                ]))
            };
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                intercept(
                    scope,
                    json!({"extension":"summon","tool":"delegate","args":{}}),
                    &CancellationToken::new(),
                    actual,
                ),
            )
            .await
            .unwrap();
            assert_eq!(outcome.is_err(), mismatch);
            if let Err(error) = outcome {
                assert!(error.message.contains("differs from recording"));
            }
            server.await.unwrap();
            unconfigure(scope);
        }
    }

    #[tokio::test]
    async fn failed_snapshot_begin_invalidates_without_executing_tool() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        let path = root.path().join("broker.sock");
        let missing_marker = root.path().join("missing.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let message: Value = serde_json::from_str(&line).unwrap();
                let response = if index == 0 {
                    assert_eq!(message["op"], "start");
                    json!({"ok":true,"token":0})
                } else {
                    assert_eq!(message, json!({"op":"invalidate","token":0}));
                    json!({"ok":true})
                };
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let error = dispatch(
            &path,
            "record",
            json!({"tool":"write"}),
            &CancellationToken::new(),
            Some(&missing_marker),
            || async { panic!("tool must not run when its snapshot boundary fails") },
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_active_tool_settles_before_broker_invalidation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        let path = root.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let cleaned = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cleaned);
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let message: Value = serde_json::from_str(&line).unwrap();
                let response = if index == 0 {
                    assert_eq!(message["op"], "start");
                    json!({"ok":true,"token":0})
                } else {
                    assert!(observed.load(Ordering::SeqCst));
                    assert_eq!(message, json!({"op":"invalidate","token":0}));
                    json!({"ok":true})
                };
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let callback_token = token.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            dispatch(
                &path,
                "record",
                json!({"tool":"shell"}),
                &worker_token,
                None,
                || async move {
                    let _ = started.send(());
                    callback_token.cancelled().await;
                    cleaned.store(true, Ordering::SeqCst);
                    Ok(json!({"unused":"after cancellation"}))
                },
            )
            .await
        });
        ready.await.unwrap();
        token.cancel();
        assert_eq!(
            worker.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn broker_preserves_a_real_goose_developer_result() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let client = DeveloperClient::new(PlatformExtensionContext {
            extension_manager: None,
            provider: Arc::new(tokio::sync::Mutex::new(None)),
            session_manager: Arc::new(SessionManager::new(root.path().join("sessions"))),
            scheduler: None,
            use_login_shell_path: false,
        })
        .unwrap();
        let context = ToolCallContext::new("fixture".into(), Some(workspace.clone()), None);
        let args = json!({"path":"note.txt", "content":"goose native result"})
            .as_object()
            .unwrap()
            .clone();
        let result = client
            .call_tool(&context, "write", Some(args), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("note.txt")).unwrap(),
            "goose native result"
        );
        let value = serde_json::to_value(result).unwrap();
        let path = root.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            reader.get_mut().write_all(line.as_bytes()).await.unwrap();
        });
        assert_eq!(
            request(&path, &value, &CancellationToken::new())
                .await
                .unwrap(),
            value
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn real_developer_call_is_recorded_and_not_executed_again() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let client = DeveloperClient::new(PlatformExtensionContext {
            extension_manager: None,
            provider: Arc::new(tokio::sync::Mutex::new(None)),
            session_manager: Arc::new(SessionManager::new(root.path().join("sessions"))),
            scheduler: None,
            use_login_shell_path: false,
        })
        .unwrap();
        let context = ToolCallContext::new("fixture".into(), Some(workspace.clone()), None);
        let arguments = json!({"path":"note.txt", "content":"once"});
        let call = json!({"tool":"write", "args":arguments});
        let path = root.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let expected_call = call.clone();
        let server = tokio::spawn(async move {
            let mut saved = Value::Null;
            for index in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let message: Value = serde_json::from_str(&line).unwrap();
                let response = match index {
                    0 => {
                        assert_eq!(message["request"], expected_call);
                        json!({"ok":true,"token":0})
                    }
                    1 => {
                        saved = message["outcome"]["value"].clone();
                        json!({"ok":true})
                    }
                    _ => {
                        assert_eq!(message["request"], expected_call);
                        json!({"ok":true,"value":saved})
                    }
                };
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let token = CancellationToken::new();
        let recorded = dispatch(&path, "record", call.clone(), &token, None, || async {
            let args = arguments.as_object().unwrap().clone();
            let result = client
                .call_tool(&context, "write", Some(args), CancellationToken::new())
                .await
                .map_err(io::Error::other)?;
            serde_json::to_value(result).map_err(io::Error::other)
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("note.txt")).unwrap(),
            "once"
        );
        std::fs::write(workspace.join("note.txt"), "changed").unwrap();
        let replayed = dispatch(&path, "replay", call, &token, None, || async {
            panic!("replay must not call the native tool")
        })
        .await
        .unwrap();
        assert_eq!(replayed, recorded);
        assert_eq!(
            std::fs::read_to_string(workspace.join("note.txt")).unwrap(),
            "changed"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn real_asb_broker_records_native_call_and_applies_replay_effect() {
        use super::super::{CallRequest, ExtensionManager};
        use crate::agents::extension::ExtensionConfig;
        use rmcp::model::CallToolRequestParams;
        use std::process::Stdio;
        use tokio::process::{Child, Command};

        let Ok(module) = std::env::var("ASB_TEST_NATIVE_BROKER_MODULE") else {
            return;
        };
        let node = std::env::var("ASB_TEST_NODE").expect("set ASB_TEST_NODE");
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("asb-goose-")
            .tempdir_in(base)
            .unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let manager = Arc::new(ExtensionManager::new_without_provider(
            root.path().join("sessions"),
        ));
        let session = manager
            .get_context()
            .session_manager
            .create_session(
                workspace.clone(),
                "asb probe".to_string(),
                crate::session::SessionType::Hidden,
                crate::config::GooseMode::default(),
            )
            .await
            .unwrap();
        manager
            .add_extension(
                ExtensionConfig::Platform {
                    name: "developer".into(),
                    description: String::new(),
                    display_name: None,
                    bundled: None,
                    available_tools: vec![],
                },
                Some(workspace.clone()),
                None,
                Some(&session.id),
            )
            .await
            .unwrap();
        let lease = manager.current_lease(&session.id, Some(&workspace)).await;
        let args = json!({"path":"note.txt", "content":"recorded effect"});
        let events = root.path().join("events.json");
        let snapshot = root.path().join("snapshot");
        let target = workspace.join("note.txt");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/agents/extension_manager/asb_broker_probe.mjs");
        let snapshot_modules = std::env::var("ASB_TEST_SNAPSHOT_MODULES").ok();
        let marker_socket = root.path().join("marker.sock");
        let boundary_module = std::env::var("ASB_TEST_BOUNDARY_MODULE").ok();

        let mut snapshot_service = if let Some(modules) = &snapshot_modules {
            let script = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/agents/extension_manager/asb_snapshot_probe.py");
            let mut child = Command::new("python3")
                .arg(script)
                .arg(modules)
                .arg(&workspace)
                .arg(&marker_socket)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut ready = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut ready),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(ready, "READY\n");
            Some(child)
        } else {
            None
        };

        async fn start(
            node: &str,
            fixture: &Path,
            module: &str,
            mode: &str,
            socket: &Path,
            events: &Path,
            snapshot: &Path,
            target: &Path,
            marker: Option<(&Path, &str)>,
        ) -> Child {
            let mut command = Command::new(node);
            if let Some((path, boundary_module)) = marker {
                command
                    .env("ASB_TEST_MARKER", path)
                    .env("ASB_TEST_BOUNDARY_MODULE", boundary_module);
            }
            let mut child = command
                .arg(fixture)
                .args([module, mode])
                .arg(socket)
                .arg(events)
                .arg(snapshot)
                .arg(target)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut ready = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut ready),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(ready, "READY\n");
            child
        }
        async fn stop(mut child: Child) {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"STOP\n")
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
                    .await
                    .unwrap()
                    .unwrap()
                    .success()
            );
        }
        let token = CancellationToken::new();
        let record_socket = root.path().join("record.sock");
        let record = start(
            &node,
            &fixture,
            &module,
            "record",
            &record_socket,
            &events,
            &snapshot,
            &target,
            snapshot_modules.as_ref().map(|_| {
                (
                    marker_socket.as_path(),
                    boundary_module.as_deref().expect("set boundary module"),
                )
            }),
        )
        .await;
        configure(
            &session.id,
            ProbeConfig {
                socket: record_socket,
                mode: "record".into(),
                marker: snapshot_modules.as_ref().map(|_| marker_socket.clone()),
            },
        );
        let tool = CallToolRequestParams::new("write".to_string())
            .with_arguments(args.as_object().unwrap().clone());
        let recorded = lease
            .call(tool.clone(), CallRequest::new("record"), token.clone())
            .await
            .unwrap()
            .result
            .await
            .unwrap();
        stop(record).await;
        if let Some(service) = snapshot_service.as_mut() {
            service.stdin.as_mut().unwrap().write_all(b"SEAL\n").await.unwrap();
            let mut sealed = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(10), BufReader::new(service.stdout.as_mut().unwrap()).read_line(&mut sealed)).await.unwrap().unwrap();
            assert_eq!(sealed, "SEALED\n");
        }
        if snapshot_modules.is_none() {
            std::fs::copy(&target, &snapshot).unwrap();
        }
        std::fs::write(&target, "changed").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o400)).unwrap();
        let replay_socket = root.path().join("replay.sock");
        let replay = start(
            &node,
            &fixture,
            &module,
            "replay",
            &replay_socket,
            &events,
            &snapshot,
            &target,
            snapshot_modules.as_ref().map(|_| {
                (
                    marker_socket.as_path(),
                    boundary_module.as_deref().expect("set boundary module"),
                )
            }),
        )
        .await;
        configure(
            &session.id,
            ProbeConfig {
                socket: replay_socket,
                mode: "replay".into(),
                marker: None,
            },
        );
        let mut replicas = vec![(
            session.id.clone(),
            lease,
            target,
            replay,
            snapshot_service.take(),
        )];
        if let Some(modules) = &snapshot_modules {
            for index in 2..=4 {
                let next_workspace = root.path().join(format!("workspace-{index}"));
                std::fs::create_dir(&next_workspace).unwrap();
                let next_target = next_workspace.join("note.txt");
                std::fs::write(&next_target, "changed").unwrap();
                std::fs::set_permissions(&next_target, std::fs::Permissions::from_mode(0o400))
                    .unwrap();
                let next_session = manager
                    .get_context()
                    .session_manager
                    .create_session(
                        next_workspace.clone(),
                        format!("replica-{index}"),
                        crate::session::SessionType::Hidden,
                        crate::config::GooseMode::default(),
                    )
                    .await
                    .unwrap();
                let next_lease = manager
                    .current_lease(&next_session.id, Some(&next_workspace))
                    .await;
                let next_marker = root.path().join(format!("marker-{index}.sock"));
                let script = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/agents/extension_manager/asb_snapshot_replay_probe.py");
                let mut service = Command::new("python3")
                    .arg(script)
                    .arg(modules)
                    .arg(root.path().join("snapshots"))
                    .arg(&next_workspace)
                    .arg(&next_marker)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let mut ready = String::new();
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    BufReader::new(service.stdout.as_mut().unwrap()).read_line(&mut ready),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(ready, "READY\n");
                let next_socket = root.path().join(format!("replay-{index}.sock"));
                let next_broker = start(
                    &node,
                    &fixture,
                    &module,
                    "replay",
                    &next_socket,
                    &events,
                    &snapshot,
                    &next_target,
                    Some((
                        &next_marker,
                        boundary_module.as_deref().expect("set boundary module"),
                    )),
                )
                .await;
                configure(
                    &next_session.id,
                    ProbeConfig {
                        socket: next_socket,
                        mode: "replay".into(),
                        marker: None,
                    },
                );
                replicas.push((
                    next_session.id,
                    next_lease,
                    next_target,
                    next_broker,
                    Some(service),
                ));
            }
        }
        if snapshot_modules.is_some() {
            assert_eq!(replicas.len(), 4);
        }
        let results = futures::future::join_all(replicas.iter().map(|(_, lease, _, _, _)| async {
            lease
                .call(tool.clone(), CallRequest::new("replay"), token.clone())
                .await
                .unwrap()
                .result
                .await
                .unwrap()
        }))
        .await;
        let expected = serde_json::to_value(recorded).unwrap();
        for result in results {
            assert_eq!(expected, serde_json::to_value(result).unwrap());
        }
        for (scope, _, target, broker, service) in replicas {
            assert_eq!(std::fs::read_to_string(target).unwrap(), "recorded effect");
            stop(broker).await;
            if let Some(service) = service {
                stop(service).await;
            }
            unconfigure(&scope);
        }
    }
}
