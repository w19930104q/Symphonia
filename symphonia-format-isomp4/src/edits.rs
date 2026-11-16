// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Track edit list support.
//!
//! An edit list (`edts`/`elst`) maps a track's presentation timeline (the timeline exposed by the
//! demuxer) onto the media timeline defined by the track's sample tables. The edit list atom stores
//! edit durations in the movie (`mvhd`) timescale while media times are in the media (`mdhd`)
//! timescale, so all entries are normalized to the media timescale when an [`EditList`] is
//! constructed.
//!
//! Two kinds of edits are supported:
//!
//! * **Media edits** play a contiguous range of the media. Multiple media edits may refer to
//!   overlapping or repeated ranges, which gives the presentation timeline a looping semantics.
//! * **Empty edits** (an `elst` entry with a media time of -1) present no media and are rendered as
//!   silence for audio tracks or a blank frame for video tracks. The demuxer emits no packets for
//!   the duration of an empty edit; the gap is simply reflected in the presentation timestamps of
//!   the surrounding packets.
//!
//! Only edits playing the media at the normal rate (1.0) are supported.

use std::sync::Arc;

use symphonia_core::errors::Result;
use symphonia_core::units::Duration;

use crate::atoms::ElstAtom;

/// A single edit normalized to the media (mdhd) timescale.
#[derive(Copy, Clone, Debug)]
pub struct Edit {
    /// The start of the edit on the presentation timeline, in media timescale units.
    pub start_ts: u64,
    /// The duration of the edit on the presentation timeline, in media timescale units.
    pub duration: u64,
    /// The media time corresponding to the start of the edit, in media timescale units. An
    /// [`Option::None`] indicates an empty edit.
    pub media_time: Option<u64>,
}

impl Edit {
    /// Returns `true` if the edit is an empty (silence/blank) edit.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.media_time.is_none()
    }

    /// The end of the edit on the presentation timeline (exclusive), in media timescale units.
    pub fn end_ts(&self) -> u64 {
        self.start_ts + self.duration
    }
}

/// A normalized edit list for a single track. All timestamps and durations are expressed in the
/// media (mdhd) timescale of the owning track.
#[derive(Debug)]
pub struct EditList {
    edits: Vec<Edit>,
    /// The total duration of the presentation timeline, in media timescale units.
    duration: u64,
}

impl EditList {
    /// Constructs an edit list from the raw `elst` atom.
    ///
    /// `movie_timescale` is the timescale of the `mvhd` atom (the edit durations are expressed in
    /// it), `media_timescale` is the timescale of the track's `mdhd` atom, and `media_duration` is
    /// the total duration of the track's media in media timescale units.
    ///
    /// Returns `None` if the edit list should be ignored, either because it is empty, contains only
    /// entries that cannot be represented, or because it uses unsupported features (such as a
    /// playback rate other than 1.0).
    pub fn from_elst(
        elst: &ElstAtom,
        movie_timescale: u32,
        media_timescale: u32,
        media_duration: u64,
    ) -> Option<EditList> {
        let mut edits: Vec<Edit> = Vec::with_capacity(elst.entries.len());
        let mut start_ts: u64 = 0;

        for entry in &elst.entries {
            // Edits with a zero duration occupy no presentation time and are skipped.
            if entry.segment_duration == 0 {
                continue;
            }

            // Only the normal playback rate (1.0) is supported.
            if !entry.is_normal_rate() {
                log::warn!(
                    "isomp4 (elst): non-unit media rates are not supported, ignoring edits."
                );
                return None;
            }

            let duration =
                scale_movie_to_media(entry.segment_duration, movie_timescale, media_timescale)?;

            let media_time = if entry.is_empty_edit() {
                // An empty edit presents silence/a blank frame.
                None
            }
            else if entry.media_time < 0 {
                // A negative media time other than -1 is not defined by the specification. Treat
                // the portion of the edit preceding the start of the media as an empty edit, which
                // matches the semantics defined for encoder delay.
                let delay = scale_movie_to_media(
                    entry.media_time.unsigned_abs(),
                    movie_timescale,
                    media_timescale,
                )?;

                if delay >= duration {
                    // The entire edit is before the start of the media.
                    push_edit(&mut edits, &mut start_ts, duration, None)?;
                    continue;
                }

                // The leading part of the edit is empty, the remainder plays the media from the
                // start.
                let empty_duration = delay;
                let media_duration_part = duration - delay;

                push_edit(&mut edits, &mut start_ts, empty_duration, None)?;
                push_edit(&mut edits, &mut start_ts, media_duration_part, Some(0))?;
                continue;
            }
            else {
                Some(entry.media_time as u64)
            };

            // Clamp media edits to the actual extent of the media. An edit that starts at or beyond
            // the end of the media behaves like an empty edit.
            let media_time = match media_time {
                Some(media_time) if media_time < media_duration => Some(media_time),
                _ => None,
            };

            let duration = match media_time {
                Some(media_time) => duration.min(media_duration - media_time),
                None => duration,
            };

            if duration == 0 {
                continue;
            }

            push_edit(&mut edits, &mut start_ts, duration, media_time)?;
        }

        if edits.is_empty() {
            return None;
        }

        Some(EditList { edits, duration: start_ts })
    }

