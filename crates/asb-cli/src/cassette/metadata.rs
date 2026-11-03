//! A bounded metadata view prevents ZIP fallback parsing unapproved directories.
//!
//! The ZIP crate may search earlier EOCDs after a parse error and follows header
//! lengths rather than enforcing the EOCD directory-byte bound. Before creating
//! its index we therefore walk a bounded directory snapshot, and expose only that
//! snapshot and its chosen EOCD. All other construction reads synthesize zeros.
//! Payload access is enabled only after checking the index's offset and count.
use super::{Error, Limits};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// Fixed ZIP32 schema widths, shared by validation and recording reservation.
const DIRECTORY_HEADER_BYTES: usize = 46;
const END_RECORD_BYTES: usize = 22;

pub(super) fn recording_directory_size(count: u64) -> Result<u64, Error> {
    let manifest = DIRECTORY_HEADER_BYTES as u64 + "manifest.json".len() as u64;
    let track = DIRECTORY_HEADER_BYTES as u64 + format!("tracks/{count}.jsonl").len() as u64;
    track
        .checked_mul(count)
        .and_then(|bytes| bytes.checked_add(manifest))
        .and_then(|bytes| bytes.checked_add(END_RECORD_BYTES as u64))
        .ok_or_else(|| Error("recording ZIP directory capacity overflow".into()))
}

pub(super) struct MetadataReader {
    file: File,
    phase: Arc<AtomicBool>,
    position: u64,
    length: u64,
    start: u64,
    metadata: Vec<u8>,
}

impl MetadataReader {
    pub(super) fn metadata_bytes(&self) -> u64 {
        self.metadata.len() as u64
    }

    pub(super) fn new(
        mut file: File,
        limits: Limits,
    ) -> Result<(Self, Arc<AtomicBool>, usize, u64), Error> {
        let length = file.metadata().map_err(error)?.len();
        if length < END_RECORD_BYTES as u64 || length > limits.max_archive_bytes {
            return Err(Error(
                "cassette archive length is invalid or exceeds limit".into(),
            ));
        }
        let tail_len = length.min(65_557);
        file.seek(SeekFrom::Start(length - tail_len))
            .map_err(error)?;
        let mut tail = vec![0; tail_len as usize];
        file.read_exact(&mut tail).map_err(error)?;
        let offset = (0..=tail.len() - END_RECORD_BYTES)
            .rev()
            .find(|index| {
                &tail[*index..*index + 4] == b"PK\x05\x06"
                    && *index + END_RECORD_BYTES + word(&tail[*index..], 20) as usize == tail.len()
            })
            .ok_or_else(|| Error("cassette ZIP directory is missing or truncated".into()))?;
        let end = &tail[offset..];
        let entries32 = word(end, 10) as u64;
        let bytes32 = dword(end, 12);
        let start32 = dword(end, 16);
        let eocd_offset = length - tail_len + offset as u64;
        if word(end, 4) != 0 || word(end, 6) != 0 || word(end, 8) as u64 != entries32 {
            return Err(Error(
                "unsupported or excessive cassette ZIP directory".into(),
            ));
        }
        let locator_offset = eocd_offset.checked_sub(20);
        let mut locator = [0u8; 20];
        let has_locator = if let Some(position) = locator_offset {
            file.seek(SeekFrom::Start(position)).map_err(error)?;
            file.read_exact(&mut locator).map_err(error)?;
            &locator[..4] == b"PK\x06\x07"
        } else {
            false
        };
        let mut zip64_header = None;
        let (entries, bytes, start, directory_end) = if has_locator {
            let position = qword(&locator, 8);
            let mut header = [0u8; 56];
            if dword(&locator, 4) != 0
                || dword(&locator, 16) != 1
                || position
                    .checked_add(56)
                    .is_none_or(|end| end > locator_offset.unwrap())
            {
                return Err(Error("invalid ZIP64 locator".into()));
            }
            file.seek(SeekFrom::Start(position)).map_err(error)?;
            file.read_exact(&mut header).map_err(error)?;
            let size = qword(&header, 4);
            let entries = qword(&header, 32);
            let bytes = qword(&header, 40);
            let start = qword(&header, 48);
            if &header[..4] != b"PK\x06\x06"
                || size < 44
                || position
                    .checked_add(12)
                    .and_then(|end| end.checked_add(size))
                    != locator_offset
                || dword(&header, 16) != 0
                || dword(&header, 20) != 0
                || qword(&header, 24) != entries
                || (entries32 != u16::MAX as u64 && entries32 != entries)
                || (bytes32 != u32::MAX as u64 && bytes32 != bytes)
                || (start32 != u32::MAX as u64 && start32 != start)
            {
                return Err(Error("invalid ZIP64 directory".into()));
            }
            zip64_header = Some(header);
            (entries, bytes, start, position)
        } else {
            if entries32 == u16::MAX as u64
                || bytes32 == u32::MAX as u64
                || start32 == u32::MAX as u64
            {
                return Err(Error("missing ZIP64 directory".into()));
            }
            (entries32, bytes32, start32, eocd_offset)
        };
        if entries == 0
            || entries > limits.max_tracks.saturating_add(1) as u64
            || bytes > limits.max_directory_bytes
            || start.checked_add(bytes) != Some(directory_end)
        {
            return Err(Error(
                "unsupported or excessive cassette ZIP directory".into(),
            ));
        }
        let entries =
            usize::try_from(entries).map_err(|_| Error("cassette entry count overflow".into()))?;
        let snapshot_len = length
            .checked_sub(start)
            .filter(|length| *length <= limits.max_directory_bytes)
            .and_then(|length| usize::try_from(length).ok())
            .ok_or_else(|| {
                Error("cassette ZIP directory and end record exceed metadata limit".into())
            })?;
        file.seek(SeekFrom::Start(start)).map_err(error)?;
        let count =
            usize::try_from(bytes).map_err(|_| Error("cassette directory size overflow".into()))?;
        let mut metadata = Vec::new();
        metadata
            .try_reserve_exact(snapshot_len)
            .map_err(|_| Error("cannot allocate bounded cassette directory snapshot".into()))?;
        metadata.resize(snapshot_len, 0);
        file.read_exact(&mut metadata).map_err(error)?;
        let overlap = start.max(length - tail_len);
        if metadata[(overlap - start) as usize..]
            != tail[(overlap - (length - tail_len)) as usize..]
            || zip64_header.is_some_and(|header| {
                metadata.get(count..count + header.len()) != Some(header.as_slice())
            })
        {
            return Err(Error(
                "cassette ZIP directory changed during inspection".into(),
            ));
        }
        validate_directory(&metadata[..count], entries, start)?;
        let phase = Arc::new(AtomicBool::new(true));
        Ok((
            Self {
                file,
                phase: phase.clone(),
                position: 0,
                length,
                start,
                metadata,
            },
            phase,
            entries,
            start,
        ))
    }
}

