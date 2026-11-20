// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression tests for demuxing FLAC streams with trailing ID3v1 and APEv2 tags.
//!
//! Appending a trailing tag to a FLAC file must not cause the final audio frame to be lost.

use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::{Duration, Timestamp};

use symphonia_bundle_flac::{FlacDecoder, FlacReader};

/// The FLAC test stream: 0.2s of a 440Hz sine wave, mono, 8kHz, 16-bit (1600 samples).
const TONE_FLAC: &[u8] = include_bytes!("res/tone.flac");

/// Make an ID3v1 tag. An ID3v1 tag is exactly 128 bytes long and starts with "TAG".
fn make_id3v1_tag() -> Vec<u8> {
    let mut tag = b"TAG".to_vec();
    tag.resize(128, 0);
    tag
}

/// Make an APEv2 tag (with a header) containing the given key-value items.
fn make_apev2_tag(items: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();

    for (key, value) in items {
        body.extend_from_slice(&(value.len() as u32).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(value);
    }

    // The size includes the footer, but not the header.
    let size = (body.len() + 32) as u32;
    let num_items = items.len() as u32;

    let make_record = |flags: u32| {
        let mut record = Vec::new();
        record.extend_from_slice(b"APETAGEX");
        record.extend_from_slice(&2000u32.to_le_bytes());
        record.extend_from_slice(&size.to_le_bytes());
        record.extend_from_slice(&num_items.to_le_bytes());
        record.extend_from_slice(&flags.to_le_bytes());
        record.extend_from_slice(&[0; 8]);
        record
    };

    // Header flags: bit 31 = tag has a header, bit 29 = this is the header.
    // Footer flags: bit 31 = tag has a header.
    let mut tag = make_record(0xA000_0000);
    tag.extend_from_slice(&body);
    tag.extend_from_slice(&make_record(0x8000_0000));

    tag
}

/// Demux all packets from the stream, and decode them, returning the packets (timestamp,
/// duration, and data), and the total number of decoded audio frames.
fn demux_and_decode(data: &[u8]) -> (Vec<(Timestamp, Duration, Box<[u8]>)>, u64) {
    let mss =
        MediaSourceStream::new(Box::new(std::io::Cursor::new(data.to_vec())), Default::default());

    let mut reader = FlacReader::try_new(mss, Default::default()).expect("should probe");

    let track_params = reader.tracks()[0].codec_params.as_ref().expect("codec parameters");
    let params = track_params.audio().expect("audio codec parameters");
    let mut decoder =
        FlacDecoder::try_new(params, &AudioDecoderOptions::default()).expect("should instantiate");

    let mut packets = Vec::new();
    let mut n_frames = 0u64;

    while let Some(packet) = reader.next_packet().expect("should demux packet") {
        let decoded = decoder.decode(&packet).expect("should decode packet");
        n_frames += decoded.frames() as u64;

        packets.push((packet.pts, packet.dur, packet.data.clone()));
    }

    (packets, n_frames)
}

/// Demux and decode the test stream with the given trailing data appended. The result must be
/// identical to the result for the test stream without any trailing data.
fn check_with_trailing_data(trailing: &[u8]) {
    let (ref_packets, ref_frames) = demux_and_decode(TONE_FLAC);

    let mut tagged = TONE_FLAC.to_vec();
    tagged.extend_from_slice(trailing);

    let (packets, frames) = demux_and_decode(&tagged);

    assert_eq!(frames, ref_frames, "decoded frame count mismatch");
    assert_eq!(packets.len(), ref_packets.len(), "packet count mismatch");

    for (packet, ref_packet) in packets.iter().zip(ref_packets.iter()) {
        assert_eq!(packet, ref_packet, "packet mismatch");
    }
}

#[test]
fn trailing_id3v1_tag() {
    check_with_trailing_data(&make_id3v1_tag());
}

#[test]
fn trailing_apev2_tag() {
    check_with_trailing_data(&make_apev2_tag(&[("Title", b"Test Title"), ("Artist", b"Test")]));
}

#[test]
fn trailing_apev2_tag_large() {
    let big_value = vec![b'x'; 8096];
    check_with_trailing_data(&make_apev2_tag(&[("Comment", &big_value)]));
}

#[test]
fn trailing_apev2_and_id3v1_tags() {
    let mut tags = make_apev2_tag(&[("Title", b"Test Title")]);
    tags.extend_from_slice(&make_id3v1_tag());
    check_with_trailing_data(&tags);
}

#[test]
fn trailing_chained_id3v1_tags() {
    let mut tags = make_id3v1_tag();
    tags.extend_from_slice(&make_id3v1_tag());
    check_with_trailing_data(&tags);
}

#[test]
fn seek_with_trailing_id3v1_tag() {
    let (ref_packets, _) = demux_and_decode(TONE_FLAC);
    let last_ts = ref_packets.last().unwrap().0;

    let mut tagged = TONE_FLAC.to_vec();
    tagged.extend_from_slice(&make_id3v1_tag());

    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(tagged)), Default::default());

    let mut reader = FlacReader::try_new(mss, Default::default()).expect("should probe");

    // Seek to the final frame of the stream.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: last_ts, track_id: 0 })
        .expect("should seek");

    assert_eq!(seeked.actual_ts, last_ts);

    // The next packet must be the final frame of the stream.
    let packet = reader.next_packet().expect("should demux packet").expect("should have packet");

    assert_eq!(packet.pts, last_ts);
    assert_eq!(packet.data, ref_packets.last().unwrap().2);
}
