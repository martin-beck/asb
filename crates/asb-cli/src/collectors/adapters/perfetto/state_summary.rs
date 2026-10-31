//! Full observed thread-time aggregates, independent of the bounded SVG view.

use super::*;

pub(super) struct Summary {
    pub start: u64,
    pub end: u64,
    pub intervals: u64,
    pub threads: u64,
    pub unfinished: u64,
}

pub(super) fn scope(pid: u32, trace_id: &str) -> String {
    scoped_scope(pid, trace_id, None)
}

/// Scope used only by the rendered timeline.  The SVG deliberately presents
/// at most 64 lanes, so constrain the source rows before materializing the
/// potentially very large thread-state relation.  The full state summary
/// continues to use [`scope`] and therefore accounts for every observed
/// thread.
pub(super) fn timeline_scope(pid: u32, trace_id: &str) -> String {
    scoped_scope(pid, trace_id, Some(64))
}

fn scoped_scope(pid: u32, trace_id: &str, thread_limit: Option<usize>) -> String {
    // Prevent SQLite from repeatedly expanding the recursive process census
    // for each scheduler-state row on fork-heavy workloads.
    let scope = perfetto_scope_sql(pid, trace_id)
        .replace("tree(upid) AS (", "tree(upid) AS MATERIALIZED (")
        .replace(", bounds AS (", ", bounds AS MATERIALIZED (");
    let threads = match thread_limit {
        Some(limit) => {
            format!("SELECT utid,tid FROM thread WHERE upid IN tree ORDER BY utid LIMIT {limit}")
        }
        None => "SELECT utid,tid FROM thread WHERE upid IN tree".into(),
    };
    format!(
        "{scope}, scoped_threads AS MATERIALIZED ({threads}), clipped AS MATERIALIZED (SELECT th.utid,th.tid,MAX(st.ts,b.start_ts) AS ts,MIN(st.ts+st.dur,b.end_ts)-MAX(st.ts,b.start_ts) AS dur,st.state,st.io_wait FROM scoped_threads th JOIN thread_state st ON st.utid=th.utid CROSS JOIN bounds b WHERE st.dur>0 AND st.ts<b.end_ts AND st.ts+st.dur>b.start_ts)"
    )
}

pub(super) fn query(pid: u32, trace_id: &str) -> String {
    format!(
        "{} SELECT 'bounds' AS kind,'' AS state,-1 AS io_wait,0 AS duration_ns,(SELECT COUNT(*) FROM clipped) AS intervals,(SELECT COUNT(DISTINCT utid) FROM clipped) AS threads,b.start_ts,b.end_ts,(SELECT COUNT(*) FROM thread_state st JOIN scoped_threads th ON st.utid=th.utid WHERE st.dur=-1 AND st.ts<b.end_ts) AS unfinished FROM bounds b WHERE {VALID_SCOPE} UNION ALL SELECT 'state',state,COALESCE(io_wait,-1),SUM(dur),COUNT(*),0,0,0,0 FROM clipped GROUP BY state,io_wait",
        scope(pid, trace_id)
    )
}

pub(super) fn write(
    dir: &Path,
    processor: &Path,
    trace: &Path,
    pid: u32,
    trace_id: &str,
) -> Result<Summary, String> {
    let output = processor_query(processor, trace, &query(pid, trace_id))
        .map_err(|error| format!("cannot aggregate Perfetto scheduler states: {error}"))?;
    if !output.status.success() {
        return Err("cannot aggregate complete Perfetto scheduler states".into());
    }
    let (summary, metrics) = parse(&bounded(&output.stdout))?;
    measurements::write_json(
        &dir.join("perfetto-state-summary.json"),
        &serde_json::json!({
            "schema_version": 2, "collector": "perfetto-state-summary",
            "semantics": "Observed process-tree thread-time, not additive elapsed wall time",
            "interval_policy": "All positive-duration states overlapping execution, clipped at both bounds; unfinished dur=-1 excluded, never extended by guessing",
            "observed_intervals": summary.intervals,
            "observed_threads": summary.threads,
            "unfinished_intervals_excluded": summary.unfinished,
            "execution_start_ns": summary.start, "execution_end_ns": summary.end,
            "metrics": metrics,
        }),
    )?;
    Ok(summary)
}

