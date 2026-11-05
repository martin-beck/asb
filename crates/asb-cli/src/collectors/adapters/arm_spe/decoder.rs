use super::{
    MIB,
    quality::Quality,
    samples::{self, Decoded},
};
use crate::measurements::{self, Session};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn decode(session: &Session, raw: &Path, quality: &Quality) -> Result<Decoded, String> {
    let executable = measurements::resolve_executable(session.tool_dir.as_deref(), "perf");
    let executable = crate::tools::host_executable(&executable)
        .ok_or("SPE offline decoder is unavailable; install the matching perf tool")?;
    let mut launch = crate::launch::ProcessLaunch::new(executable);
    launch.arg("script").arg("-i").arg(raw);
    for arg in [
        "-F",
        "pid,tid,time,event,weight",
        "--show-task-events",
        "--show-lost-events",
    ] {
        launch.arg(arg);
    }
    let (mut command, route) = measurements::producer_command(&mut launch)?;
    super::super::perf::configure_offline_decoder_environment(&mut command, &session.dir);
    let mut captures = [
        crate::evidence::Capture::create(&session.dir.join("arm-spe-decoded.stdout"))?,
        crate::evidence::Capture::create(&session.dir.join("arm-spe-decoded.stderr"))?,
    ];
    let mut producer = measurements::Producer::spawn_with_dependency(
        &mut command,
        route,
        measurements::ProducerPolicy {
            stream_bytes: 64 * MIB,
            graceful: None,
        },
        [captures[0].writer()?, captures[1].writer()?],
        None,
    )?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let completion = loop {
        if let Some(completion) = producer.poll()? {
            break completion;
        }
        if Instant::now() >= deadline {
            let completion = producer.stop();
            return Err(format!(
                "SPE offline decoding timed out; cleanup confirmed {:?}; raw and decoder captures retained",
                completion.guardian_cleanup_confirmed
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if !completion.capture_complete
        || completion.guardian_cleanup_confirmed != Some(true)
        || !completion.exit.is_some_and(|s| s.success())
    {
        return Err(
            "SPE offline decoding failed or exceeded its capture bounds; evidence retained".into(),
        );
    }
    for capture in &mut captures {
        capture.freeze()?;
    }
    // A successful exit with a warning/error stream is not sufficient decoder
    // evidence. Preserve it for inspection instead of silently accepting it.
    use std::io::Read;
    let mut stderr = String::new();
    captures[1]
        .reader()?
        .take(4097)
        .read_to_string(&mut stderr)
        .map_err(|e| e.to_string())?;
    if !stderr.trim().is_empty() {
        return Err("SPE offline decoder reported diagnostics; inspect arm-spe-decoded.stderr before trusting measurements".into());
    }
    let decoded = samples::parse(captures[0].reader()?, quality)?;
    for capture in &captures {
        capture.consume()?;
    }
    Ok(decoded)
}
