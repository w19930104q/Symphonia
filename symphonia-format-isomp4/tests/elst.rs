// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for edit list (`edts`/`elst`) handling.
//!
//! The test files are synthesized in-memory by a minimal MP4 writer. Each file contains a single
//! audio track with a fixed number of constant-size samples and a configurable edit list, allowing
//! the asserted durations, timestamps and seek positions to be computed exactly.

use std::io::Cursor;

use symphonia_core::formats::prelude::*;
use symphonia_core::io::MediaSourceStream;
use symphonia_format_isomp4::IsoMp4Reader;

/// The movie (mvhd) and media (mdhd) timescale used by all test files. Using the same timescale for
/// both avoids any timescale conversion in the edit list.
const TIMESCALE: u32 = 1000;
/// The number of samples in the synthesized media.
const SAMPLE_COUNT: u32 = 10;
/// The duration of each sample in timescale ticks.
const SAMPLE_DUR: u32 = 100;
/// The size in bytes of each sample.
const SAMPLE_SIZE: u32 = 4;
/// The total duration of the media in timescale ticks.
const MEDIA_DURATION: u64 = SAMPLE_COUNT as u64 * SAMPLE_DUR as u64;

/// Wraps a payload in a version-0 MP4 box.
fn box4(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let len = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

/// Concatenates the provided boxes into a single payload buffer.
fn concat(boxes: &[Vec<u8>]) -> Vec<u8> {
    boxes.concat()
}

/// Builds a full-box header (version + flags).
fn full_header(version: u8, flags: u32) -> [u8; 4] {
    [version, ((flags >> 16) & 0xff) as u8, ((flags >> 8) & 0xff) as u8, (flags & 0xff) as u8]
}

/// Builds an `elst` box from raw `(segment_duration, media_time)` pairs. The media rate is always
/// 1.0.
fn elst_box(entries: &[(u64, i64)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    for &(segment_duration, media_time) in entries {
        payload.extend_from_slice(&(segment_duration as u32).to_be_bytes());
        payload.extend_from_slice(&(media_time as i32).to_be_bytes());
        // Media rate: integer part 1, fractional part 0.
        payload.extend_from_slice(&1i16.to_be_bytes());
        payload.extend_from_slice(&0i16.to_be_bytes());
    }

    box4(b"elst", &payload)
}

/// Builds the `ftyp` box.
fn ftyp_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"mp42"); // Major brand.
    payload.extend_from_slice(&0u32.to_be_bytes()); // Minor version.
    payload.extend_from_slice(b"mp42"); // Compatible brand.
    box4(b"ftyp", &payload)
}

/// Builds the `mvhd` box with the given duration.
fn mvhd_box(duration: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&0u32.to_be_bytes()); // Creation time.
    payload.extend_from_slice(&0u32.to_be_bytes()); // Modification time.
    payload.extend_from_slice(&TIMESCALE.to_be_bytes());
    payload.extend_from_slice(&(duration as u32).to_be_bytes());
    payload.extend_from_slice(&0x00010000u32.to_be_bytes()); // Preferred rate: 1.0.
    payload.extend_from_slice(&0x0100u16.to_be_bytes()); // Preferred volume: 1.0.
    payload.extend_from_slice(&[0u8; 10]); // Reserved.
    box4(b"mvhd", &payload)
}

/// Builds the `tkhd` box with the given duration.
fn tkhd_box(duration: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 3)); // Track enabled + in movie.
    payload.extend_from_slice(&0u32.to_be_bytes()); // Creation time.
    payload.extend_from_slice(&0u32.to_be_bytes()); // Modification time.
    payload.extend_from_slice(&1u32.to_be_bytes()); // Track ID.
    payload.extend_from_slice(&0u32.to_be_bytes()); // Reserved.
    payload.extend_from_slice(&(duration as u32).to_be_bytes());
    payload.extend_from_slice(&[0u8; 8]); // Reserved.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Layer.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Alternate group.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Volume.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Reserved.
    box4(b"tkhd", &payload)
}

/// Builds the `mdhd` box.
fn mdhd_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&0u32.to_be_bytes()); // Creation time.
    payload.extend_from_slice(&0u32.to_be_bytes()); // Modification time.
    payload.extend_from_slice(&TIMESCALE.to_be_bytes());
    payload.extend_from_slice(&(MEDIA_DURATION as u32).to_be_bytes());
    payload.extend_from_slice(&0x55c4u16.to_be_bytes()); // Language: "und".
    payload.extend_from_slice(&0u16.to_be_bytes()); // Quality.
    box4(b"mdhd", &payload)
}

/// Builds the `hdlr` box for a sound track.
fn hdlr_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&0u32.to_be_bytes()); // Pre-defined.
    payload.extend_from_slice(b"soun"); // Handler type.
    payload.extend_from_slice(&[0u8; 12]); // Reserved.
    payload.extend_from_slice(b"SoundHandler\0"); // Name.
    box4(b"hdlr", &payload)
}