    /// The normalized edits, ordered by their start on the presentation timeline.
    #[allow(dead_code)]
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// The total duration of the presentation timeline in media timescale units.
    pub fn duration(&self) -> Duration {
        Duration::new(self.duration)
    }

    /// The presentation timestamp of the first media sample emitted by the edit list, in media
    /// timescale units. Leading empty edits occupy presentation time but emit no samples, so the
    /// returned timestamp is the start of the first media edit. Returns 0 when the edit list
    /// contains no media edit.
    pub fn first_presentation_ts(&self) -> u64 {
        self.edits
            .iter()
            .find(|edit| edit.media_time.is_some())
            .map(|edit| edit.start_ts)
            .unwrap_or(0)
    }

    /// Maps a presentation timestamp (in media timescale units) to the corresponding media
    /// timestamp and the index of the edit containing it.
    ///
    /// If the timestamp falls within an empty edit, the edit index is still returned but the media
    /// timestamp is [`Option::None`]. Returns `None` if the timestamp is at or beyond the end of
    /// the presentation timeline.
    pub fn map_presentation_ts(&self, pts: u64) -> Option<(usize, Option<u64>)> {
        let (idx, edit) = self.edits.iter().enumerate().find(|(_, edit)| pts < edit.end_ts())?;

        let media_ts = edit.media_time.map(|media_time| {
            let offset = pts - edit.start_ts;
            media_time + offset
        });

        Some((idx, media_ts))
    }
}

fn push_edit(
    edits: &mut Vec<Edit>,
    start_ts: &mut u64,
    duration: u64,
    media_time: Option<u64>,
) -> Option<()> {
    let next_start = start_ts.checked_add(duration)?;

    edits.push(Edit { start_ts: *start_ts, duration, media_time });
    *start_ts = next_start;

    Some(())
}

/// Converts a duration expressed in movie timescale units into media timescale units, rounding to
/// the nearest whole tick. Returns `None` on overflow.
fn scale_movie_to_media(duration: u64, movie_timescale: u32, media_timescale: u32) -> Option<u64> {
    // Round-half-up: (duration * media_timescale + movie_timescale / 2) / movie_timescale.
    let scaled = u128::from(duration)
        .checked_mul(u128::from(media_timescale))?
        .checked_add(u128::from(movie_timescale / 2))?
        / u128::from(movie_timescale);

    u64::try_from(scaled).ok()
}

