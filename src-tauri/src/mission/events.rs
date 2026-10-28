use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::{
    model::{
        MissionCompactedHistoryBoundary, MissionEvent, MissionEventActor, MissionEventCheckpoint,
        MissionEventCursor, MissionEventKind, MissionEventPage, MissionEventProjection, MissionId,
        MAX_SUMMARY_CHARS,
    },
    snapshot,
    store::MissionStore,
};
use crate::recovery_store;

pub(crate) const MAX_EVENT_PAYLOAD_CHARS: usize = MAX_SUMMARY_CHARS;
const MAX_EVENT_TASK_ID_CHARS: usize = 160;
const MAX_EVENT_ACTOR_ID_CHARS: usize = 160;
const MAX_CURSOR_PAGE: usize = 256;
const PRODUCTION_SEGMENT_EVENTS: usize = 256;
const PRODUCTION_SEGMENT_BYTES: u64 = 1024 * 1024;
const PRODUCTION_COMPACT_AFTER_CLOSED_SEGMENTS: usize = 64;
const PRODUCTION_RETAIN_CLOSED_SEGMENTS: usize = 32;
const MAX_SEGMENT_RECOVERY_SLACK: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissionEventInput {
    pub(crate) kind: MissionEventKind,
    pub(crate) actor: MissionEventActor,
    pub(crate) task_id: Option<String>,
    pub(crate) payload_summary: String,
}

#[derive(Clone, Copy)]
struct JournalLimits {
    max_events_per_segment: usize,
    max_segment_bytes: u64,
    compact_after_closed_segments: usize,
    retain_closed_segments: usize,
    fsync: bool,
}

impl JournalLimits {
    fn production() -> Self {
        Self {
            max_events_per_segment: PRODUCTION_SEGMENT_EVENTS,
            max_segment_bytes: PRODUCTION_SEGMENT_BYTES,
            compact_after_closed_segments: PRODUCTION_COMPACT_AFTER_CLOSED_SEGMENTS,
            retain_closed_segments: PRODUCTION_RETAIN_CLOSED_SEGMENTS,
            fsync: true,
        }
    }

    #[cfg(test)]
    fn testing(
        max_events_per_segment: usize,
        max_segment_bytes: u64,
        compact_after_closed_segments: usize,
    ) -> Self {
        Self {
            max_events_per_segment: max_events_per_segment.max(1),
            max_segment_bytes: max_segment_bytes.max(1024),
            compact_after_closed_segments,
            retain_closed_segments: if compact_after_closed_segments == usize::MAX {
                usize::MAX
            } else {
                (compact_after_closed_segments / 2).max(1)
            },
            fsync: false,
        }
    }
}

struct EventJournal {
    root: PathBuf,
    limits: JournalLimits,
}

struct ParsedSegment {
    events: Vec<MissionEvent>,
    complete_bytes: u64,
    actual_bytes: u64,
}

struct MeasuredPage {
    page: MissionEventPage,
    #[cfg_attr(not(test), allow(dead_code))]
    segments_read: usize,
}

