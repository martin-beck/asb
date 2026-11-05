//! Inspect perf's record framing before accepting decoded SPE observations.
//! AUX payloads are opaque bytes; their surrounding perf metadata is bounded.
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Debug, serde::Serialize)]
pub(super) struct Lifetime {
    pub pid: u32,
    pub start_ns: u64,
    pub end_ns: u64,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct Quality {
    pub tasks: BTreeMap<u32, Lifetime>,
    pub aux_records: u64,
    pub time_conversion: Option<crate::measurements::phase_alignment::TimeConversion>,
}

fn word(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or("truncated perf field")?
            .try_into()
            .map_err(|_| "truncated perf field")?,
    ))
}
fn pid(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or("truncated perf task identity")?
            .try_into()
            .map_err(|_| "truncated perf task identity")?,
    ))
}

pub(super) fn inspect(
    mut file: impl Read + Seek,
    root: u32,
    budget: u64,
) -> Result<Quality, String> {
    let size = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    if root == 0 || size > budget || size < 104 {
        return Err("SPE recording exceeds its validation bound or has no valid header".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut header = [0; 104];
    file.read_exact(&mut header).map_err(|e| e.to_string())?;
    if &header[..8] != b"PERFILE2" || word(&header, 8)? < 104 {
        return Err("Unsupported SPE perf file header or byte order".into());
    }
    let mut offset = word(&header, 40)?;
    let end = offset
        .checked_add(word(&header, 48)?)
        .filter(|end| *end <= size)
        .ok_or("SPE data section is truncated")?;
    if offset < word(&header, 8)? || offset == end {
        return Err("SPE data section is absent or overlaps its header".into());
    }
    let mut forks = BTreeMap::new();
    let mut exits = BTreeMap::new();
    let mut aux_records = 0;
    let mut time_conversion = None;
    let mut records = 0;
    while offset < end {
        if forks.len() + exits.len() > 200_000 {
            return Err("SPE task metadata exceeds its bound".into());
        }
        records += 1;
        if records > 1_000_000 {
            return Err("SPE metadata exceeds its record bound".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut raw = [0; 8];
        file.read_exact(&mut raw)
            .map_err(|_| "truncated perf record header")?;
        let kind = pid(&raw, 0)?;
        let length = u16::from_le_bytes([raw[6], raw[7]]) as u64;
        let next = offset
            .checked_add(length)
            .filter(|v| *v <= end && length >= 8)
            .ok_or("Invalid SPE perf record size")?;
        let mut body = vec![0; (length - 8) as usize];
        file.read_exact(&mut body)
            .map_err(|_| "truncated perf record")?;
        offset = next;
        match kind {
            2 => {
                if word(&body, 8)? != 0 {
                    return Err("SPE perf events were lost".into());
                }
            }
            13 => {
                if word(&body, 0)? != 0 {
                    return Err("SPE samples were lost".into());
                }
            }
            11 => {
                if word(&body, 16)? != 0 {
                    return Err(
                        "SPE AUX data is truncated, overwritten, partial or has unknown flags"
                            .into(),
                    );
                }
                aux_records += 1;
            }
            7 | 4 => {
                let identity = (pid(&body, 0)?, pid(&body, 8)?);
                let parent = (pid(&body, 4)?, pid(&body, 12)?);
                let time = word(&body, 16)?;
                if identity.0 == 0 || identity.1 == 0 {
                    return Err("SPE task identity is unavailable".into());
                }
                if kind == 7 {
                    if forks
                        .insert(identity.1, (identity.0, parent, time))
                        .is_some()
                    {
                        return Err("SPE task ID reuse is ambiguous".into());
                    }
                } else if exits.insert(identity.1, (identity.0, time)).is_some() {
                    return Err("SPE task exit is ambiguous".into());
                }
            }
            71 => {
                offset = offset
                    .checked_add(word(&body, 0)?)
                    .filter(|v| *v <= end)
                    .ok_or("SPE AUX payload is truncated")?;
            }
            79 => {
                if body.len() != 24 && body.len() != 48 {
                    return Err("Unsupported perf time conversion record layout".into());
                }
                if body.len() == 48 {
                    let candidate = crate::measurements::phase_alignment::TimeConversion {
                        time_shift: word(&body, 0)?,
                        time_mult: word(&body, 8)?,
                        time_zero: word(&body, 16)?,
                        time_cycles: word(&body, 24)?,
                        time_mask: word(&body, 32)?,
                        cap_user_time_zero: body[40],
                        cap_user_time_short: body[41],
                    };
                    if time_conversion
                        .as_ref()
                        .is_some_and(|old| old != &candidate)
                    {
                        return Err("Conflicting perf time conversion records".into());
                    }
                    time_conversion = Some(candidate);
                }
            }
            72 | 81 => {
                return Err(
                    "SPE auxiliary decode errors or compressed records are unsupported".into(),
                );
            }
            _ => {}
        }
    }
    if aux_records == 0 {
        return Err("No validated SPE AUX records were captured".into());
    }
    if forks.contains_key(&root) {
        return Err("SPE root task generation is ambiguous".into());
    }
    let root_end = exits.get(&root).filter(|(pid, _)| *pid == root).map(|(_, end)| *end)
        .ok_or("SPE capture ended before the agent root exit was recorded (capture may have reached its budget)")?;
    let mut tasks = BTreeMap::from([(
        root,
        Lifetime {
            pid: root,
            start_ns: 0,
            end_ns: root_end,
        },
    )]);
    while !forks.is_empty() {
        let mut admitted = Vec::new();
        for (tid, (pid, (ppid, ptid), start)) in &forks {
            if let Some(parent) = tasks.get(ptid).filter(|parent| parent.pid == *ppid) {
                let end = exits
                    .get(tid)
                    .filter(|(p, _)| p == pid)
                    .map(|(_, end)| *end)
                    .ok_or("SPE descendant exit was not captured")?;
                if *start < parent.start_ns || *start > parent.end_ns || end < *start {
                    return Err("SPE task lifetime contradicts recorded ancestry".into());
                }
                admitted.push((
                    *tid,
                    Lifetime {
                        pid: *pid,
                        start_ns: *start,
                        end_ns: end,
                    },
                ));
            }
        }
        if admitted.is_empty() {
            return Err("SPE fork metadata contains unrelated or unresolved tasks".into());
        }
        for (tid, task) in admitted {
            forks.remove(&tid);
            tasks.insert(tid, task);
        }
    }
    for (tid, (pid, _)) in exits {
        if tasks.get(&tid).is_none_or(|task| task.pid != pid) {
            return Err("SPE exit metadata contains an unrelated task".into());
        }
    }
    Ok(Quality {
        tasks,
        aux_records,
        time_conversion,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(kind: u32, body: &[u8]) -> Vec<u8> {
        [
            kind.to_le_bytes().as_slice(),
            &[0, 0],
            &((body.len() + 8) as u16).to_le_bytes(),
            body,
        ]
        .concat()
    }
    fn task(pid: u32, parent: u32, tid: u32, ptid: u32, time: u64) -> Vec<u8> {
        [
            pid.to_le_bytes().as_slice(),
            parent.to_le_bytes().as_slice(),
            tid.to_le_bytes().as_slice(),
            ptid.to_le_bytes().as_slice(),
            &time.to_le_bytes(),
        ]
        .concat()
    }
    fn file(extra: Vec<u8>) -> Vec<u8> {
        let data = [
            record(11, &[0; 24]),
            record(7, &task(43, 42, 43, 42, 10)),
            record(4, &task(43, 42, 43, 42, 20)),
            extra,
            record(4, &task(42, 1, 42, 1, 30)),
        ]
        .concat();
        let mut header = vec![0; 104];
        header[..8].copy_from_slice(b"PERFILE2");
        header[8..16].copy_from_slice(&104u64.to_le_bytes());
        header[40..48].copy_from_slice(&104u64.to_le_bytes());
        header[48..56].copy_from_slice(&(data.len() as u64).to_le_bytes());
        [header, data].concat()
    }
    #[test]
    fn timer_conversion_retains_exact_fields_and_rejects_conflicting_records() {
        let mut body = vec![0u8; 48];
        for (offset, value) in [
            (0, 21u64),
            (8, 20_971_520),
            (16, u64::MAX - 4),
            (24, 1000),
            (32, 65_535),
        ] {
            body[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        body[40] = 1;
        body[41] = 1;
        let quality = inspect(std::io::Cursor::new(file(record(79, &body))), 42, 4096).unwrap();
        let tc = quality.time_conversion.unwrap();
        assert_eq!(tc.time_shift, 21);
        assert_eq!(tc.time_zero, u64::MAX - 4);
        assert_eq!(tc.time_cycles, 1000);
        assert_eq!(tc.cap_user_time_short, 1);
        let mut conflict = body.clone();
        conflict[0] = 22;
        assert!(
            inspect(
                std::io::Cursor::new(file([record(79, &body), record(79, &conflict)].concat())),
                42,
                4096
            )
            .is_err()
        );
        assert!(inspect(std::io::Cursor::new(file(record(79, &[0; 32]))), 42, 4096).is_err());
        assert!(
            inspect(std::io::Cursor::new(file(record(79, &[0; 24]))), 42, 4096)
                .unwrap()
                .time_conversion
                .is_none()
        );
    }

    #[test]
    fn raw_records_prove_root_and_descendant_lifetimes() {
        let quality = inspect(std::io::Cursor::new(file(vec![])), 42, 4096).unwrap();
        assert_eq!(quality.tasks.len(), 2);
        assert_eq!(quality.tasks[&43].start_ns, 10);
        assert_eq!(quality.aux_records, 1);
    }
    #[test]
    fn loss_aux_flags_unrelated_tasks_and_id_reuse_are_rejected() {
        for extra in [
            record(13, &1u64.to_le_bytes()),
            record(
                11,
                &[
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
                ],
            ),
            record(7, &task(44, 99, 44, 99, 15)),
            record(7, &task(43, 42, 43, 42, 15)),
            record(4, &task(99, 1, 99, 1, 20)),
            record(72, &[0; 16]),
        ] {
            assert!(inspect(std::io::Cursor::new(file(extra)), 42, 4096).is_err());
        }
    }
    #[test]
    fn truncated_payloads_headers_sections_and_size_bounds_are_rejected() {
        let mut bytes = file(vec![]);
        bytes.pop();
        assert!(inspect(std::io::Cursor::new(bytes), 42, 4096).is_err());
        assert!(
            inspect(
                std::io::Cursor::new(file(record(71, &10000u64.to_le_bytes()))),
                42,
                4096
            )
            .is_err()
        );
        assert!(inspect(std::io::Cursor::new(file(vec![])), 42, 32).is_err());
        let mut bytes = file(vec![]);
        bytes[0] = b'?';
        assert!(inspect(std::io::Cursor::new(bytes), 42, 4096).is_err());
    }
}
