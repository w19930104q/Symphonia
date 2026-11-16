// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Authors.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Seek boundary tests for the OGG demuxer.
//!
//! The tests synthesize minimal OGG physical streams containing an Opus logical stream whose
//! absolute granule positions span negative timestamps (encoder pre-skip) and a final page with
//! padding. All audio packets use the simplest Opus TOC (single 10 ms, 480-sample frame) so the
//! packet timing is fully deterministic without a codec decoder.

use std::io::Cursor;

use symphonia_core::checksum::Crc32;
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::{MediaSourceStream, Monitor, ReadOnlySource};
use symphonia_core::units::Timestamp;
use symphonia_format_ogg::OggReader;

const SERIAL: u32 = 0xdead_beef;

/// OGG page header type flags.
const FLAG_FIRST_PAGE: u8 = 0x02;
const FLAG_LAST_PAGE: u8 = 0x04;

/// One 10 ms mono Opus frame at 48 kHz (480 samples). TOC byte 0x00 is configuration 0 (10 ms
/// frame), code 0 (single frame). The second byte is arbitrary payload since the tests never
/// decode the packet.
const AUDIO_PACKET: &[u8] = &[0x00, 0x00];

/// Number of audio frames (samples) per audio packet.
const PACKET_DUR: i64 = 480;

/// A synthesized OGG page. Packets are encoded into as many lacing segments as required.
struct SynthPage {
    flags: u8,
    sequence: u32,
    absgp: i64,
    packets: Vec<Vec<u8>>,
}

fn opus_head_packet() -> Vec<u8> {
    let mut buf = Vec::with_capacity(19);

    buf.extend_from_slice(b"OpusHead");
    buf.push(1); // encapsulation version
    buf.push(1); // 1 channel
    buf.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    buf.extend_from_slice(&48_000u32.to_le_bytes()); // original sample rate
    buf.extend_from_slice(&0i16.to_le_bytes()); // gain
    buf.push(0); // channel mapping 0

    buf
}

fn opus_tags_packet() -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);

    buf.extend_from_slice(b"OpusTags");
    buf.extend_from_slice(&0u32.to_le_bytes()); // empty vendor string
    buf.extend_from_slice(&0u32.to_le_bytes()); // no comments

    buf
}

fn audio_packet() -> Vec<u8> {
    AUDIO_PACKET.to_vec()
}

fn encode_page(page: &SynthPage) -> Vec<u8> {
    let mut segment_table = Vec::new();
    let mut body = Vec::new();

    for packet in &page.packets {
        assert!(packet.len() <= 255 * 255, "test packet must fit in one ogg page");

        // Encode the packet length as a run of 255-byte segments terminated by a final segment.
        let mut remaining = packet.len();

        while remaining >= 255 {
            segment_table.push(255);
            remaining -= 255;
        }

        segment_table.push(remaining as u8);
        body.extend_from_slice(packet);
    }

    assert!(segment_table.len() <= 255, "test page must fit in one ogg page");

    let mut header = Vec::with_capacity(27 + segment_table.len());

    header.extend_from_slice(b"OggS");
    header.push(0); // version
    header.push(page.flags);
    header.extend_from_slice(&page.absgp.to_le_bytes());
    header.extend_from_slice(&SERIAL.to_le_bytes());
    header.extend_from_slice(&page.sequence.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
    header.push(segment_table.len() as u8);
    header.extend_from_slice(&segment_table);

    // Calculate the CRC over the full page with the CRC field zeroed, then insert it.
    let mut crc32 = Crc32::new(0);
    crc32.process_buf_bytes(&header);
    crc32.process_buf_bytes(&body);

    header[22..26].copy_from_slice(&crc32.crc().to_le_bytes());

    header.extend_from_slice(&body);
    header
}

fn encode_stream(pages: &[SynthPage]) -> Vec<u8> {
    pages.iter().flat_map(encode_page).collect()
}

fn open_reader(data: Vec<u8>) -> Box<dyn FormatReader> {
    let cursor = Cursor::new(data);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    Box::new(OggReader::try_new(mss, FormatOptions::default()).expect("reader should open"))
}

fn open_unseekable_reader(data: Vec<u8>) -> Box<dyn FormatReader> {
    let cursor = Cursor::new(data);
    let source = ReadOnlySource::new(cursor);
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    Box::new(OggReader::try_new(mss, FormatOptions::default()).expect("reader should open"))
}

fn seek_ts(reader: &mut dyn FormatReader, ts: i64) -> Result<SeekedTo, Error> {
    reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: SERIAL })
}