impl EventJournal {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            limits: JournalLimits::production(),
        }
    }

    #[cfg(test)]
    fn with_limits(root: PathBuf, limits: JournalLimits) -> Self {
        Self { root, limits }
    }

    fn from_app(app: &AppHandle, mission_id: &MissionId) -> Result<Self, String> {
        let store = MissionStore::from_app(app)?;
        Ok(Self::new(store.events_directory(mission_id)))
    }

    fn segment_path(&self, start_seq: u64) -> PathBuf {
        self.root.join(format!("segment-{start_seq:020}.jsonl"))
    }

    fn checkpoint_path(&self, revision: u64) -> PathBuf {
        self.root.join(format!("checkpoint-{revision:020}.json"))
    }

    fn ensure_directory(&self) -> Result<(), String> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(
                        "Refusing Agent Mission event storage through a symbolic link.".to_string(),
                    );
                }
                if !metadata.file_type().is_dir() {
                    return Err("Refusing non-directory Agent Mission event storage.".to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.root).map_err(|error| {
                    format!("Could not create Agent Mission event storage: {error}")
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700)).map_err(
                        |error| format!("Could not secure Agent Mission event storage: {error}"),
                    )?;
                }
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect Agent Mission event storage: {error}"
                ));
            }
        }
        Ok(())
    }

    fn ensure_regular_or_missing(path: &Path, label: &str) -> Result<bool, String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!("Refusing {label} through a symbolic link."));
                }
                if !metadata.file_type().is_file() {
                    return Err(format!("Refusing non-file {label}."));
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("Could not inspect {label}: {error}")),
        }
    }

    fn segment_start_from_name(name: &str) -> Option<u64> {
        let value = name.strip_prefix("segment-")?.strip_suffix(".jsonl")?;
        value.parse().ok()
    }

    fn checkpoint_revision_from_name(name: &str) -> Option<u64> {
        let value = name.strip_prefix("checkpoint-")?.strip_suffix(".json")?;
        value.parse().ok()
    }

    fn segment_paths(&self) -> Result<Vec<(u64, PathBuf)>, String> {
        match fs::symlink_metadata(&self.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!(
                    "Could not inspect Agent Mission event storage: {error}"
                ))
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(
                    "Refusing Agent Mission event storage through a symbolic link.".to_string(),
                )
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err("Refusing non-directory Agent Mission event storage.".to_string())
            }
            Ok(_) => {}
        }
        let mut segments = Vec::new();
        for entry in fs::read_dir(&self.root)
            .map_err(|error| format!("Could not list Agent Mission event storage: {error}"))?
        {
            let entry = entry.map_err(|error| format!("Could not read event entry: {error}"))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(start) = Self::segment_start_from_name(&name) else {
                continue;
            };
            Self::ensure_regular_or_missing(&entry.path(), "Agent Mission event segment")?;
            segments.push((start, entry.path()));
        }
        segments.sort_by_key(|(start, _)| *start);
        if segments.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err("Agent Mission journal contains duplicate segment starts.".to_string());
        }
        Ok(segments)
    }

    fn read_segment(
        &self,
        start_seq: u64,
        path: &Path,
        active: bool,
    ) -> Result<ParsedSegment, String> {
        Self::ensure_regular_or_missing(path, "Agent Mission event segment")?;
        let metadata = fs::metadata(path)
            .map_err(|error| format!("Could not stat Agent Mission event segment: {error}"))?;
        if metadata.len()
            > self
                .limits
                .max_segment_bytes
                .saturating_add(MAX_SEGMENT_RECOVERY_SLACK)
        {
            return Err("Agent Mission event segment exceeds its recovery bound.".to_string());
        }
        let bytes = fs::read(path)
            .map_err(|error| format!("Could not read Agent Mission event segment: {error}"))?;
        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        if !active && complete_len != bytes.len() {
            return Err("Closed Agent Mission event segment has an incomplete tail.".to_string());
        }

        let mut events = Vec::new();
        let mut expected_seq = start_seq;
        for raw_line in bytes[..complete_len].split(|byte| *byte == b'\n') {
            if raw_line.is_empty() {
                continue;
            }
            let event: MissionEvent = serde_json::from_slice(raw_line)
                .map_err(|error| format!("Agent Mission journal corruption: {error}"))?;
            if event.seq != expected_seq {
                return Err(format!(
                    "Agent Mission journal sequence gap: expected {expected_seq}, got {}.",
                    event.seq
                ));
            }
            validate_event(&event)?;
            expected_seq = expected_seq.saturating_add(1);
            events.push(event);
        }
        Ok(ParsedSegment {
            events,
            complete_bytes: complete_len as u64,
            actual_bytes: bytes.len() as u64,
        })
    }

    fn reconcile_projection(&self, projection: &mut MissionEventProjection) -> Result<(), String> {
        let committed_last = projection.last_event_seq;
        let mut segments = self.segment_paths()?;
        if segments.is_empty() {
            if committed_last > 0 {
                return Err(
                    "Agent Mission snapshot is ahead of the durable event journal.".to_string(),
                );
            }
            projection.retained_raw_start_seq = 1;
            projection.current_segment_start_seq = 0;
            projection.current_segment_event_count = 0;
            projection.current_segment_bytes = 0;
            projection.closed_segment_count = 0;
            projection.latest_human_directive_seqs.clear();
            return Ok(());
        }

        let (last_start, last_path) = segments.last().expect("non-empty segments");
        let parsed_last = self.read_segment(*last_start, last_path, true)?;
        let actual_last = parsed_last
            .events
            .last()
            .map(|event| event.seq)
            .unwrap_or(last_start.saturating_sub(1));
        if committed_last > actual_last {
            return Err(
                "Agent Mission snapshot is ahead of the durable event journal.".to_string(),
            );
        }

        // Journal records are written before the corresponding snapshot is atomically saved.
        // If the process dies after the journal fsync but before the snapshot save, records past
        // the snapshot cursor are uncommitted intent. Never promote those records into Mission
        // history because the journal cannot reconstruct the missing task/directive/decision state.
        if actual_last > committed_last {
            if committed_last == 0 {
                for (_, path) in &segments {
                    Self::ensure_regular_or_missing(path, "Agent Mission event segment")?;
                    fs::remove_file(path).map_err(|error| {
                        format!("Could not remove uncommitted Agent Mission event segment: {error}")
                    })?;
                }
                self.sync_directory()?;
                segments.clear();
            } else {
                let committed_segment_index = segments
                    .iter()
                    .rposition(|(start, _)| *start <= committed_last)
                    .ok_or_else(|| {
                        "Agent Mission committed event cursor is outside retained journal segments."
                            .to_string()
                    })?;
                let (committed_start, committed_path) = &segments[committed_segment_index];
                let committed_bytes =
                    self.bytes_through_seq(*committed_start, committed_path, committed_last)?;
                self.truncate_file(committed_path, committed_bytes)?;
                for (_, path) in segments.iter().skip(committed_segment_index + 1) {
                    Self::ensure_regular_or_missing(path, "Agent Mission event segment")?;
                    fs::remove_file(path).map_err(|error| {
                        format!("Could not remove uncommitted Agent Mission event segment: {error}")
                    })?;
                }
                self.sync_directory()?;
                segments.truncate(committed_segment_index + 1);
            }
        }

        projection
            .latest_human_directive_seqs
            .retain(|seq| *seq <= committed_last);
        if segments.is_empty() {
            projection.retained_raw_start_seq = 1;
            projection.current_segment_start_seq = 0;
            projection.current_segment_event_count = 0;
            projection.current_segment_bytes = 0;
            projection.closed_segment_count = 0;
            return Ok(());
        }

        let (last_start, last_path) = segments.last().expect("non-empty segments");
        let parsed = self.read_segment(*last_start, last_path, true)?;
        let recovered_last = parsed
            .events
            .last()
            .map(|event| event.seq)
            .unwrap_or(last_start.saturating_sub(1));
        if recovered_last != committed_last {
            return Err(format!(
                "Agent Mission journal recovery mismatch: snapshot committed {committed_last}, journal recovered {recovered_last}."
            ));
        }
        if parsed.actual_bytes != parsed.complete_bytes {
            self.truncate_file(last_path, parsed.complete_bytes)?;
        }

        let retained_start = segments[0].0;
        let snapshot_retained_start = projection.retained_raw_start_seq.max(1);
        if retained_start > snapshot_retained_start {
            let journal_mission_id = parsed
                .events
                .last()
                .map(|event| event.mission_id.clone())
                .ok_or_else(|| {
                    "Agent Mission compacted journal has no retained event identity.".to_string()
                })?;
            let checkpoint = self
                .latest_checkpoint(&journal_mission_id)?
                .ok_or_else(|| {
                    "Agent Mission journal discarded retained history without a durable checkpoint."
                        .to_string()
                })?;
            if checkpoint.summary_revision <= projection.summary_revision
                || checkpoint.compacted_through_seq.saturating_add(1) != retained_start
            {
                return Err(
                    "Agent Mission compaction checkpoint does not exactly bridge the snapshot and retained journal."
                        .to_string(),
                );
            }
            projection.summary_revision = checkpoint.summary_revision;
            projection.compacted_through_seq = checkpoint.compacted_through_seq;
            projection.retained_raw_start_seq = retained_start;
        } else {
            // Extra older segments are harmless after a snapshot commit; never widen the
            // snapshot's declared retained-history boundary merely because stale files remain.
            projection.retained_raw_start_seq = snapshot_retained_start;
        }
        projection.current_segment_start_seq = *last_start;
        projection.current_segment_event_count = parsed.events.len();
        projection.current_segment_bytes = parsed.complete_bytes;
        projection.closed_segment_count = segments.len().saturating_sub(1);
        for event in parsed.events {
            if event.kind == MissionEventKind::HumanDirective
                && !projection.latest_human_directive_seqs.contains(&event.seq)
            {
                projection.latest_human_directive_seqs.push(event.seq);
            }
        }
        while projection.latest_human_directive_seqs.len() > 32 {
            projection.latest_human_directive_seqs.remove(0);
        }
        Ok(())
    }

    fn bytes_through_seq(
        &self,
        start_seq: u64,
        path: &Path,
        target_seq: u64,
    ) -> Result<u64, String> {
        Self::ensure_regular_or_missing(path, "Agent Mission event segment")?;
        let bytes = fs::read(path)
            .map_err(|error| format!("Could not read Agent Mission event segment: {error}"))?;
        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let mut expected_seq = start_seq;
        let mut complete_bytes = 0_u64;
        for line in bytes[..complete_len].split_inclusive(|byte| *byte == b'\n') {
            let raw_line = line.strip_suffix(b"\n").unwrap_or(line);
            if raw_line.is_empty() {
                complete_bytes = complete_bytes.saturating_add(line.len() as u64);
                continue;
            }
            let event: MissionEvent = serde_json::from_slice(raw_line)
                .map_err(|error| format!("Agent Mission journal corruption: {error}"))?;
            if event.seq != expected_seq {
                return Err(format!(
                    "Agent Mission journal sequence gap: expected {expected_seq}, got {}.",
                    event.seq
                ));
            }
            validate_event(&event)?;
            complete_bytes = complete_bytes.saturating_add(line.len() as u64);
            if event.seq == target_seq {
                return Ok(complete_bytes);
            }
            if event.seq > target_seq {
                break;
            }
            expected_seq = expected_seq.saturating_add(1);
        }
        Err(format!(
            "Agent Mission committed event {target_seq} was not found in segment starting at {start_seq}."
        ))
    }

    fn sync_directory(&self) -> Result<(), String> {
        if !self.limits.fsync {
            return Ok(());
        }
        let directory = File::open(&self.root).map_err(|error| {
            format!("Could not open Agent Mission event directory for sync: {error}")
        })?;
        directory.sync_all().map_err(|error| {
            format!("Could not sync Agent Mission event directory after recovery: {error}")
        })
    }

    fn truncate_file(&self, path: &Path, len: u64) -> Result<(), String> {
        Self::ensure_regular_or_missing(path, "Agent Mission event segment")?;
        let file = OpenOptions::new().write(true).open(path).map_err(|error| {
            format!("Could not open Agent Mission event segment for recovery: {error}")
        })?;
        file.set_len(len)
            .map_err(|error| format!("Could not repair Agent Mission event tail: {error}"))?;
        if self.limits.fsync {
            file.sync_all().map_err(|error| {
                format!("Could not sync repaired Agent Mission event tail: {error}")
            })?;
        }
        Ok(())
    }

    fn append(
        &self,
        mission_id: &MissionId,
        authority_revision: u64,
        projection: &mut MissionEventProjection,
        input: MissionEventInput,
        now: u64,
    ) -> Result<MissionEvent, String> {
        validate_event_input(&input)?;
        if authority_revision == 0 {
            return Err("Agent Mission event authority revision must be positive.".to_string());
        }
        self.ensure_directory()?;
        self.reconcile_projection(projection)?;

        let next_seq = projection.last_event_seq.saturating_add(1);
        if next_seq == 0 {
            return Err("Agent Mission event sequence overflowed.".to_string());
        }
        let event = MissionEvent {
            seq: next_seq,
            mission_id: mission_id.clone(),
            authority_revision,
            kind: input.kind,
            actor: input.actor,
            task_id: input.task_id,
            created_at: now,
            payload_summary: input.payload_summary.trim().to_string(),
        };
        validate_event(&event)?;
        let mut line = serde_json::to_vec(&event)
            .map_err(|error| format!("Could not encode Agent Mission event: {error}"))?;
        line.push(b'\n');
        let line_len = line.len() as u64;
        if line_len > self.limits.max_segment_bytes {
            return Err("Agent Mission event is too large for one journal segment.".to_string());
        }

        let rotate = projection.current_segment_start_seq != 0
            && (projection.current_segment_event_count >= self.limits.max_events_per_segment
                || projection.current_segment_bytes.saturating_add(line_len)
                    > self.limits.max_segment_bytes);
        if rotate {
            projection.closed_segment_count = projection.closed_segment_count.saturating_add(1);
            projection.current_segment_start_seq = next_seq;
            projection.current_segment_event_count = 0;
            projection.current_segment_bytes = 0;
        } else if projection.current_segment_start_seq == 0 {
            projection.current_segment_start_seq = next_seq;
        }

        let path = self.segment_path(projection.current_segment_start_seq);
        let existed = Self::ensure_regular_or_missing(&path, "Agent Mission event segment")?;
        let mut options = OpenOptions::new();
        options.create(true).append(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| format!("Could not append Agent Mission event: {error}"))?;
        if !existed {
            Self::ensure_regular_or_missing(&path, "Agent Mission event segment")?;
        }
        file.write_all(&line)
            .map_err(|error| format!("Could not write Agent Mission event: {error}"))?;
        if self.limits.fsync {
            file.sync_data()
                .map_err(|error| format!("Could not fsync Agent Mission event: {error}"))?;
        }

        projection.last_event_seq = next_seq;
        if projection.retained_raw_start_seq == 0 {
            projection.retained_raw_start_seq = projection.current_segment_start_seq;
        }
        projection.current_segment_event_count =
            projection.current_segment_event_count.saturating_add(1);
        projection.current_segment_bytes =
            projection.current_segment_bytes.saturating_add(line_len);
        if event.kind == MissionEventKind::HumanDirective {
            projection.latest_human_directive_seqs.push(event.seq);
            while projection.latest_human_directive_seqs.len() > 32 {
                projection.latest_human_directive_seqs.remove(0);
            }
        }
        Ok(event)
    }

    fn read_since(
        &self,
        mission_id: &MissionId,
        after_seq: u64,
        limit: usize,
        projection: &MissionEventProjection,
    ) -> Result<MissionEventPage, String> {
        Ok(self
            .read_since_with_stats(mission_id, after_seq, limit, projection)?
            .page)
    }

    fn read_since_with_stats(
        &self,
        mission_id: &MissionId,
        after_seq: u64,
        limit: usize,
        projection: &MissionEventProjection,
    ) -> Result<MeasuredPage, String> {
        if limit == 0 || limit > MAX_CURSOR_PAGE {
            return Err(format!(
                "Agent Mission event page limit must be 1..={MAX_CURSOR_PAGE}."
            ));
        }
        if after_seq > projection.last_event_seq {
            return Err("Agent Mission event cursor is in the future.".to_string());
        }
        if projection.last_event_seq == 0 || after_seq == projection.last_event_seq {
            return Ok(MeasuredPage {
                page: MissionEventPage {
                    events: Vec::new(),
                    next_cursor: MissionEventCursor(after_seq),
                    has_more: false,
                    compacted_boundary: None,
                },
                segments_read: 0,
            });
        }

        let retained_from = projection.retained_raw_start_seq.max(1);
        if after_seq.saturating_add(1) < retained_from {
            if projection.compacted_through_seq > 0 {
                return Ok(MeasuredPage {
                    page: MissionEventPage {
                        events: Vec::new(),
                        next_cursor: MissionEventCursor(after_seq),
                        has_more: true,
                        compacted_boundary: Some(MissionCompactedHistoryBoundary {
                            summary_revision: projection.summary_revision,
                            compacted_through_seq: projection.compacted_through_seq,
                            retained_from_seq: retained_from,
                        }),
                    },
                    segments_read: 0,
                });
            }
            return Err(
                "Agent Mission event cursor does not refer to retained history.".to_string(),
            );
        }

        let segments = self.segment_paths()?;
        if segments.is_empty() {
            return Err(
                "Agent Mission event projection references missing journal data.".to_string(),
            );
        }
        let target = after_seq.saturating_add(1);
        let mut start_index = 0usize;
        for (index, (start, _)) in segments.iter().enumerate() {
            if *start <= target {
                start_index = index;
            } else {
                break;
            }
        }

        let mut events = Vec::with_capacity(limit);
        let mut segments_read = 0usize;
        'segments: for (index, (start, path)) in segments.iter().enumerate().skip(start_index) {
            let parsed = self.read_segment(*start, path, index + 1 == segments.len())?;
            segments_read = segments_read.saturating_add(1);
            for event in parsed.events {
                if event.mission_id != *mission_id {
                    return Err(
                        "Agent Mission journal contains an event for another Mission.".to_string(),
                    );
                }
                if event.seq <= after_seq {
                    continue;
                }
                events.push(event);
                if events.len() == limit {
                    break 'segments;
                }
            }
        }

        if events.first().map(|event| event.seq) != Some(target) {
            return Err(
                "Agent Mission event cursor points into missing or corrupted history.".to_string(),
            );
        }
        let next = events.last().map(|event| event.seq).unwrap_or(after_seq);
        Ok(MeasuredPage {
            page: MissionEventPage {
                events,
                next_cursor: MissionEventCursor(next),
                has_more: next < projection.last_event_seq,
                compacted_boundary: None,
            },
            segments_read,
        })
    }

    #[cfg(test)]
    fn active_segment_path(&self, projection: &MissionEventProjection) -> Option<PathBuf> {
        (projection.current_segment_start_seq > 0)
            .then(|| self.segment_path(projection.current_segment_start_seq))
    }

    #[cfg(test)]
    fn repair_active_tail(&self, projection: &MissionEventProjection) -> Result<(), String> {
        let Some(path) = self.active_segment_path(projection) else {
            return Ok(());
        };
        let parsed = self.read_segment(projection.current_segment_start_seq, &path, true)?;
        if parsed.actual_bytes != parsed.complete_bytes {
            self.truncate_file(&path, parsed.complete_bytes)?;
        }
        Ok(())
    }

    fn checkpoint_paths(&self) -> Result<Vec<(u64, PathBuf)>, String> {
        match fs::read_dir(&self.root) {
            Ok(entries) => {
                let mut checkpoints = Vec::new();
                for entry in entries {
                    let entry = entry
                        .map_err(|error| format!("Could not read checkpoint entry: {error}"))?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    let Some(revision) = Self::checkpoint_revision_from_name(&name) else {
                        continue;
                    };
                    Self::ensure_regular_or_missing(
                        &entry.path(),
                        "Agent Mission event checkpoint",
                    )?;
                    checkpoints.push((revision, entry.path()));
                }
                checkpoints.sort_by_key(|(revision, _)| *revision);
                Ok(checkpoints)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(format!(
                "Could not list Agent Mission event checkpoints: {error}"
            )),
        }
    }

    fn latest_checkpoint(
        &self,
        mission_id: &MissionId,
    ) -> Result<Option<MissionEventCheckpoint>, String> {
        let Some((_, path)) = self.checkpoint_paths()?.pop() else {
            return Ok(None);
        };
        let recovered = recovery_store::load_json::<MissionEventCheckpoint>(
            &path,
            "Agent Mission event checkpoint",
        )?;
        let Some(checkpoint) = recovered.value else {
            return Ok(None);
        };
        for event in &checkpoint.preserved_events {
            if event.mission_id != *mission_id {
                return Err(
                    "Agent Mission checkpoint contains an event for another Mission.".to_string(),
                );
            }
        }
        Ok(Some(checkpoint))
    }

    fn compact_if_needed(
        &self,
        mission_id: &MissionId,
        projection: &mut MissionEventProjection,
        active_refs: &[u64],
        now: u64,
    ) -> Result<Option<MissionEventCheckpoint>, String> {
        if self.limits.compact_after_closed_segments == usize::MAX {
            return Ok(None);
        }
        let segments = self.segment_paths()?;
        let closed_count = segments.len().saturating_sub(1);
        projection.closed_segment_count = closed_count;
        if closed_count <= self.limits.compact_after_closed_segments {
            return Ok(None);
        }
        let compact_count = closed_count.saturating_sub(self.limits.retain_closed_segments);
        if compact_count == 0 {
            return Ok(None);
        }
        let active_refs: HashSet<u64> = active_refs.iter().copied().collect();
        let previous = self.latest_checkpoint(mission_id)?;
        let mut preserved = previous
            .as_ref()
            .map(|checkpoint| {
                checkpoint
                    .preserved_events
                    .iter()
                    .filter(|event| active_refs.contains(&event.seq))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut compacted_count = previous
            .as_ref()
            .map(|checkpoint| checkpoint.compacted_event_count)
            .unwrap_or(0);
        let mut compacted_through = previous
            .as_ref()
            .map(|checkpoint| checkpoint.compacted_through_seq)
            .unwrap_or(0);

        for (start, path) in segments.iter().take(compact_count) {
            let parsed = self.read_segment(*start, path, false)?;
            compacted_count = compacted_count.saturating_add(parsed.events.len() as u64);
            for event in parsed.events {
                compacted_through = compacted_through.max(event.seq);
                if active_refs.contains(&event.seq) {
                    preserved.push(event);
                }
            }
        }
        preserved.sort_by_key(|event| event.seq);
        preserved.dedup_by_key(|event| event.seq);

        let revision = projection.summary_revision.saturating_add(1).max(1);
        let checkpoint = MissionEventCheckpoint {
            summary_revision: revision,
            compacted_through_seq: compacted_through,
            compacted_event_count: compacted_count,
            preserved_events: preserved,
            created_at: now,
        };
        recovery_store::save_json(
            &self.checkpoint_path(revision),
            &checkpoint,
            "Agent Mission event checkpoint",
        )?;

        for (_, path) in segments.iter().take(compact_count) {
            Self::ensure_regular_or_missing(path, "Agent Mission event segment")?;
            fs::remove_file(path).map_err(|error| {
                format!("Could not remove compacted Agent Mission segment: {error}")
            })?;
        }
        if self.limits.fsync {
            File::open(&self.root)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    format!("Could not fsync Agent Mission event directory: {error}")
                })?;
        }
        for (old_revision, path) in self.checkpoint_paths()? {
            if old_revision != revision {
                let _ = fs::remove_file(path);
            }
        }

        let remaining = self.segment_paths()?;
        projection.summary_revision = revision;
        projection.compacted_through_seq = compacted_through;
        projection.retained_raw_start_seq = remaining
            .first()
            .map(|(start, _)| *start)
            .unwrap_or(projection.last_event_seq.saturating_add(1));
        projection.closed_segment_count = remaining.len().saturating_sub(1);
        Ok(Some(checkpoint))
    }
}

