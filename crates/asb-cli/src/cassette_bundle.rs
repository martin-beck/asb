//! Portable, bounded sweep replay bundles.
//!
//! This format intentionally wraps existing immutable recording archives rather
//! than extending their strict cassette ZIP contract. A bundle holds the exact
//! source sweep and benchmark TOMLs plus the selected cassette archive for each
//! configuration. It contains no credentials and is not a workspace snapshot.

use crate::benchmark::BenchmarkConfig;
use crate::benchmark_paths::benchmark_stem;
use crate::{Error, install, project, sweep, tape};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

const SCHEMA_VERSION: u32 = 1;
const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = crate::sweep::MAX_CONFIGS * 2 + 2;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    kind: String,
    sweep: Entry,
    benchmarks: Vec<Benchmark>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Benchmark {
    config: Entry,
    cassette: Entry,
}

/// Create one non-overwriting replay bundle for a recorded sweep.
pub(crate) fn export_sweep_at(
    base: &Path,
    requested: &Path,
    cassette_root: Option<&Path>,
    output: Option<&Path>,
    writer: &mut dyn Write,
) -> Result<(), Error> {
    let sweep_path = resolve(base, requested);
    let sweep = sweep::read_manifest(&sweep_path)?;
    let parent = sweep_path.parent().unwrap_or(base);
    let source_root = cassette_root.map(|path| resolve(base, path));
    let mut benchmarks = Vec::with_capacity(sweep.configs.len());
    let mut sources = Vec::with_capacity(sweep.configs.len());
    for (index, reference) in sweep.configs.iter().enumerate() {
        let config_path = crate::benchmark_paths::checked_benchmark_reference(
            parent,
            reference,
            &sweep_path.display().to_string(),
        )?;
        let source_entry = sweep_entry_name(index, Path::new(reference));
        let staged_entry = sweep_entry_name(index, Path::new(&format!("{index:06}.toml")));
        let cassette = match &source_root {
            Some(root) => cassette_under(root, &source_entry)?,
            None => discover_default_cassette(base, &source_entry)?,
        };
        // Validate the sealed inner cassette before publishing it in a portable
        // wrapper. The replay side validates it again through the normal path.
        crate::cassette::Archive::open(&cassette, crate::cassette::Limits::default()).map_err(
            |error| Error::Input(format!("cannot export {}: {error}", cassette.display())),
        )?;
        let config = describe_file(&config_path, crate::benchmark::MAX_CONFIG_BYTES as u64)?;
        let cassette_entry = describe_file(&cassette, MAX_BUNDLE_BYTES)?;
        benchmarks.push(Benchmark {
            config: Entry {
                path: format!("configs/{index:06}.toml"),
                ..config
            },
            cassette: Entry {
                path: format!("cassettes/{staged_entry}/recording.asb.zip"),
                ..cassette_entry
            },
        });
        sources.push((config_path, cassette));
    }
    let sweep_entry = Entry {
        path: "sweep.toml".into(),
        ..describe_file(&sweep_path, sweep::MAX_SWEEP_BYTES)?
    };
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        kind: "asb-portable-sweep-replay".into(),
        sweep: sweep_entry,
        benchmarks,
    };
    validate_manifest(&manifest)?;
    let destination =
        output.map_or_else(|| default_output(&sweep_path), |path| resolve(base, path));
    write_bundle(&destination, &manifest, &sweep_path, &sources)?;
    writeln!(
        writer,
        "Portable sweep replay bundle: {}\nContains {} benchmark TOMLs and their sealed cassettes. Treat it as private recording evidence.",
        destination.display(),
        manifest.benchmarks.len()
    )
    .map_err(crate::io_error)
}