fn parse(output: &str) -> Result<(Summary, BTreeMap<String, serde_json::Value>), String> {
    let mut summary = None;
    let mut metrics = BTreeMap::<String, serde_json::Value>::new();
    let mut unknown_io = false;
    let mut counted = 0u64;
    for line in output.lines() {
        let fields: Vec<_> = line
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .collect();
        if fields.first() == Some(&"kind") {
            continue;
        }
        if fields.len() != 9 {
            return Err("invalid Perfetto state aggregate row".into());
        }
        let number = |index: usize| {
            fields[index]
                .parse::<u64>()
                .map_err(|_| "invalid Perfetto state aggregate number".to_owned())
        };
        match fields[0] {
            "bounds" if summary.is_none() => {
                let (start, end) = (number(6)?, number(7)?);
                if end <= start {
                    return Err("invalid Perfetto state execution bounds".into());
                }
                summary = Some(Summary {
                    start,
                    end,
                    intervals: number(4)?,
                    threads: number(5)?,
                    unfinished: number(8)?,
                });
            }
            "state" => {
                let io = match fields[2] {
                    "0" => Some(false),
                    "1" => Some(true),
                    "-1" => None,
                    _ => return Err("invalid Perfetto I/O-wait classification".into()),
                };
                let (label, _) = state_style(fields[1], io);
                let key = match label {
                    "running" => "running_ms",
                    "runnable" => "runnable_ms",
                    "I/O wait" => "io_wait_observed_ms",
                    "uninterruptible sleep" => "uninterruptible_sleep_ms",
                    "interruptible sleep" => "interruptible_sleep_ms",
                    _ => "other_scheduler_state_ms",
                };
                let value = number(3)? as f64 / 1_000_000.0;
                let previous = metrics
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                metrics.insert(key.into(), serde_json::json!(previous + value));
                counted = counted
                    .checked_add(number(4)?)
                    .ok_or("Perfetto state interval count overflow")?;
                if matches!(fields[1], "D" | "Uninterruptible Sleep") && io.is_none() {
                    unknown_io = true;
                }
            }
            _ => return Err("invalid or duplicate Perfetto state aggregate header".into()),
        }
    }
    let summary = summary.ok_or("missing Perfetto state execution bounds")?;
    if counted != summary.intervals {
        return Err("incomplete Perfetto state aggregate".into());
    }
    let observed = metrics
        .get("io_wait_observed_ms")
        .cloned()
        .unwrap_or(serde_json::json!(0.0));
    metrics.insert(
        "io_wait_ms".into(),
        if unknown_io || summary.intervals == 0 {
            serde_json::Value::Null
        } else {
            observed
        },
    );
    Ok((summary, metrics))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an explicitly selected retained native trace and installed processor"]
    fn installed_processor_aggregates_retained_scoped_trace() {
        let tools =
            std::env::var_os("ASB_TEST_PERFETTO_TOOLS").expect("set project tools directory");
        let trace =
            std::env::var_os("ASB_TEST_PERFETTO_SUMMARY_TRACE").expect("set retained native trace");
        let pid = std::env::var("ASB_TEST_PERFETTO_SUMMARY_PID")
            .expect("set recorded root PID")
            .parse()
            .unwrap();
        let identity =
            std::env::var("ASB_TEST_PERFETTO_SUMMARY_ID").expect("set recorded trace identity");
        assert!(!identity.is_empty() && identity.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perfetto-summary-native")
            .join(crate::workloads::TaskIdentity::local("summary", None).attempt_id);
        fs::create_dir_all(&dir).unwrap();
        let summary = write(
            &dir,
            &PathBuf::from(tools).join("perfetto/bin/trace_processor_shell"),
            Path::new(&trace),
            pid,
            &identity,
        )
        .unwrap();
        assert!(summary.intervals > 4096);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("perfetto-state-summary.json")).unwrap())
                .unwrap();
        assert!(json["metrics"]["running_ms"].as_f64().unwrap() > 1000.0);
        assert!(json["metrics"]["io_wait_ms"].is_null());
        println!(
            "{}",
            serde_json::json!({"observed_intervals":summary.intervals,"threads":summary.threads,"metrics":json["metrics"],"output":dir})
        );
    }

    #[test]
    fn aggregate_preserves_running_runnable_unknown_io_and_full_counts() {
        let (summary,metrics)=parse("kind,state,io_wait,duration_ns,intervals,threads,start_ts,end_ts,unfinished\nbounds,,-1,0,5001,70,10,10000,2\nstate,Running,-1,2000000,2000,0,0,0,0\nstate,R,-1,3000000,2000,0,0,0,0\nstate,R+,-1,4000000,999,0,0,0,0\nstate,D,-1,5000000,1,0,0,0,0\nstate,D,1,6000000,1,0,0,0,0\n").unwrap();
        assert_eq!(summary.intervals, 5001);
        assert_eq!(summary.threads, 70);
        assert_eq!(summary.unfinished, 2);
        assert_eq!(metrics["running_ms"], 2.0);
        assert_eq!(metrics["runnable_ms"], 7.0);
        assert_eq!(metrics["uninterruptible_sleep_ms"], 5.0);
        assert_eq!(metrics["io_wait_observed_ms"], 6.0);
        assert!(metrics["io_wait_ms"].is_null());
    }

    #[test]
    fn timeline_scope_bounds_source_threads_but_summary_scope_is_complete() {
        let summary = scope(42, "fixture");
        let timeline = timeline_scope(42, "fixture");
        assert!(!summary.contains("ORDER BY utid LIMIT 64"));
        assert!(timeline.contains("ORDER BY utid LIMIT 64"));
        assert!(timeline.contains("clipped AS MATERIALIZED"));
    }

    #[test]
    fn malformed_or_partial_aggregate_is_not_published() {
        for text in [
            "",
            "bad\n",
            "bounds,,-1,0,1,1,10,10,0\n",
            "bounds,,-1,0,1,1,10,20,0\n",
            "bounds,,-1,0,0,1,10,20,0\nbounds,,-1,0,0,1,10,20,0\n",
            "bounds,,-1,0,1,1,10,20,0\nstate,D,3,1,1,0,0,0,0\n",
            "bounds,,-1,0,1,1,10,20,0\nstate,Running,-1,NaN,1,0,0,0,0\n",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        let (_, metrics) =
            parse("bounds,,-1,0,1,1,10,20,0\nstate,Running,-1,1000000,1,0,0,0,0\n").unwrap();
        assert_eq!(metrics["io_wait_ms"], 0.0);
        assert_eq!(state_style("R", None).0, "runnable");
        assert_eq!(state_style("Running", Some(true)).0, "running");
    }

    #[test]
    fn actual_scheduler_series_sql_clips_partial_buckets_and_rejects_invalid_roots() {
        let script = r#"
import csv,sqlite3,sys
db=sqlite3.connect(':memory:')
db.executescript('''
CREATE TABLE process(upid INTEGER,pid INTEGER,parent_upid INTEGER,start_ts INTEGER,end_ts INTEGER);
CREATE TABLE thread(utid INTEGER,tid INTEGER,upid INTEGER);
CREATE TABLE slice(ts INTEGER,name TEXT);
CREATE TABLE sched(ts INTEGER,dur INTEGER,utid INTEGER);
INSERT INTO process VALUES(1,42,NULL,0,2000000000),(2,43,1,0,2000000000),(3,99,NULL,0,2000000000);
INSERT INTO thread VALUES(1,42,1),(2,43,2),(3,99,3);
INSERT INTO slice VALUES(0,'asb.agent.started.fixture'),(1250000000,'asb.agent.exited.fixture');
INSERT INTO sched VALUES(-50000000,100000000,1),(900000000,300000000,1),(1100000000,400000000,2),(0,2000000000,3),(1250000000,100000000,1),(-100000000,100000000,1);
''')
if sys.argv[2]=='absent': db.execute('DELETE FROM process WHERE pid=42')
if sys.argv[2]=='ambiguous': db.execute('INSERT INTO process VALUES(4,42,NULL,0,2000000000)')
cursor=db.execute(sys.argv[1]);out=csv.writer(sys.stdout,lineterminator='\n');out.writerows(cursor)
"#;
        let (_, query) = perfetto_series_queries(Some(42), "fixture")
            .into_iter()
            .find(|(name, _)| name == "perfetto.scheduler_duration_ms_per_s")
            .unwrap();
        for (mode, expected) in [
            ("valid", "0,150.0\n1,350.0\n"),
            ("absent", ""),
            ("ambiguous", ""),
        ] {
            let output = Command::new("python3")
                .args(["-c", script, &query, mode])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                expected,
                "{mode}"
            );
        }
    }

    #[test]
    fn actual_sql_aggregates_beyond_svg_limit_clips_bounds_and_excludes_unfinished() {
        // Standard-library SQLite executes the actual product query, not a
        // mocked expected output. No native tracing or third-party SDK needed.
        let script = r#"
import csv,io,sqlite3,sys
db=sqlite3.connect(':memory:')
db.executescript('''
CREATE TABLE process(upid INTEGER,pid INTEGER,parent_upid INTEGER,start_ts INTEGER,end_ts INTEGER);
CREATE TABLE thread(utid INTEGER,tid INTEGER,upid INTEGER);
CREATE TABLE slice(ts INTEGER,name TEXT);
CREATE TABLE sched(ts INTEGER,dur INTEGER,utid INTEGER);
CREATE TABLE thread_state(ts INTEGER,dur INTEGER,utid INTEGER,state TEXT,io_wait INTEGER);
INSERT INTO process VALUES(1,42,NULL,0,10000),(2,43,1,0,10000),(3,99,NULL,0,10000);
INSERT INTO thread VALUES(1,42,1),(2,43,2),(3,99,3);
INSERT INTO slice VALUES(100,'asb.agent.started.fixture'),(9000,'asb.agent.exited.fixture');
INSERT INTO thread_state VALUES(50,100,1,'Running',NULL),(8950,100,1,'R',NULL),(200,-1,2,'S',NULL),(200,1000,3,'Running',NULL),(200,500,2,'D',NULL);
''')
db.executemany('INSERT INTO thread_state VALUES(?,1,1,?,NULL)',[(i+200,'Running' if i%2==0 else 'R+') for i in range(5000)])
if len(sys.argv)>2:
 if sys.argv[2]=='absent': db.execute('DELETE FROM process WHERE pid=42')
 if sys.argv[2]=='ambiguous': db.execute('INSERT INTO process VALUES(4,42,NULL,0,10000)')
cursor=db.execute(sys.argv[1]); out=csv.writer(sys.stdout,lineterminator='\n');out.writerow([c[0] for c in cursor.description]);out.writerows(cursor)
"#;
        let output = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(query(42, "fixture"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (summary, metrics) = parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
        assert_eq!((summary.start, summary.end), (100, 9000));
        assert_eq!(
            (summary.intervals, summary.threads, summary.unfinished),
            (5003, 2, 1)
        );
        assert_eq!(metrics["running_ms"], 0.00255);
        assert_eq!(metrics["runnable_ms"], 0.00255);
        assert_eq!(metrics["uninterruptible_sleep_ms"], 0.0005);
        assert!(metrics["io_wait_ms"].is_null());
        assert!(!query(42, "fixture").contains("LIMIT 4096"));
        for mode in ["absent", "ambiguous"] {
            let output = Command::new("python3")
                .arg("-c")
                .arg(script)
                .arg(query(42, "fixture"))
                .arg(mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                parse(std::str::from_utf8(&output.stdout).unwrap()).is_err(),
                "{mode} must publish no numeric summary"
            );
        }
    }
}
