//! Local cli2key API lifecycle and client configuration.

use crate::Error;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Mutex;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
#[path = "custody.rs"]
mod custody;
#[cfg(not(unix))]
#[path = "custody_unsupported.rs"]
mod custody;
#[cfg(unix)]
#[path = "proxy.rs"]
mod proxy;

#[cfg(unix)]
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
// Upstream cannot bind port zero. Serialize startup through readiness, not
// session lifetime, so our concurrent sessions select different free ports.
static STARTUP: Mutex<()> = Mutex::new(());
const BUILTIN_COMPAT_PROXY: &str = include_str!("../../../../assets/scripts/cli2key-proxy.mjs");
const CHAT_COMPAT: &str = include_str!("../../../../assets/scripts/cli2key-chat-compat.mjs");
const NETWORK_FETCH: &str = include_str!("../../../../assets/scripts/network-fetch.mjs");

// Each proxy owns its modules. Separate ASB processes have independent
// startup mutexes and must never truncate files another proxy is loading.
struct RuntimeFiles {
    path: std::path::PathBuf,
    retain: bool,
}

impl RuntimeFiles {
    fn create(parent: &std::path::Path) -> Result<Self, Error> {
        for _ in 0..100 {
            let path = parent.join(format!("cli2key-{}", crate::tools::runtime::unique_id()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => {
                    let files = Self {
                        path,
                        retain: false,
                    };
                    for (name, source) in [
                        ("network-fetch.mjs", NETWORK_FETCH),
                        ("cli2key-chat-compat.mjs", CHAT_COMPAT),
                        ("cli2key-proxy.mjs", BUILTIN_COMPAT_PROXY),
                    ] {
                        std::fs::write(files.path.join(name), source).map_err(|error| {
                            Error::Run(format!(
                                "cannot materialize cli2key proxy module {name}: {error}"
                            ))
                        })?;
                    }
                    return Ok(files);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(Error::Run(format!(
                        "cannot allocate private cli2key proxy directory: {error}"
                    )));
                }
            }
        }
        Err(Error::Run(
            "cannot allocate a unique cli2key proxy directory".into(),
        ))
    }
}

struct LocalResources {
    owner: Option<custody::Local>,
    files: Option<RuntimeFiles>,
}

impl LocalResources {
    fn finish(&mut self) -> Result<(), Error> {
        let result = self
            .owner
            .take()
            .map_or(Ok(()), |owner| owner.finish().map_err(Error::Run));
        if let Some(mut files) = self.files.take() {
            files.retain = result.is_err();
        }
        result
    }
}

impl Drop for LocalResources {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

impl Drop for RuntimeFiles {
    fn drop(&mut self) {
        if !self.retain {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

pub(in crate::providers) fn prepare(
    command: &mut Command,
    model: &str,
    bridge: Option<(&str, &str)>,
) -> Result<(), Error> {
    let (key, base_url) = bridge.ok_or_else(|| {
        Error::Run("cli2key provider has no API endpoint; run `asb tool install cli2key` or set CLI2KEY_BASE_URL and CLI2KEY_API_KEY".into())
    })?;
    command
        .env("CLI2KEY_API_KEY", key)
        .env("CLI2KEY_BASE_URL", base_url)
        .env("CLI2KEY_MODEL", model)
        .env("OPENAI_API_KEY", key)
        .env("OPENAI_BASE_URL", base_url)
        .env("OPENAI_API_BASE", base_url)
        .env("OPENAI_MODEL", model)
        .env("ANTHROPIC_MODEL", model)
        .env("ANTHROPIC_API_KEY", key)
        .env("ANTHROPIC_BASE_URL", base_url.trim_end_matches("/v1"));
    Ok(())
}

pub(in crate::providers) fn local_executable() -> Option<String> {
    let base = crate::project::network_root();
    let (root, config) = crate::project::load(&base).ok()?;
    local_executable_at(
        &root,
        &config,
        &crate::project::network_invocation_dir(),
        std::env::var_os("ASB_TOOL_DIR").as_deref(),
    )
}

fn local_executable_at(
    root: &std::path::Path,
    config: &crate::project::ProjectConfig,
    invocation_dir: &std::path::Path,
    override_dir: Option<&std::ffi::OsStr>,
) -> Option<String> {
    let mut candidates = Vec::new();
    if let Some(directory) = override_dir {
        candidates.push(invocation_dir.join(directory).join("codex-bridge"));
    }
    candidates.push(
        crate::project::resolve(root, &config.tools_dir)
            .join("cli2key/node_modules/.bin/codex-bridge"),
    );
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn relative_bridge_override_uses_invocation_directory_inside_project() {
        let root = std::env::temp_dir().join(format!(
            "asb-cli2key-context-{}",
            crate::tools::runtime::unique_id()
        ));
        let nested = root.join("nested");
        std::fs::create_dir_all(nested.join("custom")).unwrap();
        std::fs::create_dir_all(root.join("custom")).unwrap();
        let selected = nested.join("custom/codex-bridge");
        std::fs::write(&selected, "nested").unwrap();
        std::fs::write(root.join("custom/codex-bridge"), "root").unwrap();
        assert_eq!(
            local_executable_at(
                &root,
                &crate::project::ProjectConfig::default(),
                &nested,
                Some(std::ffi::OsStr::new("custom"))
            ),
            Some(
                selected
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// A cli2key API endpoint and the optional server process ASB owns.
pub struct Bridge {
    api_key: String,
    base_url: String,
    _session: Session,
}

enum Session {
    External,
    Local { resources: LocalResources },
    Finished,
}

impl crate::providers::ProviderSession for Bridge {
    fn api_key(&self) -> &str {
        Bridge::api_key(self)
    }
    fn base_url(&self) -> &str {
        Bridge::base_url(self)
    }
    fn finish(&mut self) -> Result<(), Error> {
        if let Session::Local { mut resources } =
            std::mem::replace(&mut self._session, Session::Finished)
        {
            resources.finish()
        } else {
            Ok(())
        }
    }
}

impl Bridge {
    /// Connect to a configured endpoint or start the project-local bridge.
    pub fn connect(executable: Option<&str>) -> Result<Self, Error> {
        if let Some(endpoint) = std::env::var_os("CLI2KEY_BASE_URL") {
            let base_url = normalize_endpoint(&endpoint.to_string_lossy())?;
            let api_key = required_key()?;
            return Ok(Self {
                api_key,
                base_url,
                _session: Session::External,
            });
        }
        let executable = executable.ok_or(Error::Run(
            "cli2key provider needs a local bridge; run `asb tool install cli2key` in the initialized project, or set CLI2KEY_BASE_URL and CLI2KEY_API_KEY for an external bridge".into(),
        ))?;
        let _startup = STARTUP.lock().map_err(|_| {
            Error::Run("cli2key bridge startup coordination failed; retry the run".into())
        })?;
        let host = std::env::var("CODEX_BRIDGE_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let requested_port = match std::env::var("CODEX_BRIDGE_PORT") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => return Err(Error::Run("CODEX_BRIDGE_PORT must be valid UTF-8".into())),
        };
        let requested_port = requested_port
            .map(|port| {
                port.parse::<u16>()
                    .ok()
                    .filter(|value| *value != 0)
                    .ok_or_else(|| {
                        Error::Run("CODEX_BRIDGE_PORT must be an integer from 1 to 65535".into())
                    })
            })
            .transpose()?;
        let client_host = if host == "0.0.0.0" || host == "::" {
            "127.0.0.1"
        } else {
            host.as_str()
        };
        let (mut resources, discovered_key, port_number) =
            start_bridge_with(executable, &host, requested_port, free_port)?;
        let base_url = format!("http://{client_host}:{port_number}/v1");
        let result: Result<(String, String), Error> = (|| {
            let api_key = std::env::var("CLI2KEY_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(discovered_key);
            let proxy_root = crate::project::network_invocation_dir().join(".asb-runtime");
            std::fs::create_dir_all(&proxy_root).map_err(|error| {
                Error::Run(format!(
                    "cannot create cli2key compatibility proxy directory {}: {error}",
                    proxy_root.display()
                ))
            })?;
            let mut files = RuntimeFiles::create(&proxy_root)?;
            files.retain = true;
            resources.files = Some(files);
            let upstream = base_url.clone();
            let mut command = Command::new("node");
            command.current_dir(crate::project::network_invocation_dir());
            crate::project::apply_network_proxy(&mut command, std::path::Path::new("."))?;
            command
                .arg(
                    resources
                        .files
                        .as_ref()
                        .expect("runtime files")
                        .path
                        .join("cli2key-proxy.mjs"),
                )
                .args(["--upstream", &upstream])
                .stdin(Stdio::piped())
                .stderr(Stdio::null());
            let owner = resources.owner.as_mut().expect("live owner");
            let address = owner.spawn_proxy(command).map_err(Error::Run)?;
            owner.confirm_ready().map_err(Error::Run)?;
            Ok((api_key, format!("http://{address}/v1")))
        })();
        match result {
            Ok((api_key, base_url)) => Ok(Self {
                api_key,
                base_url,
                _session: Session::Local { resources },
            }),
            Err(error) => match resources.finish() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Run(format!("{error}; {cleanup}"))),
            },
        }
    }

    /// Return the API key to configure the selected agent.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Return the OpenAI-compatible base URL to configure the selected agent.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

fn start_bridge_with(
    executable: &str,
    host: &str,
    requested: Option<u16>,
    mut allocate: impl FnMut() -> Result<u16, Error>,
) -> Result<(LocalResources, String, u16), Error> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        let port = match requested {
            Some(port) => port,
            None => allocate()?,
        };
        let mut command = Command::new(executable);
        command.current_dir(crate::project::network_invocation_dir());
        crate::project::apply_network_proxy(&mut command, std::path::Path::new("."))?;
        command
            .arg("serve")
            .env("CODEX_BRIDGE_HOST", host)
            .env("CODEX_BRIDGE_PORT", port.to_string())
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        let mut resources = LocalResources {
            owner: Some(custody::Local::new().map_err(Error::Run)?),
            files: None,
        };
        match resources
            .owner
            .as_mut()
            .expect("live owner")
            .spawn_bridge(&mut command)
        {
            Ok(key) => return Ok((resources, key, port)),
            Err(reason) => {
                if let Err(cleanup) = resources.finish() {
                    return Err(Error::Run(format!("{reason}; {cleanup}")));
                }
                let occupied = TcpListener::bind((host, port))
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::AddrInUse);
                if requested.is_none() && occupied && attempt + 1 < ATTEMPTS {
                    continue;
                }
                let detail = if occupied {
                    format!(
                        "; port {port} is in use; unset CODEX_BRIDGE_PORT or choose a free port and retry"
                    )
                } else {
                    String::new()
                };
                return Err(Error::Run(format!("{reason}{detail}")));
            }
        }
    }
    unreachable!("the final attempt returns")
}

fn free_port() -> Result<u16, Error> {
    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| {
            Error::Run(format!(
                "cannot allocate cli2key compatibility proxy port: {error}"
            ))
        })
}

fn required_key() -> Result<String, Error> {
    std::env::var("CLI2KEY_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Error::Run(
                "CLI2KEY_BASE_URL is set but CLI2KEY_API_KEY is missing; provide the matching key or unset CLI2KEY_BASE_URL and run `asb tool install cli2key`"
                    .into(),
            )
        })
}

fn normalize_endpoint(value: &str) -> Result<String, Error> {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.contains(['\r', '\n']) {
        return Err(Error::Run(
            "CLI2KEY_BASE_URL must be a non-empty single-line endpoint".into(),
        ));
    }
    Ok(if trimmed.ends_with("/v1") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/v1")
    })
}

#[cfg(test)]
#[path = "../../../../tests/unit/cli2key_lifecycle.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::normalize_endpoint;