/// Builds the `smhd` box.
fn smhd_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&0u16.to_be_bytes()); // Balance.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Reserved.
    box4(b"smhd", &payload)
}

/// Builds an MP3 audio sample entry (`mp4a` without a codec-specific box). The demuxer assigns the
/// MP3 codec to such entries.
fn mp4a_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&[0u8; 6]); // Reserved.
    payload.extend_from_slice(&1u16.to_be_bytes()); // Data reference index.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Version.
    payload.extend_from_slice(&[0u8; 6]); // Revision + vendor.
    payload.extend_from_slice(&2u16.to_be_bytes()); // Number of channels.
    payload.extend_from_slice(&16u16.to_be_bytes()); // Sample size.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Compression ID.
    payload.extend_from_slice(&0u16.to_be_bytes()); // Packet size.
    // Sample rate as a 16.16 fixed point value.
    payload.extend_from_slice(&(TIMESCALE << 16).to_be_bytes());
    box4(b".mp3", &payload)
}

/// Builds the `stsd` box containing a single sample entry.
fn stsd_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&1u32.to_be_bytes()); // Entry count.
    payload.extend_from_slice(&mp4a_box());
    box4(b"stsd", &payload)
}

/// Builds the `stts` box describing constant-duration samples.
fn stts_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&1u32.to_be_bytes()); // Entry count.
    payload.extend_from_slice(&SAMPLE_COUNT.to_be_bytes());
    payload.extend_from_slice(&SAMPLE_DUR.to_be_bytes());
    box4(b"stts", &payload)
}

/// Builds the `stsc` box with one samples-per-chunk entry.
fn stsc_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&1u32.to_be_bytes()); // Entry count.
    payload.extend_from_slice(&1u32.to_be_bytes()); // First chunk (1-based).
    payload.extend_from_slice(&SAMPLE_COUNT.to_be_bytes()); // Samples per chunk.
    payload.extend_from_slice(&1u32.to_be_bytes()); // Sample description index.
    box4(b"stsc", &payload)
}

/// Builds the `stsz` box with a constant sample size.
fn stsz_box() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&SAMPLE_SIZE.to_be_bytes()); // Constant sample size.
    payload.extend_from_slice(&SAMPLE_COUNT.to_be_bytes());
    box4(b"stsz", &payload)
}

/// Builds the `stco` box with a single chunk offset.
fn stco_box(offset: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&full_header(0, 0));
    payload.extend_from_slice(&1u32.to_be_bytes()); // Entry count.
    payload.extend_from_slice(&offset.to_be_bytes());
    box4(b"stco", &payload)
}

/// Builds a complete MP4 file with the given edit list entries. Returns the serialized file and
/// the offset of the `mdat` payload.
fn build_mp4(elst_entries: &[(u64, i64)]) -> (Vec<u8>, u32) {
    let ftyp = ftyp_box();

    // The edit list duration (in movie ticks) is the sum of all entry durations.
    let edit_duration: u64 = elst_entries.iter().map(|(dur, _)| *dur).sum();
    // When no edits are present, the movie spans the full media.
    let movie_duration = if edit_duration == 0 { MEDIA_DURATION } else { edit_duration };

    let mvhd = mvhd_box(movie_duration);
    let tkhd = tkhd_box(movie_duration);
    let mdhd = mdhd_box();
    let hdlr = hdlr_box();

    // Sample table boxes. The chunk offset is patched in after the total size of the moov box is
    // known.
    let stsd = stsd_box();
    let stts = stts_box();
    let stsc = stsc_box();
    let stsz = stsz_box();
    let stco_placeholder = stco_box(0);

    let stbl = box4(b"stbl", &concat(&[stsd, stts, stsc, stsz, stco_placeholder]));
    let smhd = smhd_box();
    let minf = box4(b"minf", &concat(&[smhd, stbl]));
    let mdia = box4(b"mdia", &concat(&[mdhd, hdlr, minf]));

    let edts =
        if elst_entries.is_empty() { Vec::new() } else { box4(b"edts", &elst_box(elst_entries)) };

    let trak = box4(b"trak", &concat(&[tkhd, edts, mdia]));
    let moov = box4(b"moov", &concat(&[mvhd, trak]));

    // The mdat box starts immediately after the ftyp and moov boxes. Its payload begins 8 bytes
    // later.
    let mdat_payload_offset = (ftyp.len() + moov.len() + 8) as u32;

    // Patch the chunk offset inside the serialized moov box.
    let mut moov = moov;
    let stco_offset = find_stco_offset(&moov);
    moov[stco_offset..stco_offset + 4].copy_from_slice(&mdat_payload_offset.to_be_bytes());

    // Build the sample payload.
    let mut mdat_payload = Vec::with_capacity((SAMPLE_COUNT * SAMPLE_SIZE) as usize);
    for sample in 0..SAMPLE_COUNT {
        // Fill each sample with a distinguishable, non-zero pattern.
        let byte = (sample as u8).wrapping_add(1);
        mdat_payload.extend_from_slice(&[byte; SAMPLE_SIZE as usize]);
    }
    let mdat = box4(b"mdat", &mdat_payload);

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp);
    file.extend_from_slice(&moov);
    file.extend_from_slice(&mdat);

    (file, mdat_payload_offset)
}