/// A cursor tracking playback position within an [`EditList`].
///
/// The cursor assumes the media timeline is traversed in order. When the media is played more than
/// once by successive (looping) edits, the media timeline restarts at 0 while the presentation
/// timeline keeps advancing; the cursor detects such a restart and moves to the next edit.
#[derive(Debug)]
pub struct EditCursor {
    edits: Arc<EditList>,
    /// The index of the edit containing the next sample to be emitted. Empty edits are skipped
    /// over by advancing this index.
    edit_idx: usize,
    /// The index of the edit targeted by the most recent seek. Unlike `edit_idx`, it may point at
    /// an empty edit; it is used to map the seeked media timestamp back onto the presentation
    /// timeline. Reset to `edit_idx` once playback resumes.
    seek_edit_idx: Option<usize>,
    /// The media timestamp of the previously mapped sample, used to detect a media timeline restart
    /// caused by a looping edit.
    last_media_ts: Option<u64>,
    /// The index of the edit that presented the previously mapped sample. When the media timeline
    /// restarts, playback resumes at the edit following this one; empty edits in between are then
    /// skipped by the main loop.
    last_presented_edit_idx: Option<usize>,
}

impl EditCursor {
    /// Instantiates a new cursor over the provided edit list, positioned at the start.
    pub fn new(edits: Arc<EditList>) -> Self {
        EditCursor {
            edits,
            edit_idx: 0,
            seek_edit_idx: None,
            last_media_ts: None,
            last_presented_edit_idx: None,
        }
    }

    /// Returns `true` if all edits have been consumed.
    pub fn is_at_end(&self) -> bool {
        self.edit_idx >= self.edits.edits.len()
    }

    /// The total duration of the edit list in media timescale units.
    pub fn duration(&self) -> Duration {
        self.edits.duration()
    }

    /// Advances the cursor to the edit containing the given presentation timestamp. Empty edits
    /// are skipped over. Returns the media timestamp to seek the media to, or `None` if the
    /// presentation timeline ends at or before the requested timestamp.
    ///
    /// The previous-sample baseline used to detect a media timeline restart is cleared: a seek
    /// establishes a new playback position, so the next mapped sample must not be mistaken for a
    /// looping restart. When the target lies within an empty edit, the media is positioned at the
    /// start of the following media edit, while the empty edit still governs the reported
    /// presentation timestamp.
    pub fn seek_to_presentation_ts(&mut self, pts: u64) -> Option<u64> {
        let (idx, media_ts) = self.edits.map_presentation_ts(pts)?;

        // Record the edit that actually contains the requested presentation timestamp. This is the
        // edit used to report the actual seeked-to presentation timestamp.
        self.seek_edit_idx = Some(idx);
        self.last_media_ts = None;
        self.last_presented_edit_idx = None;

        match media_ts {
            // The target lies inside a media edit. Position the playback cursor on it.
            Some(media_ts) => {
                self.edit_idx = idx;
                Some(media_ts)
            }
            // The target lies inside an empty edit. Position the playback cursor on the next media
            // edit and seek the media to its start.
            None => {
                self.edit_idx = idx;
                self.edits.edits[idx..].iter().position(|edit| edit.media_time.is_some()).map(
                    |offset| {
                        let media_idx = idx + offset;
                        self.edit_idx = media_idx;
                        self.edits.edits[media_idx]
                            .media_time
                            .expect("the located edit is a media edit")
                    },
                )
            }
        }
    }