fn validate_directory(bytes: &[u8], count: usize, start: u64) -> Result<(), Error> {
    let bad = || {
        Error("cassette central-directory headers exceed or disagree with approved bounds".into())
    };
    let mut cursor = 0usize;
    for _ in 0..count {
        let header = bytes
            .get(cursor..cursor.checked_add(DIRECTORY_HEADER_BYTES).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        if &header[..4] != b"PK\x01\x02" || word(header, 34) != 0 {
            return Err(bad());
        }
        let name_len = word(header, 28) as usize;
        let extra_len = word(header, 30) as usize;
        let comment_len = word(header, 32) as usize;
        let extra_start = cursor
            .checked_add(DIRECTORY_HEADER_BYTES)
            .and_then(|value| value.checked_add(name_len))
            .ok_or_else(bad)?;
        let extra_end = extra_start.checked_add(extra_len).ok_or_else(bad)?;
        let extra = bytes.get(extra_start..extra_end).ok_or_else(bad)?;
        let mut field = 0usize;
        let mut zip64 = None;
        while field < extra.len() {
            let info = extra
                .get(field..field.checked_add(4).ok_or_else(bad)?)
                .ok_or_else(bad)?;
            let next = field
                .checked_add(4 + word(info, 2) as usize)
                .filter(|end| *end <= extra.len())
                .ok_or_else(bad)?;
            if word(info, 0) == 1 {
                if zip64.is_some() {
                    return Err(bad());
                }
                zip64 = Some(&extra[field + 4..next]);
            }
            field = next;
        }
        let mut zip64_offset = 0usize;
        let mut resolved = [0u64; 3];
        for (slot, index) in [24usize, 20, 42].into_iter().enumerate() {
            let value = dword(header, index);
            resolved[slot] = if value == u32::MAX as u64 {
                let values = zip64.ok_or_else(bad)?;
                let end = zip64_offset.checked_add(8).ok_or_else(bad)?;
                let bytes = values.get(zip64_offset..end).ok_or_else(bad)?;
                zip64_offset = end;
                qword(bytes, 0)
            } else {
                value
            };
        }
        if resolved[2] >= start || zip64.is_some_and(|values| values.len() != zip64_offset) {
            return Err(bad());
        }
        cursor = extra_end
            .checked_add(comment_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(bad)?;
    }
    if cursor != bytes.len() {
        return Err(bad());
    }
    Ok(())
}

impl Read for MetadataReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if !self.phase.load(Ordering::Acquire) {
            self.file.seek(SeekFrom::Start(self.position))?;
            let count = self.file.read(output)?;
            self.position += count as u64;
            return Ok(count);
        }
        let count = self
            .length
            .saturating_sub(self.position)
            .min(output.len() as u64) as usize;
        output[..count].fill(0);
        let begin = self.position.max(self.start);
        let end = (self.position + count as u64).min(self.length);
        if begin < end {
            let destination = (begin - self.position) as usize;
            let source = (begin - self.start) as usize;
            let length = (end - begin) as usize;
            output[destination..destination + length]
                .copy_from_slice(&self.metadata[source..source + length]);
        }
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for MetadataReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::End(value) => i128::from(self.length) + i128::from(value),
            SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
        };
        self.position = u64::try_from(position)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid cassette seek"))?;
        Ok(self.position)
    }
}

fn word(bytes: &[u8], index: usize) -> u16 {
    u16::from_le_bytes([bytes[index], bytes[index + 1]])
}
fn dword(bytes: &[u8], index: usize) -> u64 {
    u32::from_le_bytes(
        bytes[index..index + 4]
            .try_into()
            .expect("bounded ZIP field"),
    ) as u64
}
fn qword(bytes: &[u8], index: usize) -> u64 {
    u64::from_le_bytes(
        bytes[index..index + 8]
            .try_into()
            .expect("bounded ZIP64 field"),
    )
}
fn error(error: io::Error) -> Error {
    Error(format!("cassette metadata I/O error: {error}"))
}