/// A multi-page Opus stream whose first bitstream page has a negative absolute granule position,
/// and whose last page contains a full packet of padding.
///
/// Page layout (each audio packet holds 480 frames):
///
/// | page | seq | absgp | packets        | start ts | end ts |
/// |------|-----|-------|----------------|----------|--------|
/// | 0    | 0   |   0   | OpusHead       | n/a      | n/a    |
/// | 1    | 1   |   0   | OpusTags       | n/a      | n/a    |
/// | 2    | 2   |   0   | audio 0        | -480     | 0      |
/// | 3    | 3   | 480   | audio 1        | 0        | 480    |
/// | 4    | 4   | 960   | audio 2        | 480      | 960    |
/// | 5    | 5   | 1440  | audio 3        | 960      | 1440   |
/// | 6    | 6   | 1440  | audio 4 (pad)  | 1440     | 1440   |
fn negative_ts_stream() -> Vec<u8> {
    let pages = [
        SynthPage {
            flags: FLAG_FIRST_PAGE,
            sequence: 0,
            absgp: 0,
            packets: vec![opus_head_packet()],
        },
        SynthPage { flags: 0, sequence: 1, absgp: 0, packets: vec![opus_tags_packet()] },
        SynthPage { flags: 0, sequence: 2, absgp: 0, packets: vec![audio_packet()] },
        SynthPage { flags: 0, sequence: 3, absgp: 480, packets: vec![audio_packet()] },
        SynthPage { flags: 0, sequence: 4, absgp: 960, packets: vec![audio_packet()] },
        SynthPage { flags: 0, sequence: 5, absgp: 1440, packets: vec![audio_packet()] },
        SynthPage {
            flags: FLAG_LAST_PAGE,
            sequence: 6,
            absgp: 1440,
            packets: vec![audio_packet()],
        },
    ];

    encode_stream(&pages)
}

/// A short stream whose final page has a negative absolute granule position. The whole stream
/// starts at t = -480 and ends at t = -240.
fn negative_end_ts_stream() -> Vec<u8> {
    let pages = [
        SynthPage {
            flags: FLAG_FIRST_PAGE,
            sequence: 0,
            absgp: 0,
            packets: vec![opus_head_packet()],
        },
        SynthPage { flags: 0, sequence: 1, absgp: 0, packets: vec![opus_tags_packet()] },
        SynthPage { flags: 0, sequence: 2, absgp: 0, packets: vec![audio_packet()] },
        SynthPage {
            flags: FLAG_LAST_PAGE,
            sequence: 3,
            absgp: -240,
            packets: vec![audio_packet()],
        },
    ];

    encode_stream(&pages)
}

/// Builds a stream whose audio pages are padded close to the maximum OGG page size so that the
/// overall physical stream exceeds the demuxer's bisection threshold
/// (`2 * OGG_PAGE_MAX_SIZE`), forcing the bisection code path in `do_seek`.
///
/// Every audio packet still holds 480 frames; the extra bytes are arbitrary payload and never
/// decoded. Timestamps mirror [`negative_ts_stream`], with padding on the final page.
fn large_page_stream() -> Vec<u8> {
    // 254 segments of 255 bytes plus a 254-byte terminator: a valid packet just under the 65025
    // byte single-page body limit.
    fn big_audio_packet() -> Vec<u8> {
        let mut packet = vec![0u8; 254 * 255 + 254];
        packet[0] = AUDIO_PACKET[0];
        packet
    }

    let mut pages = vec![
        SynthPage {
            flags: FLAG_FIRST_PAGE,
            sequence: 0,
            absgp: 0,
            packets: vec![opus_head_packet()],
        },
        SynthPage { flags: 0, sequence: 1, absgp: 0, packets: vec![opus_tags_packet()] },
    ];

    // Six full-size audio pages, the last one marked as the final page with a repeated granule
    // position so its packet is entirely padding.
    let absgps = [0, 480, 960, 1440, 1920, 1920];

    for (i, absgp) in absgps.into_iter().enumerate() {
        let is_last = i == absgps.len() - 1;

        pages.push(SynthPage {
            flags: if is_last { FLAG_LAST_PAGE } else { 0 },
            sequence: (i + 2) as u32,
            absgp,
            packets: vec![big_audio_packet()],
        });
    }

    encode_stream(&pages)
}

