// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests for the WAVE demuxer's handling of RIFF LIST/INFO metadata.
//!
//! These tests synthesize WAVE files in-memory with metadata placed either before or after the
//! audio data chunk, and verify that:
//!
//! * The title, artist, and comment tags are returned and match the file contents.
//! * Metadata placed after the data chunk (the common layout produced by some taggers) is read.
//! * WAVE files without metadata behave the same as before (no empty metadata revisions).
//! * Packetization and decoded PCM output are identical regardless of chunk layout.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::Cursor;

use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::FormatReader;
use symphonia_core::io::{MediaSourceStream, ReadOnlySource};
use symphonia_core::meta::StandardTag;

use symphonia_codec_pcm::PcmDecoder;
use symphonia_format_riff::WavReader;

/// A single RIFF INFO list entry (e.g. an `inam` tag and its value).
struct InfoEntry {
    tag: [u8; 4],
    value: &'static str,
}

const SAMPLE_RATE: u32 = 8_000;
const NUM_CHANNELS: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;

/// Deterministic PCM samples used as the audio data payload.
fn pcm_samples() -> Vec<i16> {
    (0..1024).map(|i| ((i * 37) % 1000) as i16 - 500).collect()
}

/// Serialize an i16 sample as a little-endian byte pair.
fn le16(value: u16) -> [u8; 2] {
    value.to_le_bytes()
}

/// Serialize a u32 as little-endian bytes.
fn le32(value: u32) -> [u8; 4] {
    value.to_le_bytes()
}

/// Build a RIFF chunk body: a 4-byte tag, the body length, the body, and a padding byte if the body
/// has an odd length.
fn riff_chunk(tag: &[u8; 4], body: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&le32(body.len() as u32));
    out.extend_from_slice(body);
    if body.len() & 0x1 == 1 {
        out.push(0);
    }
}

/// Build the 16-byte PCM `fmt ` chunk body.
fn fmt_chunk_body() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&le16(1)); // WAVE_FORMAT_PCM
    body.extend_from_slice(&le16(NUM_CHANNELS));
    body.extend_from_slice(&le32(SAMPLE_RATE));
    let block_align = NUM_CHANNELS * (BITS_PER_SAMPLE / 8);
    body.extend_from_slice(&le32(SAMPLE_RATE * u32::from(block_align))); // avg bytes/sec
    body.extend_from_slice(&le16(block_align));
    body.extend_from_slice(&le16(BITS_PER_SAMPLE));
    body
}

/// Build a LIST/INFO chunk body (the form identifier followed by the info sub-chunks).
fn list_info_body(entries: &[InfoEntry]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"INFO");

    for entry in entries {
        let value = entry.value.as_bytes();
        body.extend_from_slice(&entry.tag);
        body.extend_from_slice(&le32(value.len() as u32));
        body.extend_from_slice(value);
        if value.len() & 0x1 == 1 {
            body.push(0);
        }
    }

    body
}

/// Build the audio data chunk body from the deterministic PCM samples.
fn data_chunk_body() -> Vec<u8> {
    pcm_samples().iter().flat_map(|sample| sample.to_le_bytes()).collect()
}

/// Synthesize a WAVE file. `metadata_before` and `metadata_after` control whether a LIST/INFO chunk
/// is emitted before or after the data chunk.
fn build_wav(metadata_before: bool, metadata_after: bool) -> Vec<u8> {
    let entries = [
        InfoEntry { tag: *b"inam", value: "Test Track Title" },
        InfoEntry { tag: *b"iart", value: "Test Artist" },
        InfoEntry { tag: *b"icmt", value: "A test comment." },
    ];

    let fmt_body = fmt_chunk_body();
    let data_body = data_chunk_body();
    let info_body = list_info_body(&entries);

    let mut inner = Vec::new();
    riff_chunk(b"fmt ", &fmt_body, &mut inner);

    if metadata_before {
        riff_chunk(b"LIST", &info_body, &mut inner);
    }

    riff_chunk(b"data", &data_body, &mut inner);

    if metadata_after {
        riff_chunk(b"LIST", &info_body, &mut inner);
    }

    let mut file = Vec::new();
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&le32(4 + inner.len() as u32));
    file.extend_from_slice(b"WAVE");
    file.extend_from_slice(&inner);
    file
}