/// Finds the byte position of the chunk offset value (the payload of the single `stco` entry)
/// within a serialized moov box.
fn find_stco_offset(moov: &[u8]) -> usize {
    let marker = b"stco";
    let pos = moov.windows(4).position(|w| w == marker).expect("moov box contains an stco box");

    // `pos` points at the 4-byte box type. The chunk offset value follows the type (4 bytes), the
    // full-box header (4 bytes) and the entry count (4 bytes).
    pos + 4 + 4 + 4
}

/// Opens a synthesized MP4 file as an `IsoMp4Reader`. The `Cursor<Vec<u8>>` owns its data, so the
/// resulting reader has a `'static` lifetime.
fn open_reader(file: Vec<u8>) -> IsoMp4Reader<'static> {
    let cursor = Cursor::new(file);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    IsoMp4Reader::try_new(mss, Default::default()).expect("synthesized mp4 should open")
}

/// Collects the presentation timestamps and durations of every packet emitted by the reader.
fn collect_packets(reader: &mut IsoMp4Reader<'static>) -> Vec<(i64, u64)> {
    let mut packets = Vec::new();

    while let Some(packet) = reader.next_packet().expect("packet read should not fail") {
        packets.push((packet.pts.get(), packet.dur.get()));
    }

    packets
}

#[test]
fn verify_no_edits_uses_media_timeline() {
    // A file without an edit list should behave exactly like the raw media: 10 samples starting at
    // timestamp 0 with a total duration of 1000 ticks.
    let (file, _) = build_mp4(&[]);
    let mut reader = open_reader(file);

    let track = &reader.tracks()[0];
    assert_eq!(track.duration.map(|d| d.get()), Some(MEDIA_DURATION));
    assert_eq!(track.start_ts.get(), 0);

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), SAMPLE_COUNT as usize);

    for (i, (ts, dur)) in packets.iter().enumerate() {
        assert_eq!(*ts, i as i64 * SAMPLE_DUR as i64);
        assert_eq!(*dur, SAMPLE_DUR as u64);
    }
}

#[test]
fn verify_empty_edit_shifts_first_sample_timestamp() {
    // A 300 tick empty edit followed by the full 1000 tick media. The first media sample is
    // presented at tick 300, and the track duration includes the empty edit.
    let (file, _) = build_mp4(&[(300, -1), (MEDIA_DURATION, 0)]);
    let mut reader = open_reader(file);

    let track = &reader.tracks()[0];
    assert_eq!(track.duration.map(|d| d.get()), Some(1300));
    // The first frame is presented after the 300 tick empty edit.
    assert_eq!(track.start_ts.get(), 300);

    let packets = collect_packets(&mut reader);

    // The empty edit emits no packets; all 10 media samples are still present.
    assert_eq!(packets.len(), SAMPLE_COUNT as usize);

    // The first sample's presentation timestamp accounts for the empty edit.
    assert_eq!(packets[0].0, 300);
    assert_eq!(packets[1].0, 400);
    assert_eq!(packets[9].0, 1200);
}

#[test]
fn verify_start_offset_trim_skips_samples() {
    // A single edit starting at media time 400 for 600 ticks. Samples before media time 400 are
    // not presented, and the track duration is 600.
    let (file, _) = build_mp4(&[(600, 400)]);
    let mut reader = open_reader(file);

    let track = &reader.tracks()[0];
    assert_eq!(track.duration.map(|d| d.get()), Some(600));
    assert_eq!(track.start_ts.get(), 0);

    let packets = collect_packets(&mut reader);

    // Media samples are at 0, 100, 200, ... 900. The edit covers media [400, 1000), so samples 4
    // through 9 are presented (6 samples).
    assert_eq!(packets.len(), 6);

    // The first presented sample is media sample 4 (media time 400), presented at tick 0.
    assert_eq!(packets[0].0, 0);
    assert_eq!(packets[1].0, 100);
    assert_eq!(packets[5].0, 500);
}