    /// Maps a media timestamp to the presentation timestamp of the edit targeted by the most
    /// recent seek, or the edit the playback cursor currently points at when no seek is pending.
    ///
    /// This is the inverse of [`EditList::map_presentation_ts`] and is used after a seek to report
    /// the actual presentation timestamp of the located sample. When the seek targeted an empty
    /// edit, the media was positioned at the start of the following media edit (the edit the
    /// playback cursor points at), so that edit provides the mapping.
    pub fn seek_map_media_ts(&self, media_ts: u64) -> Option<(usize, Option<u64>)> {
        let idx = self.seek_edit_idx.unwrap_or(self.edit_idx);
        let edit = self.edits.edits.get(idx)?;

        match edit.media_time {
            Some(edit_media_start) => {
                let presentation_ts = edit.start_ts + media_ts.saturating_sub(edit_media_start);
                Some((idx, Some(presentation_ts)))
            }
            None => {
                // The seek targeted an empty edit. The media timeline was positioned at the media
                // edit the playback cursor now points at; map through that edit instead.
                let edit = self.edits.edits.get(self.edit_idx)?;
                let presentation_ts = edit.media_time.map(|edit_media_start| {
                    edit.start_ts + media_ts.saturating_sub(edit_media_start)
                });
                Some((self.edit_idx, presentation_ts))
            }
        }
    }

    /// Given the media timestamp of the next sample available on the media timeline, adjusts the
    /// cursor so that it points at the edit containing that sample, skipping any edits that end at
    /// or before the sample.
    ///
    /// Returns the presentation timestamp at which the sample should be emitted, or `None` if the
    /// sample is not part of the presentation timeline (for example, it belongs to a skipped
    /// region) or the end of the edit list has been reached.
    pub fn presentation_ts_for_media_ts(&mut self, media_ts: u64) -> Option<u64> {
        // Playback has resumed after a seek: the seek-specific edit index is no longer needed.
        self.seek_edit_idx = None;

        // A media timestamp that is not greater than the previously mapped one means the media
        // timeline restarted. This happens when a looping edit replays the media: resume at the
        // edit following the one that presented the previous sample. Empty edits in between are
        // skipped by the loop below.
        if let Some(last_media_ts) = self.last_media_ts {
            if media_ts <= last_media_ts {
                self.edit_idx =
                    self.last_presented_edit_idx.map(|idx| idx + 1).unwrap_or(self.edit_idx + 1);
            }
        }

        loop {
            let edit = self.edits.edits.get(self.edit_idx)?;

            match edit.media_time {
                // Empty edit: skip to the next edit. Empty edits emit no packets.
                None => {
                    self.edit_idx += 1;
                    continue;
                }
                Some(edit_media_start) => {
                    let edit_media_end = edit_media_start + edit.duration;

                    if media_ts < edit_media_start {
                        // The sample precedes the start of the current edit. It is not presented.
                        return None;
                    }

                    if media_ts >= edit_media_end {
                        // The sample is beyond the end of the current edit. Advance to the next
                        // edit and re-evaluate since the media may be played again (looping).
                        self.edit_idx += 1;
                        continue;
                    }

                    let offset = media_ts - edit_media_start;
                    self.last_media_ts = Some(media_ts);
                    self.last_presented_edit_idx = Some(self.edit_idx);
                    return Some(edit.start_ts + offset);
                }
            }
        }
    }

    /// Clamps the duration of a sample so that a packet never extends past the end of the edit
    /// containing it. Returns the original duration if the sample is fully within the edit.
    pub fn clamp_sample_duration(&self, media_ts: u64, duration: u64) -> u64 {
        match self
            .edits
            .edits
            .get(self.edit_idx)
            .and_then(|edit| edit.media_time.map(|edit_media_start| (edit, edit_media_start)))
        {
            Some((edit, edit_media_start))
                if media_ts >= edit_media_start && media_ts < edit_media_start + edit.duration =>
            {
                let remaining = edit_media_start + edit.duration - media_ts;
                duration.min(remaining)
            }
            _ => duration,
        }
    }
}