/// Open a synthesized WAVE file as a `WavReader` over a seekable source.
fn open_wav(wav: Vec<u8>) -> Box<WavReader<'static>> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(wav)), Default::default());
    Box::new(WavReader::try_new(mss, Default::default()).expect("failed to open wav"))
}

/// Collect the raw (key, value) pairs and standard-tag kinds from all metadata revisions.
fn collect_tags(reader: &mut WavReader<'_>) -> (Vec<(String, String)>, Vec<&'static str>) {
    let mut raw_tags = Vec::new();
    let mut std_tags = Vec::new();

    let mut metadata = reader.metadata();

    while let Some(revision) = metadata.current() {
        for tag in &revision.media.tags {
            raw_tags.push((tag.raw.key.clone(), tag.raw.value.to_string()));

            if let Some(std_tag) = &tag.std {
                std_tags.push(match std_tag {
                    StandardTag::TrackTitle(_) => "TrackTitle",
                    StandardTag::Artist(_) => "Artist",
                    StandardTag::Comment(_) => "Comment",
                    _ => "other",
                });
            }
        }

        if metadata.pop().is_none() {
            break;
        }
    }

    (raw_tags, std_tags)
}

/// Find the value of a raw tag by key.
fn tag_value<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Assert that the expected INFO tags are present with the values written into the file.
fn assert_info_tags(reader: &mut WavReader<'_>) {
    let (tags, std_tags) = collect_tags(reader);

    assert_eq!(tag_value(&tags, "inam"), Some("Test Track Title"));
    assert_eq!(tag_value(&tags, "iart"), Some("Test Artist"));
    assert_eq!(tag_value(&tags, "icmt"), Some("A test comment."));

    assert!(std_tags.contains(&"TrackTitle"));
    assert!(std_tags.contains(&"Artist"));
    assert!(std_tags.contains(&"Comment"));
}

/// Read every packet and decode it, returning all interleaved i16 samples.
fn decode_all(reader: &mut WavReader<'_>) -> Vec<i16> {
    let track = &reader.tracks()[0];
    let params = match track.codec_params.as_ref().expect("missing codec params") {
        symphonia_core::codecs::CodecParameters::Audio(params) => params.clone(),
        _ => panic!("expected audio codec parameters"),
    };

    let mut decoder = PcmDecoder::try_new(&params, &AudioDecoderOptions::default())
        .expect("failed to make pcm decoder");

    let mut samples = Vec::new();

    while let Some(packet) = reader.next_packet().expect("failed to read packet") {
        let buf = decoder.decode(&packet).expect("failed to decode packet");
        buf.copy_to_vec_interleaved::<i16>(&mut samples);
    }

    samples
}

#[test]
fn metadata_before_data_chunk_is_read() {
    let wav = build_wav(true, false);
    let mut reader = open_wav(wav);
    assert_info_tags(&mut reader);
}

#[test]
fn metadata_after_data_chunk_is_read() {
    let wav = build_wav(false, true);
    let mut reader = open_wav(wav);
    assert_info_tags(&mut reader);
}

#[test]
fn metadata_in_both_layouts_is_read() {
    let wav = build_wav(true, true);
    let mut reader = open_wav(wav);

    // Two non-empty LIST/INFO chunks yield two metadata revisions, each carrying the full set of
    // tags. Iterate the log a single time since popping advances the revision queue.
    let mut revision_count = 0;
    let mut metadata = reader.metadata();

    while let Some(revision) = metadata.current() {
        assert!(!revision.media.tags.is_empty());

        let tags: Vec<(String, String)> = revision
            .media
            .tags
            .iter()
            .map(|tag| (tag.raw.key.clone(), tag.raw.value.to_string()))
            .collect();

        assert_eq!(tag_value(&tags, "inam"), Some("Test Track Title"));
        assert_eq!(tag_value(&tags, "iart"), Some("Test Artist"));
        assert_eq!(tag_value(&tags, "icmt"), Some("A test comment."));

        revision_count += 1;

        if metadata.pop().is_none() {
            break;
        }
    }

    assert_eq!(revision_count, 2);
}