/// Materialize a portable bundle inside an initialized project, prepare the
/// same project-local inputs as a normal run, then execute regular replay.
pub(crate) fn replay_at(
    base: &Path,
    requested: &Path,
    respect_record_timings: bool,
    show_output: bool,
    interactive: bool,
    writer: &mut dyn Write,
) -> Result<(), Error> {
    let bundle = resolve(base, requested);
    let (manifest, mut archive) = open_bundle(&bundle)?;
    let (root, _) = project::require_initialized(base)?;
    let digest = digest_file(&bundle, MAX_BUNDLE_BYTES)?.1;
    let stage = allocate_stage(&root, &digest)?;
    materialize(&manifest, &mut archive, &stage)?;
    let replay_sweep = stage.join("replay.toml");
    let original = sweep::read_manifest_text(&stage.join("sweep.toml"))?;
    let mut sweep: sweep::SweepFile = toml::from_str(&original)
        .map_err(|error| Error::Input(format!("invalid bundled sweep TOML: {error}")))?;
    sweep.configs = (0..manifest.benchmarks.len())
        .map(|index| format!("configs/{index:06}.toml"))
        .collect();
    let text = toml::to_string(&sweep)
        .map_err(|error| Error::Io(format!("cannot encode staged replay sweep: {error}")))?;
    write_new(&replay_sweep, text.as_bytes())?;
    let _ = sweep::read_manifest(&replay_sweep)?;

    for index in 0..manifest.benchmarks.len() {
        let path = stage.join(format!("configs/{index:06}.toml"));
        let text = crate::read_config(&path)?;
        let config: BenchmarkConfig = toml::from_str(&text).map_err(|error| {
            Error::Input(format!(
                "invalid bundled benchmark {}: {error}",
                path.display()
            ))
        })?;
        // The normal preparation route installs only project-local inputs. In
        // replay mode it deliberately does not install a live provider bridge.
        install::ensure_execution_requirements_with_policy(
            &config,
            &path,
            Some(tape::Mode::Replay),
            base,
            writer,
            false,
            false,
        )?;
    }
    writeln!(writer, "Replaying portable bundle from {}", stage.display())
        .map_err(crate::io_error)?;
    sweep::run_with_options(
        replay_sweep.to_string_lossy().as_ref(),
        sweep::Options {
            invocation_dir: Some(base),
            mode: Some(tape::Mode::Replay),
            tool_mode: Some(tape::Mode::Replay),
            respect_record_timings,
            show_output,
            interactive,
            cassette_root: Some(&stage.join("cassettes")),
            ..Default::default()
        },
        writer,
    )
}

fn resolve(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    }
}

fn default_output(sweep: &Path) -> PathBuf {
    let stem = sweep
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("sweep");
    sweep
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.asb-sweep.zip"))
}

fn sweep_entry_name(index: usize, reference: &Path) -> String {
    format!("{index:03}-{}", benchmark_stem(reference))
}

fn cassette_under(root: &Path, entry: &str) -> Result<PathBuf, Error> {
    let candidates = [
        root.join(entry).join("recording.asb.zip"),
        root.join(entry).join("cassettes/recording.asb.zip"),
    ];
    let found: Vec<_> = candidates
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    match found.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(Error::Input(format!(
            "no recorded cassette for sweep entry `{entry}` below {}; rerun export with --cassette-root pointing to the recording root",
            root.display()
        ))),
        _ => Err(Error::Input(format!(
            "multiple cassette layouts exist for sweep entry `{entry}` below {}; choose a clean cassette root",
            root.display()
        ))),
    }
}

fn discover_default_cassette(base: &Path, entry: &str) -> Result<PathBuf, Error> {
    let root = base.join("cassettes");
    let entries = fs::read_dir(&root).map_err(|_| {
        Error::Input(format!(
            "cannot find a recorded sweep cassette for `{entry}` below {}; pass --cassette-root",
            root.display()
        ))
    })?;
    let mut found = Vec::new();
    for item in entries.take(crate::sweep::MAX_CONFIGS) {
        let item = item.map_err(crate::io_error)?;
        let name = item.file_name();
        if !name.to_string_lossy().starts_with("sweep-") || !item.path().is_dir() {
            continue;
        }
        if let Ok(path) = cassette_under(&item.path(), entry) {
            found.push(path);
        }
    }
    match found.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(Error::Input(format!(
            "cannot find a recorded sweep cassette for `{entry}`; pass --cassette-root"
        ))),
        _ => Err(Error::Input(format!(
            "multiple recorded sweeps contain `{entry}`; pass --cassette-root to choose one"
        ))),
    }
}

fn describe_file(path: &Path, limit: u64) -> Result<Entry, Error> {
    let (bytes, sha256) = digest_file(path, limit)?;
    Ok(Entry {
        path: String::new(),
        bytes,
        sha256,
    })
}

fn digest_file(path: &Path, limit: u64) -> Result<(u64, String), Error> {
    let mut file = File::open(path)
        .map_err(|error| Error::Input(format!("cannot read {}: {error}", path.display())))?;
    let metadata = file.metadata().map_err(crate::io_error)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error::Input(format!(
            "{} is not a regular file within the portable-bundle limit",
            path.display()
        )));
    }
    let mut bytes = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(crate::io_error)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .filter(|value| *value <= limit)
            .ok_or_else(|| Error::Input("portable bundle input exceeds its size limit".into()))?;
        hash.update(&buffer[..read]);
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