fn validate_event_input(input: &MissionEventInput) -> Result<(), String> {
    let payload = input.payload_summary.trim();
    if payload.chars().count() > MAX_EVENT_PAYLOAD_CHARS || input.payload_summary.contains('\0') {
        return Err(format!(
            "Agent Mission event payload exceeds the {MAX_EVENT_PAYLOAD_CHARS}-character bound or is invalid."
        ));
    }
    if let Some(task_id) = input.task_id.as_deref() {
        if task_id.trim().is_empty()
            || task_id.chars().count() > MAX_EVENT_TASK_ID_CHARS
            || task_id.contains('\0')
        {
            return Err("Agent Mission event task ID is invalid.".to_string());
        }
    }
    if let MissionEventActor::Agent(agent_id) = &input.actor {
        if agent_id.trim().is_empty()
            || agent_id.chars().count() > MAX_EVENT_ACTOR_ID_CHARS
            || agent_id.contains('\0')
        {
            return Err("Agent Mission event actor is invalid.".to_string());
        }
    }
    Ok(())
}

fn validate_event(event: &MissionEvent) -> Result<(), String> {
    if event.seq == 0 || event.authority_revision == 0 {
        return Err(
            "Agent Mission event has an invalid sequence or authority revision.".to_string(),
        );
    }
    validate_event_input(&MissionEventInput {
        kind: event.kind,
        actor: event.actor.clone(),
        task_id: event.task_id.clone(),
        payload_summary: event.payload_summary.clone(),
    })
}