#[test]
fn seek_to_negative_timestamp_does_not_panic() {
    let data = negative_ts_stream();
    let mut reader = open_reader(data);

    // The first bitstream page ends at t = 0 and starts at t = -480.
    let seeked = seek_ts(reader.as_mut(), -480).expect("seek to first page should succeed");

    assert_eq!(seeked.track_id, SERIAL);
    assert_eq!(seeked.required_ts.get(), -480);
    assert_eq!(seeked.actual_ts.get(), -480);

    // Seeking before the start of the stream is an out-of-range error, not a panic.
    let err = seek_ts(reader.as_mut(), -481).expect_err("seek before start should fail");

    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));
}

#[test]
fn seek_to_zero_lands_on_first_bitstream_page() {
    let data = negative_ts_stream();
    let mut reader = open_reader(data);

    let seeked = seek_ts(reader.as_mut(), 0).expect("seek to zero should succeed");

    // t = 0 is the end timestamp of the first bitstream page; the demuxer must not skip the
    // packet with the negative timestamp while consuming.
    assert_eq!(seeked.actual_ts.get(), -480);
}

#[test]
fn seek_to_middle_timestamp_lands_on_correct_page() {
    let data = negative_ts_stream();
    let mut reader = open_reader(data);

    for (required, expected_actual) in [
        (-480, -480),
        (-1, -480),
        (0, -480),
        (1, 0),
        (479, 0),
        (480, 0),
        (481, 480),
        (959, 480),
        (960, 480),
        (961, 960),
        (1439, 960),
        (1440, 960),
    ] {
        let seeked = seek_ts(reader.as_mut(), required).expect("in-range seek should succeed");

        assert_eq!(
            seeked.actual_ts.get(),
            expected_actual,
            "seek to {required} should land at {expected_actual}"
        );
        assert_eq!(seeked.required_ts.get(), required);
    }
}

#[test]
fn seek_to_last_page_does_not_panic_on_padding() {
    let data = negative_ts_stream();
    let mut reader = open_reader(data);

    // The last page spans 1440..1440 because its packet is entirely padding. A seek to the final
    // valid timestamp must not panic while converting the end bound.
    let seeked = seek_ts(reader.as_mut(), 1440).expect("seek to last valid ts should succeed");

    assert_eq!(seeked.actual_ts.get(), 960);
    assert_eq!(seeked.required_ts.get(), 1440);

    // One frame past the end of the valid data is out-of-range.
    let err = seek_ts(reader.as_mut(), 1441).expect_err("seek past end should fail");

    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));
}

#[test]
fn repeated_seeks_remain_stable() {
    let data = negative_ts_stream();
    let mut reader = open_reader(data);

    // Alternate between boundary and middle seeks. Every call must return the same position and
    // never panic.
    for _ in 0..3 {
        assert_eq!(seek_ts(reader.as_mut(), -480).unwrap().actual_ts.get(), -480);
        assert_eq!(seek_ts(reader.as_mut(), 500).unwrap().actual_ts.get(), 480);
        assert_eq!(seek_ts(reader.as_mut(), 1440).unwrap().actual_ts.get(), 960);
        assert!(matches!(
            seek_ts(reader.as_mut(), 10_000),
            Err(Error::SeekError(SeekErrorKind::OutOfRange))
        ));
        assert!(matches!(
            seek_ts(reader.as_mut(), -10_000),
            Err(Error::SeekError(SeekErrorKind::OutOfRange))
        ));
    }
}