#[test]
fn verify_looping_edit_replays_media() {
    // Two edits each playing the full media: the 10 samples are emitted twice, with the second
    // play-through offset by 1000 presentation ticks.
    let (file, _) = build_mp4(&[(MEDIA_DURATION, 0), (MEDIA_DURATION, 0)]);
    let mut reader = open_reader(file);

    let track = &reader.tracks()[0];
    assert_eq!(track.duration.map(|d| d.get()), Some(2 * MEDIA_DURATION));

    let packets = collect_packets(&mut reader);

    // Each play-through yields all 10 samples.
    assert_eq!(packets.len(), 2 * SAMPLE_COUNT as usize);

    // First play-through.
    for (i, packet) in packets[..SAMPLE_COUNT as usize].iter().enumerate() {
        assert_eq!(packet.0, i as i64 * SAMPLE_DUR as i64);
    }

    // Second play-through: the same media times map to the second edit.
    for (i, packet) in packets[SAMPLE_COUNT as usize..].iter().enumerate() {
        assert_eq!(packet.0, MEDIA_DURATION as i64 + i as i64 * SAMPLE_DUR as i64);
    }

    // The first sample of the second play-through is the looping boundary.
    assert_eq!(packets[SAMPLE_COUNT as usize].0, MEDIA_DURATION as i64);
}

#[test]
fn verify_triple_looping_edit_replays_media_three_times() {
    // Three edits each playing the full media. The rewind that supports looping must not be limited
    // to a single replay: all 30 packets (three play-throughs of 10 samples) must be emitted.
    let entries = [(MEDIA_DURATION, 0); 3];
    let (file, _) = build_mp4(&entries);
    let mut reader = open_reader(file);

    let track = &reader.tracks()[0];
    assert_eq!(track.duration.map(|d| d.get()), Some(3 * MEDIA_DURATION));

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), 3 * SAMPLE_COUNT as usize);

    // Every play-through starts at a presentation offset equal to the media duration, and the last
    // packet ends exactly at the edit list duration.
    for play in 0..3usize {
        let first = play * SAMPLE_COUNT as usize;
        assert_eq!(
            packets[first].0,
            play as i64 * MEDIA_DURATION as i64,
            "first packet of play-through {play}"
        );

        let last = first + SAMPLE_COUNT as usize - 1;
        assert_eq!(
            packets[last].0,
            play as i64 * MEDIA_DURATION as i64 + (SAMPLE_COUNT as i64 - 1) * SAMPLE_DUR as i64,
            "last packet of play-through {play}"
        );
    }
}

#[test]
fn verify_seek_respects_edit_list() {
    // Empty edit (300 ticks) + full media. Seek to presentation timestamp 550, which maps to media
    // time 250 and must land on the sample containing media tick 200 (sample 2).
    let (file, _) = build_mp4(&[(300, -1), (MEDIA_DURATION, 0)]);
    let mut reader = open_reader(file);

    let track_id = reader.tracks()[0].id;

    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(550), track_id })
        .expect("seek should succeed");

    // The reported actual timestamp is on the presentation timeline. The seek lands on the sample
    // starting at presentation tick 500 (media tick 200 + 300 tick empty edit).
    assert_eq!(seeked.required_ts.get(), 550);
    assert_eq!(seeked.actual_ts.get(), 500);

    // The next packet emitted after the seek confirms the edit list mapping.
    let packet = reader.next_packet().expect("a packet should follow the seek").unwrap();
    assert_eq!(packet.pts.get(), 500);

    // Seeking to timestamp 0 lands at the very beginning; the first packet is the first media
    // sample presented after the empty edit.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id })
        .expect("seek to zero should succeed");
    assert_eq!(seeked.actual_ts.get(), 300);

    let packet = reader.next_packet().expect("a packet should follow the seek").unwrap();
    assert_eq!(packet.pts.get(), 300);
}

#[test]
fn verify_seek_into_trimmed_edit() {
    // Edit covering media [400, 1000). A presentation timestamp of 250 maps to media time 650 and
    // lands on the sample at media tick 600 (sample 6), presented at tick 200.
    let (file, _) = build_mp4(&[(600, 400)]);
    let mut reader = open_reader(file);

    let track_id = reader.tracks()[0].id;

    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(250), track_id })
        .expect("seek should succeed");

    assert_eq!(seeked.actual_ts.get(), 200);

    let packet = reader.next_packet().expect("a packet should follow the seek").unwrap();
    assert_eq!(packet.pts.get(), 200);
}

#[test]
fn verify_seek_beyond_edit_duration_is_rejected() {
    let (file, _) = build_mp4(&[(300, -1), (MEDIA_DURATION, 0)]);
    let mut reader = open_reader(file);

    let track_id = reader.tracks()[0].id;

    // The presentation timeline is 1300 ticks long; seeking to 1300 or beyond must fail.
    let result =
        reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1300), track_id });

    assert!(result.is_err());
}