    #[cfg(unix)]
    #[test]
    fn occupied_automatic_bridge_port_retries_without_using_foreign_listener() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("bridge");
        std::fs::write(&script, "#!/usr/bin/python3\nimport os,socket,time\ns=socket.socket()\ns.bind((os.environ['CODEX_BRIDGE_HOST'],int(os.environ['CODEX_BRIDGE_PORT'])))\ns.listen()\nprint('Codex Bridge API key: owned-fixture',flush=True)\ntime.sleep(30)\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let foreign = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let foreign_port = foreign.local_addr().unwrap().port();
        let mut calls = 0;
        let (mut resources, key, selected) =
            super::start_bridge_with(script.to_str().unwrap(), "127.0.0.1", None, || {
                calls += 1;
                if calls == 1 {
                    Ok(foreign_port)
                } else {
                    super::free_port()
                }
            })
            .unwrap();
        assert_eq!(calls, 2);
        assert_ne!(selected, foreign_port);
        assert_eq!(key, "owned-fixture");
        foreign.set_nonblocking(true).unwrap();
        assert_eq!(
            foreign.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        resources.finish().unwrap();
        let mut attempts = 0;
        let failure = super::start_bridge_with(script.to_str().unwrap(), "127.0.0.1", None, || {
            attempts += 1;
            Ok(foreign_port)
        })
        .err()
        .unwrap();
        assert_eq!(attempts, 3);
        assert!(failure.to_string().contains("port") && failure.to_string().contains("in use"));
        let failure = super::start_bridge_with(
            script.to_str().unwrap(),
            "127.0.0.1",
            Some(foreign_port),
            || panic!("explicit port must not allocate another port"),
        )
        .err()
        .unwrap();
        assert!(failure.to_string().contains("in use"));
        drop(foreign);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_proxy_modules_are_private_and_cleanup_is_owned() {
        use std::sync::{Arc, Barrier};
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let files = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let root = &root;
                    scope.spawn(move || {
                        barrier.wait();
                        super::RuntimeFiles::create(root).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_ne!(files[0].path, files[1].path);
        let first = files[0].path.join("network-fetch.mjs");
        let second = files[1].path.join("network-fetch.mjs");
        let original = std::fs::read(&first).unwrap();
        std::fs::write(&second, "replaced").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), original);
        let mut iter = files.into_iter();
        let mut released = iter.next().unwrap();
        let mut retained = iter.next().unwrap();
        retained.retain = true;
        released.retain = false;
        drop(released);
        assert!(!first.exists());
        drop(retained);
        assert!(
            second.exists(),
            "uncertain cleanup must retain runtime files"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn endpoint_normalization_targets_openai_compatible_api() {
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:3456").unwrap(),
            "http://127.0.0.1:3456/v1"
        );
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:3456/v1/").unwrap(),
            "http://127.0.0.1:3456/v1"
        );
        assert!(normalize_endpoint("\n").is_err());
    }
}
