//! Adapter for openai.
use super::*;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_BASE_URL_BYTES: u64 = 4096;

fn configured_file() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".api_base_url_openai"))
}

fn file_endpoint(path: &Path) -> Result<Option<String>, crate::Error> {
    let invalid = || {
        crate::Error::Input("invalid OpenAI base URL file ~/.api_base_url_openai; provide one HTTP(S) URL without credentials, query or fragment, or set OPENAI_BASE_URL".into())
    };
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(invalid()),
        Ok(_) => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| invalid())?;
    if !file.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BASE_URL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() as u64 > MAX_BASE_URL_BYTES {
        return Err(invalid());
    }
    let value = std::str::from_utf8(&bytes).map_err(|_| invalid())?.trim();
    if value.is_empty() || value.contains(['\r', '\n']) || value.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let parsed = url::Url::parse(value).map_err(|_| invalid())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.port() == Some(0)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(Some(value.to_owned()))
}

fixed_offers!(
    openai_offers,
    Some("OPENAI_BASE_URL"),
    "https://api.openai.com/v1",
    "",
    false,
    &[(Responses, Bearer), (Chat, Bearer)]
);

adapter!(
    "openai",
    Some("OPENAI_API_KEY"),
    Some("OPENAI_BASE_URL"),
    Some("OPENAI_MODEL"),
    None,
    &["OPENAI_BASE_URL", "OPENAI_API_BASE"],
    Some("OPENAI_BASE_URL"),
    Some("https://api.openai.com/v1/models"),
    DataUnpaginated,
    "gpt-4o-mini",
    openai_offers,
    fn endpoint_override(&self) -> Result<Option<String>, crate::Error> {
        match configured_file() {
            Some(path) => file_endpoint(&path),
            None => Ok(None),
        }
    },
    fn catalog_environment_is_base(&self) -> bool {
        true
    },
    fn authoritative_tape_upstream(&self) -> Result<Option<String>, crate::Error> {
        let base = match std::env::var_os("OPENAI_BASE_URL") {
            Some(value) => Some(value.to_string_lossy().into_owned()),
            None => self
                .endpoint_override()?
                .or_else(|| Some("https://api.openai.com/v1".to_owned())),
        };
        Ok(base)
    },
    fn strip_proxy_version_for_recording(&self, base: &str) -> bool {
        !base.trim_end_matches('/').ends_with("/v1")
    },
    fn runtime_inputs(
        &self,
        _context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
        let mut inputs = crate::providers::credential_inputs(&self.spec());
        inputs.push(crate::runtime_inputs::Dependency::program("curl"));
        if std::env::var_os("OPENAI_BASE_URL").is_none()
            && let Some(path) = configured_file()
        {
            inputs.push(crate::runtime_inputs::Dependency::file(path));
        }
        Ok(inputs)
    }
);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_inputs_track_the_file_only_when_used() {
        const FLAG: &str = "ASB_TEST_OPENAI_BASE_RUNTIME_CHILD";
        if let Ok(mode) = std::env::var(FLAG) {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            let project = crate::project::ProjectConfig::default();
            let context = crate::runtime_inputs::Context {
                root: &home,
                invocation_dir: &home,
                project: &project,
            };
            let inputs = ADAPTER.runtime_inputs(&context).unwrap();
            assert_eq!(
                inputs.contains(&crate::runtime_inputs::Dependency::file(
                    home.join(".api_base_url_openai")
                )),
                mode == "file"
            );
            return;
        }
        let home = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&home).unwrap();
        for mode in ["file", "environment"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "providers::adapters::openai::tests::runtime_inputs_track_the_file_only_when_used", "--nocapture"])
                .env(FLAG, mode).env("HOME", &home).env_remove("OPENAI_BASE_URL");
            if mode == "environment" {
                child.env("OPENAI_BASE_URL", "http://127.0.0.1:1234/v1");
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn base_url_file_is_bounded_single_http_url_without_embedded_credentials() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("base");
        assert_eq!(file_endpoint(&path).unwrap(), None);
        std::fs::write(&path, "  http://127.0.0.1:9876/api/v1\n").unwrap();
        assert_eq!(
            file_endpoint(&path).unwrap().as_deref(),
            Some("http://127.0.0.1:9876/api/v1")
        );
        for value in [
            "",
            "ftp://example.com",
            "http://",
            "http://localhost:0/v1",
            "https://example.com\nhttps://other",
            "https://user:pass@example.com/v1",
            "https://example.com/v1?key=secret",
            "https://example.com/#fragment",
        ] {
            std::fs::write(&path, value).unwrap();
            let error = file_endpoint(&path).unwrap_err().to_string();
            assert!(error.contains(".api_base_url_openai"));
            assert!(!error.contains("secret"));
        }
        std::fs::write(&path, vec![b'x'; MAX_BASE_URL_BYTES as usize + 1]).unwrap();
        assert!(file_endpoint(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            std::fs::write(&path, "https://example.com/v1").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::File::open(&path).is_err() {
                assert!(file_endpoint(&path).is_err());
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::remove_file(&path).unwrap();
            symlink(root.join("missing-target"), &path).unwrap();
            assert!(file_endpoint(&path).is_err());
            std::fs::remove_file(&path).unwrap();
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(file_endpoint(&path).is_err());
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::create_dir(&path).unwrap();
        assert!(file_endpoint(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