pub(super) fn append_event_snapshot(
    app: &AppHandle,
    mission_id: &MissionId,
    snapshot: &mut super::model::MissionSnapshot,
    input: MissionEventInput,
    now: u64,
) -> Result<MissionEvent, String> {
    if snapshot.mission().id() != mission_id {
        return Err("Agent Mission event target does not match the loaded Mission.".to_string());
    }
    let journal = EventJournal::from_app(app, mission_id)?;
    let authority_revision = snapshot.mission().authority_revision();
    let event = journal.append(
        mission_id,
        authority_revision,
        snapshot.event_projection_mut(),
        input,
        now,
    )?;
    let mut active_refs = snapshot.event_projection().active_event_refs.clone();
    active_refs.extend_from_slice(&snapshot.event_projection().latest_human_directive_seqs);
    active_refs.sort_unstable();
    active_refs.dedup();
    let _ = journal.compact_if_needed(
        mission_id,
        snapshot.event_projection_mut(),
        &active_refs,
        now,
    )?;
    Ok(event)
}

pub(crate) fn events_since(
    app: &AppHandle,
    mission_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<MissionEventPage, String> {
    let mission_id = MissionId::parse(mission_id)?;
    let snapshot = snapshot(app, mission_id.as_str())?;
    EventJournal::from_app(app, &mission_id)?.read_since(
        &mission_id,
        after_seq,
        limit,
        snapshot.event_projection(),
    )
}

pub(super) fn context_event_window(
    app: &AppHandle,
    mission_id: &MissionId,
    projection: &MissionEventProjection,
) -> Result<(Vec<MissionEvent>, u64), String> {
    let journal = EventJournal::from_app(app, mission_id)?;
    if projection.last_event_seq == 0 {
        return Ok((Vec::new(), 0));
    }

    let retained_from = projection.retained_raw_start_seq.max(1);
    let tail_start = projection
        .last_event_seq
        .saturating_sub(128)
        .saturating_add(1);
    let start_seq = retained_from.max(tail_start);
    let page = journal.read_since(
        mission_id,
        start_seq.saturating_sub(1),
        MAX_CURSOR_PAGE,
        projection,
    )?;
    if page.compacted_boundary.is_some() {
        return Err(
            "Agent Mission context tail unexpectedly crossed compacted history.".to_string(),
        );
    }
    let mut events = page.events;
    if let Some(checkpoint) = journal.latest_checkpoint(mission_id)? {
        events.extend(checkpoint.preserved_events);
    }
    events.sort_by_key(|event| event.seq);
    events.dedup_by_key(|event| event.seq);
    if events.len() > MAX_CURSOR_PAGE {
        let drain = events.len() - MAX_CURSOR_PAGE;
        events.drain(0..drain);
    }
    Ok((events, projection.last_event_seq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::model::{
        MissionEventActor, MissionEventKind, MissionEventProjection, MissionId,
    };
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "repotunnel-mission-events-{name}-{}-{nonce:x}-{sequence:x}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create event test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mission_id() -> MissionId {
        MissionId::parse("mission-events-test").expect("mission id")
    }

    fn input(task_id: Option<&str>, payload: &str) -> MissionEventInput {
        MissionEventInput {
            kind: MissionEventKind::Finding,
            actor: MissionEventActor::System,
            task_id: task_id.map(str::to_string),
            payload_summary: payload.to_string(),
        }
    }

    #[test]
    fn event_sequence_is_monotonic_and_cursor_pagination_is_bounded() {
        let directory = TestDirectory::new("pagination");
        let journal = EventJournal::with_limits(
            directory.0.clone(),
            JournalLimits::testing(2, 1_048_576, 64),
        );
        let id = mission_id();
        let mut projection = MissionEventProjection::default();
        for index in 0..5 {
            let event = journal
                .append(
                    &id,
                    1,
                    &mut projection,
                    input(Some("task-a"), &format!("event-{index}")),
                    10_000 + index,
                )
                .expect("append event");
            assert_eq!(event.seq, index + 1);
        }

        let page = journal
            .read_since(&id, 2, 2, &projection)
            .expect("cursor page");
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(page.next_cursor.0, 4);
        assert!(page.has_more);

        let tail = journal
            .read_since(&id, 4, 10, &projection)
            .expect("tail page");
        assert_eq!(tail.events.len(), 1);
        assert_eq!(tail.events[0].seq, 5);
        assert!(!tail.has_more);
        assert!(journal.read_since(&id, 6, 10, &projection).is_err());
    }

    #[test]
    fn payloads_are_bounded_before_append() {
        let directory = TestDirectory::new("payload-bound");
        let journal = EventJournal::with_limits(
            directory.0.clone(),
            JournalLimits::testing(256, 1_048_576, 64),
        );
        let id = mission_id();
        let mut projection = MissionEventProjection::default();
        let error = journal
            .append(
                &id,
                1,
                &mut projection,
                input(None, &"x".repeat(MAX_EVENT_PAYLOAD_CHARS + 1)),
                10_000,
            )
            .expect_err("oversized event payload must fail");
        assert!(error.contains("payload"));
        assert_eq!(projection.last_event_seq, 0);
    }

    #[test]
    fn cursor_read_does_not_parse_full_history() {
        let directory = TestDirectory::new("bounded-read");
        let journal = EventJournal::with_limits(
            directory.0.clone(),
            JournalLimits::testing(10, 1_048_576, usize::MAX),
        );
        let id = mission_id();
        let mut projection = MissionEventProjection::default();
        for index in 0..1_000_u64 {
            journal
                .append(
                    &id,
                    1,
                    &mut projection,
                    input(Some("task-a"), "bounded"),
                    10_000 + index,
                )
                .expect("append event");
        }

        let measured = journal
            .read_since_with_stats(&id, 990, 5, &projection)
            .expect("bounded read");
        assert_eq!(measured.page.events[0].seq, 991);
        assert_eq!(measured.page.events.len(), 5);
        assert!(
            measured.segments_read <= 2,
            "read {} segments",
            measured.segments_read
        );
    }

    #[test]
    fn incomplete_active_tail_is_recovered_but_middle_corruption_fails_closed() {
        let directory = TestDirectory::new("tail-recovery");
        let journal = EventJournal::with_limits(
            directory.0.clone(),
            JournalLimits::testing(256, 1_048_576, 64),
        );
        let id = mission_id();
        let mut projection = MissionEventProjection::default();
        for index in 0..3_u64 {
            journal
                .append(
                    &id,
                    1,
                    &mut projection,
                    input(None, "valid"),
                    10_000 + index,
                )
                .expect("append event");
        }
        let active = journal
            .active_segment_path(&projection)
            .expect("active segment");
        OpenOptions::new()
            .append(true)
            .open(&active)
            .expect("open tail")
            .write_all(br#"{"seq":4,"broken""#)
            .expect("write truncated tail");

        let recovered = journal
            .read_since(&id, 0, 10, &projection)
            .expect("truncated final record ignored");
        assert_eq!(recovered.events.len(), 3);
        journal
            .repair_active_tail(&projection)
            .expect("repair active tail");
        let repaired = fs::read(&active).expect("read repaired segment");
        assert!(repaired.ends_with(b"\n"));

        let mut content = String::from_utf8(repaired).expect("utf8 journal");
        let first_newline = content.find('\n').expect("first line");
        content.insert_str(first_newline + 1, "{corrupt-middle}\n");
        fs::write(&active, content).expect("inject middle corruption");
        assert!(journal.read_since(&id, 0, 10, &projection).is_err());
    }

    #[test]
    fn journal_ahead_of_committed_snapshot_rolls_back_orphan_event_in_active_segment() {
        let directory = TestDirectory::new("orphan-active");
        let journal = EventJournal::with_limits(
            directory.0.clone(),
            JournalLimits::testing(256, 1_048_576, usize::MAX),
        );
        let id = mission_id();
        let mut committed = MissionEventProjection::default();
        journal
            .append(&id, 1, &mut committed, input(None, "committed"), 10_000)
            .expect("append committed event");

        let mut uncommitted = committed.clone();
        journal
            .append(
                &id,
                1,
                &mut uncommitted,
                input(None, "orphan-after-snapshot-save-failure"),
                10_001,
            )
            .expect("append orphan event");
        assert_eq!(uncommitted.last_event_seq, 2);

        journal
            .reconcile_projection(&mut committed)
            .expect("recover committed projection");
        assert_eq!(
            committed.last_event_seq, 1,
            "journal-only event must not become committed Mission history"
        );
        let segments = journal.segment_paths().expect("segments");
        assert_eq!(segments.len(), 1);
        let parsed = journal
            .read_segment(segments[0].0, &segments[0].1, true)
            .expect("read recovered segment");
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].payload_summary, "committed");
    }

    #[test]
    fn journal_ahead_of_committed_snapshot_removes_orphan_rotated_segment() {
        let directory = TestDirectory::new("orphan-rotated");
        let journal = EventJournal::with_limits(
            directory.0.clone(),
            JournalLimits::testing(1, 1_048_576, usize::MAX),
        );
        let id = mission_id();
        let mut committed = MissionEventProjection::default();
        journal
            .append(&id, 1, &mut committed, input(None, "committed"), 10_000)
            .expect("append committed event");

        let mut uncommitted = committed.clone();
        journal
            .append(
                &id,
                1,
                &mut uncommitted,
                input(None, "orphan-rotated"),
                10_001,
            )
            .expect("append orphan rotated event");
        assert_eq!(journal.segment_paths().expect("segments").len(), 2);

        journal
            .reconcile_projection(&mut committed)
            .expect("recover committed projection");
        assert_eq!(committed.last_event_seq, 1);
        let segments = journal.segment_paths().expect("segments");
        assert_eq!(segments.len(), 1, "orphan rotated segment must be removed");
        let parsed = journal
            .read_segment(segments[0].0, &segments[0].1, true)
            .expect("read committed segment");
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].seq, 1);
    }

    #[test]
    fn recovery_adopts_durable_compaction_checkpoint_when_snapshot_save_never_committed() {
        let directory = TestDirectory::new("compaction-crash");
        let journal =
            EventJournal::with_limits(directory.0.clone(), JournalLimits::testing(1, 1_048_576, 2));
        let id = mission_id();
        let mut committed = MissionEventProjection::default();
        for index in 0..3_u64 {
            journal
                .append(
                    &id,
                    1,
                    &mut committed,
                    input(None, &format!("committed-{index}")),
                    10_000 + index,
                )
                .expect("append committed event");
        }
        assert_eq!(committed.last_event_seq, 3);
        assert_eq!(committed.summary_revision, 0);

        let mut uncommitted = committed.clone();
        journal
            .append(
                &id,
                1,
                &mut uncommitted,
                input(None, "orphan-triggering-compaction"),
                10_003,
            )
            .expect("append orphan event");
        let checkpoint = journal
            .compact_if_needed(&id, &mut uncommitted, &[], 20_000)
            .expect("compact before simulated snapshot failure")
            .expect("checkpoint created");
        assert_eq!(checkpoint.compacted_through_seq, 2);
        assert_eq!(journal.segment_paths().expect("compacted segments")[0].0, 3);

        journal
            .reconcile_projection(&mut committed)
            .expect("recover old committed snapshot against compacted journal");
        assert_eq!(committed.last_event_seq, 3);
        assert_eq!(committed.summary_revision, checkpoint.summary_revision);
        assert_eq!(committed.compacted_through_seq, 2);
        assert_eq!(committed.retained_raw_start_seq, 3);

        let page = journal
            .read_since(&id, 0, 10, &committed)
            .expect("old cursor receives explicit compaction boundary");
        let boundary = page.compacted_boundary.expect("compaction boundary");
        assert_eq!(boundary.compacted_through_seq, 2);
        assert_eq!(boundary.retained_from_seq, 3);
    }

    #[test]
    fn compaction_writes_checkpoint_and_returns_explicit_old_cursor_boundary() {
        let directory = TestDirectory::new("compaction");
        let journal =
            EventJournal::with_limits(directory.0.clone(), JournalLimits::testing(1, 1_048_576, 4));
        let id = mission_id();
        let mut projection = MissionEventProjection::default();
        for index in 0..8_u64 {
            journal
                .append(
                    &id,
                    1,
                    &mut projection,
                    input(Some("task-protected"), &format!("event-{index}")),
                    10_000 + index,
                )
                .expect("append event");
        }

        let compacted = journal
            .compact_if_needed(&id, &mut projection, &[2], 20_000)
            .expect("compact")
            .expect("checkpoint written");
        assert!(compacted.compacted_through_seq >= 1);
        assert!(compacted
            .preserved_events
            .iter()
            .any(|event| event.seq == 2));
        assert!(projection.summary_revision > 0);
        assert!(projection.retained_raw_start_seq > 1);

        let page = journal
            .read_since(&id, 0, 10, &projection)
            .expect("boundary page");
        let boundary = page.compacted_boundary.expect("explicit boundary");
        assert_eq!(boundary.summary_revision, projection.summary_revision);
        assert_eq!(
            boundary.retained_from_seq,
            projection.retained_raw_start_seq
        );
        assert!(page.events.is_empty());
    }

    #[test]
    fn synthetic_five_thousand_event_tail_cursor_remains_correct() {
        let directory = TestDirectory::new("five-thousand");
        let journal = EventJournal::with_limits(
            directory.0.clone(),
            JournalLimits::testing(256, 1_048_576, usize::MAX),
        );
        let id = mission_id();
        let mut projection = MissionEventProjection::default();
        for index in 0..5_000_u64 {
            journal
                .append(
                    &id,
                    1,
                    &mut projection,
                    input(Some("task-scale"), "scale"),
                    100_000 + index,
                )
                .expect("append scale event");
        }
        let measured = journal
            .read_since_with_stats(&id, 4_990, 20, &projection)
            .expect("tail cursor");
        assert_eq!(
            measured.page.events.first().map(|event| event.seq),
            Some(4_991)
        );
        assert_eq!(
            measured.page.events.last().map(|event| event.seq),
            Some(5_000)
        );
        assert!(measured.segments_read <= 2);
    }
}