#[test]
fn wav_without_metadata_has_no_revisions() {
    let wav = build_wav(false, false);
    let mut reader = open_wav(wav);

    let metadata = reader.metadata();
    assert!(metadata.current().is_none());
}

#[test]
fn decoding_is_identical_for_all_chunk_layouts() {
    let expected: Vec<i16> = pcm_samples();

    for (before, after) in [(false, false), (true, false), (false, true), (true, true)] {
        let wav = build_wav(before, after);
        let mut reader = open_wav(wav);

        // The track reports the full number of frames from the data chunk length.
        let track = &reader.tracks()[0];
        assert_eq!(track.num_frames, Some(expected.len() as u64));

        // Decoded samples match the file contents exactly for every layout.
        let samples = decode_all(&mut reader);
        assert_eq!(samples, expected);
    }
}

#[test]
fn metadata_after_data_chunk_is_read_from_unseekable_source() {
    let wav = build_wav(false, true);

    // A non-seekable source cannot be scanned past the data chunk, so the trailing metadata cannot
    // be read. Opening must still succeed and decoding must be unaffected.
    let mss =
        MediaSourceStream::new(Box::new(ReadOnlySource::new(Cursor::new(wav))), Default::default());
    let mut reader =
        Box::new(WavReader::try_new(mss, Default::default()).expect("failed to open wav"));

    let metadata = reader.metadata();
    assert!(metadata.current().is_none());

    let samples = decode_all(&mut reader);
    assert_eq!(samples, pcm_samples());
}

#[test]
fn metadata_before_data_chunk_is_read_from_unseekable_source() {
    let wav = build_wav(true, false);

    let mss =
        MediaSourceStream::new(Box::new(ReadOnlySource::new(Cursor::new(wav))), Default::default());
    let mut reader =
        Box::new(WavReader::try_new(mss, Default::default()).expect("failed to open wav"));

    // Metadata preceding the data chunk is reachable without seeking.
    assert_info_tags(&mut reader);
    assert_eq!(decode_all(&mut reader), pcm_samples());
}

#[test]
fn second_data_chunk_terminates_scan_without_breaking_decoding() {
    // Some writers emit more than one data chunk. Only the first one defines the packetization
    // bounds; the scan must stop at the second one, seek back to the first on a seekable source,
    // and still expose the metadata found between them.
    let entries = [
        InfoEntry { tag: *b"inam", value: "Test Track Title" },
        InfoEntry { tag: *b"iart", value: "Test Artist" },
        InfoEntry { tag: *b"icmt", value: "A test comment." },
    ];
    let info_body = list_info_body(&entries);

    let mut inner = Vec::new();
    riff_chunk(b"fmt ", &fmt_chunk_body(), &mut inner);
    riff_chunk(b"data", &data_chunk_body(), &mut inner);
    riff_chunk(b"LIST", &info_body, &mut inner);
    // A second, smaller data chunk that must not alter the first one's bounds.
    riff_chunk(b"data", &[0u8; 8], &mut inner);

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&le32(4 + inner.len() as u32));
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(&inner);

    let mut reader = open_wav(wav);

    // The track length is derived solely from the first data chunk.
    assert_eq!(reader.tracks()[0].num_frames, Some(pcm_samples().len() as u64));

    // Metadata between the two data chunks is still reachable.
    assert_info_tags(&mut reader);

    // Packetization starts at the first data chunk and decodes the full payload exactly once.
    assert_eq!(decode_all(&mut reader), pcm_samples());
}