fn write_bundle(
    destination: &Path,
    manifest: &Manifest,
    sweep: &Path,
    sources: &[(PathBuf, PathBuf)],
) -> Result<(), Error> {
    if destination.exists() {
        return Err(Error::Input(format!(
            "portable bundle {} already exists; refusing to replace it",
            destination.display()
        )));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Io("bundle output has no parent".into()))?;
    fs::create_dir_all(parent).map_err(crate::io_error)?;
    let temporary = parent.join(format!(
        ".{}.partial-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("bundle"),
        std::process::id()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            Error::Io(format!(
                "cannot create bundle staging file {}: {error}",
                temporary.display()
            ))
        })?;
    let result = (|| {
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        archive
            .start_file("manifest.json", options)
            .map_err(zip_error)?;
        archive
            .write_all(&serde_json::to_vec(manifest).map_err(|error| Error::Io(error.to_string()))?)
            .map_err(crate::io_error)?;
        copy_entry(&mut archive, &manifest.sweep, sweep, options)?;
        for ((config, cassette), entry) in sources.iter().zip(&manifest.benchmarks) {
            copy_entry(&mut archive, &entry.config, config, options)?;
            copy_entry(&mut archive, &entry.cassette, cassette, options)?;
        }
        archive.finish().map_err(zip_error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return result;
    }
    fs::rename(&temporary, destination).map_err(crate::io_error)
}

fn copy_entry(
    archive: &mut ZipWriter<File>,
    entry: &Entry,
    source: &Path,
    options: SimpleFileOptions,
) -> Result<(), Error> {
    archive
        .start_file(&entry.path, options)
        .map_err(zip_error)?;
    let mut input = File::open(source).map_err(crate::io_error)?;
    let mut copied = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(crate::io_error)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .filter(|value| *value <= entry.bytes)
            .ok_or_else(|| Error::Io("bundle entry length overflow".into()))?;
        hash.update(&buffer[..read]);
        archive
            .write_all(&buffer[..read])
            .map_err(crate::io_error)?;
    }
    if copied != entry.bytes || format!("{:x}", hash.finalize()) != entry.sha256 {
        return Err(Error::Input(format!(
            "bundle input {} changed while exporting; no bundle was published",
            source.display()
        )));
    }
    Ok(())
}

fn open_bundle(path: &Path) -> Result<(Manifest, ZipArchive<File>), Error> {
    let file = File::open(path).map_err(|error| {
        Error::Input(format!(
            "cannot open portable bundle {}: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(crate::io_error)?;
    if !metadata.is_file() || metadata.len() > MAX_BUNDLE_BYTES {
        return Err(Error::Input(
            "portable replay bundle must be a regular file".into(),
        ));
    }
    let mut archive = ZipArchive::new(file).map_err(zip_error)?;
    if archive.is_empty() || archive.len() > MAX_ENTRIES {
        return Err(Error::Input(
            "portable replay bundle has an invalid entry count".into(),
        ));
    }
    let manifest_bytes = read_zip_entry(&mut archive, "manifest.json", 1024 * 1024)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| Error::Input(format!("invalid portable bundle manifest: {error}")))?;
    validate_manifest(&manifest)?;
    let expected = expected_paths(&manifest);
    let mut actual = BTreeSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(zip_error)?;
        let name = entry.name().map_err(zip_error)?.into_owned();
        if entry.encrypted()
            || entry.compression() != CompressionMethod::Deflated
            || !actual.insert(name)
        {
            return Err(Error::Input(
                "portable replay bundle has unsafe or duplicate ZIP entries".into(),
            ));
        }
    }
    if actual != expected {
        return Err(Error::Input(
            "portable replay bundle manifest and ZIP entries differ".into(),
        ));
    }
    Ok((manifest, archive))
}

fn validate_manifest(manifest: &Manifest) -> Result<(), Error> {
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.kind != "asb-portable-sweep-replay"
        || manifest.benchmarks.is_empty()
        || manifest.benchmarks.len() > crate::sweep::MAX_CONFIGS
    {
        return Err(Error::Input(
            "unsupported or invalid portable replay bundle manifest".into(),
        ));
    }
    let mut paths = BTreeSet::new();
    for entry in std::iter::once(&manifest.sweep).chain(
        manifest
            .benchmarks
            .iter()
            .flat_map(|benchmark| [&benchmark.config, &benchmark.cassette]),
    ) {
        if !safe_name(&entry.path)
            || entry.bytes == 0
            || entry.bytes > MAX_BUNDLE_BYTES
            || !crate::cassette::valid_digest(&entry.sha256)
            || !paths.insert(entry.path.clone())
        {
            return Err(Error::Input(
                "invalid portable bundle entry declaration".into(),
            ));
        }
    }
    if manifest.sweep.path != "sweep.toml"
        || manifest.sweep.bytes > sweep::MAX_SWEEP_BYTES
        || manifest
            .benchmarks
            .iter()
            .enumerate()
            .any(|(index, benchmark)| {
                benchmark.config.path != format!("configs/{index:06}.toml")
                    || benchmark.config.bytes > crate::benchmark::MAX_CONFIG_BYTES as u64
                    || benchmark.cassette.path
                        != format!(
                            "cassettes/{}/recording.asb.zip",
                            sweep_entry_name(index, Path::new(&format!("{index:06}.toml")))
                        )
            })
    {
        return Err(Error::Input(
            "portable bundle has noncanonical entry paths".into(),
        ));
    }
    Ok(())
}

fn expected_paths(manifest: &Manifest) -> BTreeSet<String> {
    std::iter::once("manifest.json".to_owned())
        .chain(std::iter::once(manifest.sweep.path.clone()))
        .chain(manifest.benchmarks.iter().flat_map(|benchmark| {
            [
                benchmark.config.path.clone(),
                benchmark.cassette.path.clone(),
            ]
        }))
        .collect()
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains("//")
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn read_zip_entry(
    archive: &mut ZipArchive<File>,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, Error> {
    let entry = archive
        .by_name(name)
        .map_err(|_| Error::Input(format!("portable replay bundle is missing `{name}`")))?;
    if entry.encrypted() || entry.size() > limit || entry.compressed_size() > MAX_BUNDLE_BYTES {
        return Err(Error::Input(format!(
            "portable replay bundle entry `{name}` is unsafe or oversized"
        )));
    }
    let expected_size = entry.size();
    let mut bytes = Vec::with_capacity(expected_size as usize);
    entry
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(crate::io_error)?;
    if bytes.len() as u64 != expected_size || bytes.len() as u64 > limit {
        return Err(Error::Input(format!(
            "portable replay bundle entry `{name}` exceeds its declared size"
        )));
    }
    Ok(bytes)
}

fn allocate_stage(root: &Path, digest: &str) -> Result<PathBuf, Error> {
    let parent = root.join(".asb/cassette-replays");
    fs::create_dir_all(&parent).map_err(crate::io_error)?;
    for index in 1..=9999usize {
        let path = parent.join(format!("{}-{index:04}", &digest[..16]));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(crate::io_error(error)),
        }
    }
    Err(Error::Io(
        "cannot allocate a portable replay staging directory".into(),
    ))
}

fn materialize(
    manifest: &Manifest,
    archive: &mut ZipArchive<File>,
    stage: &Path,
) -> Result<(), Error> {
    copy_zip_entry(archive, &manifest.sweep, &stage.join("sweep.toml"))?;
    for benchmark in &manifest.benchmarks {
        copy_zip_entry(
            archive,
            &benchmark.config,
            &stage.join(&benchmark.config.path),
        )?;
        let cassette = stage.join(&benchmark.cassette.path);
        copy_zip_entry(archive, &benchmark.cassette, &cassette)?;
        crate::cassette::Archive::open(&cassette, crate::cassette::Limits::default()).map_err(
            |error| {
                Error::Input(format!(
                    "invalid bundled cassette {}: {error}",
                    cassette.display()
                ))
            },
        )?;
    }
    Ok(())
}

fn copy_zip_entry(
    archive: &mut ZipArchive<File>,
    entry: &Entry,
    destination: &Path,
) -> Result<(), Error> {
    let mut source = archive.by_name(&entry.path).map_err(|_| {
        Error::Input(format!(
            "portable replay bundle is missing `{}`",
            entry.path
        ))
    })?;
    if source.encrypted()
        || source.compression() != CompressionMethod::Deflated
        || source.size() != entry.bytes
        || source.compressed_size() > MAX_BUNDLE_BYTES
    {
        return Err(Error::Input(format!(
            "portable replay bundle entry `{}` is unsafe or changed",
            entry.path
        )));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Io("output path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(crate::io_error)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", destination.display())))?;
    let mut copied = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer).map_err(crate::io_error)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .filter(|value| *value <= entry.bytes)
            .ok_or_else(|| {
                Error::Input("portable replay bundle entry exceeds its declared size".into())
            })?;
        hash.update(&buffer[..read]);
        output.write_all(&buffer[..read]).map_err(crate::io_error)?;
    }
    if copied != entry.bytes || format!("{:x}", hash.finalize()) != entry.sha256 {
        let _ = fs::remove_file(destination);
        return Err(Error::Input(format!(
            "portable replay bundle entry `{}` has a checksum mismatch",
            entry.path
        )));
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Io("output path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(crate::io_error)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", path.display())))?;
    file.write_all(bytes).map_err(crate::io_error)
}