#[test]
fn negative_end_timestamp_does_not_overflow_frame_count() {
    let data = negative_end_ts_stream();
    let mut reader = open_reader(data);

    let track = reader.tracks().first().expect("there should be one track");

    assert_eq!(track.start_ts.get(), -480);

    // The end timestamp is -240, so the stream contains 240 playable frames. Casting the negative
    // end timestamp directly to u64 would yield an enormous frame count.
    assert_eq!(track.num_frames, Some(240));
    assert_eq!(track.duration.map(|d| d.get()), Some(240));

    // Seeking within the entirely-negative range must work.
    let seeked = seek_ts(reader.as_mut(), -240).expect("seek to negative end should succeed");

    assert_eq!(seeked.actual_ts.get(), -480);

    // One frame past the negative end is out-of-range.
    let err = seek_ts(reader.as_mut(), -239).expect_err("seek past negative end should fail");

    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));
}

#[test]
fn bisection_seek_handles_negative_and_boundary_timestamps() {
    // This stream is large enough to force the bisection path in the demuxer's seek logic.
    let data = large_page_stream();
    assert!(data.len() > 2 * 65_307, "stream should exceed the bisection threshold");

    let mut reader = open_reader(data);

    // Seek to the negative start timestamp through bisection and linear refinement.
    let seeked = seek_ts(reader.as_mut(), -480).expect("negative seek through bisection");

    assert_eq!(seeked.actual_ts.get(), -480);

    // A middle timestamp must converge on the correct page rather than the nearest byte boundary.
    for (required, expected_actual) in
        [(-480, -480), (0, -480), (1, 0), (960, 960), (1919, 1440), (1920, 1440)]
    {
        let seeked = seek_ts(reader.as_mut(), required).expect("in-range bisection seek");

        assert_eq!(
            seeked.actual_ts.get(),
            expected_actual,
            "bisect seek to {required} should land at {expected_actual}"
        );
    }

    // Boundary violations on a bisected stream return an error instead of panicking.
    let err = seek_ts(reader.as_mut(), -481).expect_err("seek before start should fail");

    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));

    let err = seek_ts(reader.as_mut(), 1921).expect_err("seek past end should fail");

    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));
}

#[test]
fn unseekable_stream_reports_out_of_range_without_panic() {
    // An unseekable reader cannot bisect, so out-of-range seeks are discovered while consuming
    // packets until end-of-stream.
    let data = negative_ts_stream();
    let mut reader = open_unseekable_reader(data);

    // In-range seeks on unseekable streams work by consuming forward, including the packet with
    // the negative timestamp.
    let seeked = seek_ts(reader.as_mut(), 0).expect("forward seek should succeed");

    assert_eq!(seeked.actual_ts.get(), -480);

    let err = seek_ts(reader.as_mut(), 1441).expect_err("seek past end should fail");

    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));

    // Seeking before the start of an unseekable stream is rejected up-front, without consuming.
    let data = negative_ts_stream();
    let mut reader = open_unseekable_reader(data);

    let err = seek_ts(reader.as_mut(), -481).expect_err("seek before start should fail");

    assert!(matches!(err, Error::SeekError(SeekErrorKind::OutOfRange)));
}

#[test]
fn packet_timestamps_are_contiguous() {
    // Sanity check on the synthesized stream: packets read start-to-finish must have contiguous
    // PTS values and the final packet (padding) must be trimmed to zero duration.
    let data = negative_ts_stream();
    let mut reader = open_reader(data);

    let mut expected_pts: i64 = -PACKET_DUR;
    let mut count = 0;

    while let Some(packet) = reader.next_packet().expect("reading packet") {
        assert_eq!(packet.pts.get(), expected_pts, "packet {count} pts");

        expected_pts += packet.dur.get() as i64;
        count += 1;
    }

    // Four playable packets followed by the fully-trimmed padding packet.
    assert_eq!(count, 5);
    assert_eq!(expected_pts, 1440);
}