/// Constructs an [`EditList`] for a track from its optional `edts` atom. Returns `None` when the
/// track has no edit list or the edit list cannot be applied.
pub fn edit_list_for_track(
    edts: Option<&crate::atoms::EdtsAtom>,
    movie_timescale: u32,
    media_timescale: u32,
    media_duration: u64,
) -> Result<Option<Arc<EditList>>> {
    let Some(edts) = edts
    else {
        return Ok(None);
    };

    let Some(elst) = edts.elst.as_ref()
    else {
        return Ok(None);
    };

    match EditList::from_elst(elst, movie_timescale, media_timescale, media_duration) {
        Some(edit_list) => Ok(Some(Arc::new(edit_list))),
        None => {
            log::warn!("isomp4 (elst): edit list could not be applied, ignoring.");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atoms::ElstEntry;

    fn elst_entry(segment_duration: u64, media_time: i64) -> ElstEntry {
        ElstEntry { segment_duration, media_time, media_rate_int: 1, media_rate_frac: 0 }
    }

    fn elst(entries: Vec<ElstEntry>) -> ElstAtom {
        ElstAtom { entries }
    }

    #[test]
    fn verify_empty_edit_then_media() {
        // Movie timescale = 1000, media timescale = 1000 (1:1). A 1000 tick empty edit followed by
        // a 4000 tick media edit starting at media time 0.
        let atom = elst(vec![elst_entry(1000, -1), elst_entry(4000, 0)]);

        let edits = EditList::from_elst(&atom, 1000, 1000, 4000).expect("should build");

        assert_eq!(edits.duration().get(), 5000);
        assert_eq!(edits.edits().len(), 2);
        assert!(edits.edits()[0].is_empty());
        assert_eq!(edits.edits()[0].start_ts, 0);
        assert_eq!(edits.edits()[0].duration, 1000);
        assert_eq!(edits.edits()[1].media_time, Some(0));
        assert_eq!(edits.edits()[1].start_ts, 1000);
        assert_eq!(edits.edits()[1].duration, 4000);

        // The first media sample is presented at the start of the media edit, not at timestamp 0.
        assert_eq!(edits.first_presentation_ts(), 1000);

        // The first media sample (media time 0) is presented at tick 1000.
        let mut cursor = EditCursor::new(Arc::new(edits));
        assert_eq!(cursor.presentation_ts_for_media_ts(0), Some(1000));
        assert_eq!(cursor.presentation_ts_for_media_ts(100), Some(1100));
    }

    #[test]
    fn verify_looping_edit() {
        // Play the full 2000 tick media twice.
        let atom = elst(vec![elst_entry(2000, 0), elst_entry(2000, 0)]);

        let edits = EditList::from_elst(&atom, 1000, 1000, 2000).expect("should build");

        assert_eq!(edits.duration().get(), 4000);

        let mut cursor = EditCursor::new(Arc::new(edits));

        // First play-through.
        assert_eq!(cursor.presentation_ts_for_media_ts(0), Some(0));
        assert_eq!(cursor.presentation_ts_for_media_ts(500), Some(500));

        // Second play-through: the same media times map to the second edit.
        assert_eq!(cursor.presentation_ts_for_media_ts(0), Some(2000));
        assert_eq!(cursor.presentation_ts_for_media_ts(500), Some(2500));

        // The end of the edit list is reached after the second play-through.
        assert_eq!(cursor.presentation_ts_for_media_ts(2000), None);
    }

    #[test]
    fn verify_looping_edit_with_empty_edit_between() {
        // Play the full 2000 tick media, present 500 ticks of silence, then replay the media. The
        // restart detection must resume at the media edit following the empty one.
        let atom = elst(vec![elst_entry(2000, 0), elst_entry(500, -1), elst_entry(2000, 0)]);

        let edits = EditList::from_elst(&atom, 1000, 1000, 2000).expect("should build");

        assert_eq!(edits.duration().get(), 4500);

        let mut cursor = EditCursor::new(Arc::new(edits));

        // First play-through.
        assert_eq!(cursor.presentation_ts_for_media_ts(0), Some(0));
        assert_eq!(cursor.presentation_ts_for_media_ts(1500), Some(1500));

        // The sample at the end of the media is beyond the first edit.
        assert_eq!(cursor.presentation_ts_for_media_ts(2000), None);

        // Second play-through after the empty edit: the timeline restarts and the empty edit is
        // skipped, so media time 0 maps to presentation tick 2500.
        assert_eq!(cursor.presentation_ts_for_media_ts(0), Some(2500));
        assert_eq!(cursor.presentation_ts_for_media_ts(1000), Some(3500));

        // The end of the edit list is reached after the second play-through.
        assert_eq!(cursor.presentation_ts_for_media_ts(2000), None);
    }

    #[test]
    fn verify_timescale_conversion() {
        // Movie timescale 1000, media timescale 48000. A 1 second (1000 movie tick) edit maps to
        // 48000 media ticks.
        let atom = elst(vec![elst_entry(1000, 0)]);

        let edits = EditList::from_elst(&atom, 1000, 48000, 48000).expect("should build");

        assert_eq!(edits.duration().get(), 48000);
        assert_eq!(edits.edits()[0].duration, 48000);
    }

    #[test]
    fn verify_edit_clamped_to_media() {
        // The edit requests 5000 ticks of media starting at 3000, but the media only has 4000
        // ticks. The edit must be clamped to 1000 ticks.
        let atom = elst(vec![elst_entry(5000, 3000)]);

        let edits = EditList::from_elst(&atom, 1000, 1000, 4000).expect("should build");

        assert_eq!(edits.edits().len(), 1);
        assert_eq!(edits.edits()[0].media_time, Some(3000));
        assert_eq!(edits.edits()[0].duration, 1000);
        assert_eq!(edits.duration().get(), 1000);
    }

    #[test]
    fn verify_edit_starting_past_media_is_empty() {
        // An edit starting beyond the end of the media behaves as an empty edit.
        let atom = elst(vec![elst_entry(1000, 4000)]);

        let edits = EditList::from_elst(&atom, 1000, 1000, 4000).expect("should build");

        assert_eq!(edits.edits().len(), 1);
        assert!(edits.edits()[0].is_empty());
        assert_eq!(edits.duration().get(), 1000);
    }

    #[test]
    fn verify_unsupported_rate_ignored() {
        let mut entry = elst_entry(1000, 0);
        entry.media_rate_int = 2;

        let atom = elst(vec![entry]);
        assert!(EditList::from_elst(&atom, 1000, 1000, 1000).is_none());
    }

    #[test]
    fn verify_map_presentation_ts() {
        let atom = elst(vec![elst_entry(1000, -1), elst_entry(2000, 500), elst_entry(2000, 0)]);

        let edits = EditList::from_elst(&atom, 1000, 1000, 2000).expect("should build");

        // Note, the first media edit is clamped by the extent of the media: it starts at media
        // time 500 and the media ends at 2000, so its duration is reduced from 2000 to 1500. The
        // presentation timeline is therefore: empty [0, 1000), edit1 [1000, 2500) and edit2
        // [2500, 4500).

        // Inside the empty edit.
        assert_eq!(edits.map_presentation_ts(500), Some((0, None)));
        // Inside the first media edit: media time = 500 + (1500 - 1000).
        assert_eq!(edits.map_presentation_ts(1500), Some((1, Some(1000))));
        // Inside the looping media edit: media time = 0 + (3500 - 2500).
        assert_eq!(edits.map_presentation_ts(3500), Some((2, Some(1000))));
        // Past the end.
        assert_eq!(edits.map_presentation_ts(4500), None);
    }

    #[test]
    fn verify_clamp_sample_duration() {
        let atom = elst(vec![elst_entry(1000, 0)]);
        let edits = Arc::new(EditList::from_elst(&atom, 1000, 1000, 1000).expect("should build"));
        let cursor = EditCursor::new(edits);

        // A sample starting at 900 with a duration of 200 must be clamped to the 100 ticks left
        // in the edit.
        assert_eq!(cursor.clamp_sample_duration(900, 200), 100);
        // A fully contained sample is returned unchanged.
        assert_eq!(cursor.clamp_sample_duration(100, 200), 200);
    }
}