fn zip_error(error: zip::result::ZipError) -> Error {
    Error::Input(format!("invalid portable replay ZIP: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassette::{Compatibility, Manifest as CassetteManifest, Statistics, Track};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn portable_manifest_rejects_noncanonical_paths_and_duplicate_entries() {
        let mut manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            kind: "asb-portable-sweep-replay".into(),
            sweep: Entry {
                path: "sweep.toml".into(),
                bytes: 1,
                sha256: "a".repeat(64),
            },
            benchmarks: vec![Benchmark {
                config: Entry {
                    path: "configs/000000.toml".into(),
                    bytes: 1,
                    sha256: "b".repeat(64),
                },
                cassette: Entry {
                    path: "cassettes/000-000000/recording.asb.zip".into(),
                    bytes: 1,
                    sha256: "c".repeat(64),
                },
            }],
        };
        assert!(validate_manifest(&manifest).is_ok());
        manifest.benchmarks[0].config.path = "../config.toml".into();
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn bundle_paths_are_unique_and_stable() {
        assert_eq!(
            sweep_entry_name(7, Path::new("example.toml")),
            "007-example"
        );
        assert!(safe_name("cassettes/000-example/recording.asb.zip"));
        assert!(!safe_name("cassettes/../recording.asb.zip"));
    }

    #[test]
    fn export_wraps_the_original_sweep_configurations_and_cassettes() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("asb-portable-bundle-{stamp}"));
        fs::create_dir_all(root.join("tapes/000-demo")).unwrap();
        fs::write(
            root.join("demo.toml"),
            "agent='pi'\nworkload='bug-fix'\nprovider='fixture'\nmodel='fixture'\n",
        )
        .unwrap();
        fs::write(root.join("sweep.toml"), "configs=['demo.toml']\n").unwrap();
        let cassette = root.join("tapes/000-demo/recording.asb.zip");
        let manifest = CassetteManifest {
            schema_version: crate::cassette::SCHEMA_VERSION,
            recording_id: "fixture-recording".into(),
            compatibility: Compatibility {
                agent: "pi".into(),
                provider: "fixture".into(),
                model: "fixture".into(),
                workload: "bug-fix".into(),
                task: "default".into(),
                dataset_sha256: None,
                prompt_sha256: crate::cassette::digest(b"prompt"),
                material_sha256: crate::cassette::digest(b"material"),
                protocol_sha256: None,
                normalization_sha256: None,
            },
            concurrency: 1,
            tracks: vec![Track {
                recorder: None,
                recorded_max_steps: None,
                id: 1,
                batch: 1,
                replica: 1,
                complete: true,
                process_success: Some(true),
                workload_success: Some(true),
                bytes: 3,
                sha256: crate::cassette::digest(b"{}\n"),
                statistics: Statistics::default(),
                native: None,
                artifacts: Vec::new(),
            }],
            provenance: None,
        };
        let mut writer = ZipWriter::new(File::create(&cassette).unwrap());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file("manifest.json", options).unwrap();
        writer
            .write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        writer.start_file("tracks/1.jsonl", options).unwrap();
        writer.write_all(b"{}\n").unwrap();
        writer.finish().unwrap();

        let mut output = Vec::new();
        export_sweep_at(
            &root,
            Path::new("sweep.toml"),
            Some(Path::new("tapes")),
            Some(Path::new("bundle.zip")),
            &mut output,
        )
        .unwrap();
        let (bundle, _) = open_bundle(&root.join("bundle.zip")).unwrap();
        assert_eq!(bundle.sweep.path, "sweep.toml");
        assert_eq!(bundle.benchmarks.len(), 1);
        assert_eq!(bundle.benchmarks[0].config.path, "configs/000000.toml");
        assert_eq!(
            bundle.benchmarks[0].cassette.path,
            "cassettes/000-000000/recording.asb.zip"
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Portable sweep replay bundle")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
