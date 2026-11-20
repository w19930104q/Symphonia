// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for MPEG audio embedded in a WAVE container.
//!
//! Every fixture is synthesized in memory around a small real MP3 elementary stream, so the tests
//! do not depend on any external assets beyond the committed five-frame stream.

use std::io::Cursor;

use symphonia_bundle_mp3::MpaDecoder;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::{MediaSourceStream, ReadOnlySource};
use symphonia_format_riff::WavReader;

/// A five-frame, 128 kbps, 44.1 kHz MPEG-1 layer-3 stream (5760 samples).
const MP3_STREAM: &[u8] = include_bytes!("assets/sine_5frames.mp3");

const WAVE_FORMAT_MPEGLAYER3: u16 = 0x0055;
const WAVE_FORMAT_MPEG: u16 = 0x0050;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const WAVE_FORMAT_UNKNOWN: u16 = 0x0099;

const MP3_NUM_FRAMES: u64 = 5;
const MP3_NUM_SAMPLES: u64 = 5 * 1152;

fn chunk(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len() + 1);
    out.extend_from_slice(tag);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    // Chunks are word-aligned.
    if body.len() & 1 == 1 {
        out.push(0);
    }
    out
}

/// Build a minimal WAVEFORMATEX (18 bytes, cbSize == 0) for the given format tag.
fn fmt_chunk_ex(tag: u16, channels: u16, sample_rate: u32, extra: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(18 + extra.len());
    body.extend_from_slice(&tag.to_le_bytes());
    body.extend_from_slice(&channels.to_le_bytes());
    body.extend_from_slice(&sample_rate.to_le_bytes());
    // Average bytes per second and block align are not used to packetize MPEG streams.
    body.extend_from_slice(&16_000u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    body.extend_from_slice(extra);
    chunk(b"fmt ", &body)
}

fn fmt_chunk(tag: u16, channels: u16, sample_rate: u32) -> Vec<u8> {
    fmt_chunk_ex(tag, channels, sample_rate, &[])
}

fn fact_chunk(num_frames: u32) -> Vec<u8> {
    chunk(b"fact", &num_frames.to_le_bytes())
}

fn data_chunk(data: &[u8]) -> Vec<u8> {
    chunk(b"data", data)
}

fn riff_wave(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    for chunk in chunks {
        body.extend_from_slice(chunk);
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn mpeg_wav(tag: u16, channels: u16, data: &[u8]) -> Vec<u8> {
    riff_wave(&[
        fmt_chunk(tag, channels, 44_100),
        fact_chunk(MP3_NUM_SAMPLES as u32),
        data_chunk(data),
    ])
}

fn open(bytes: &[u8]) -> WavReader<'_> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    WavReader::try_new(mss, FormatOptions::default()).expect("probe should succeed")
}

/// Collect every packet as (pts, duration, raw bytes).
fn collect_packets(reader: &mut WavReader<'_>) -> Vec<(u64, u64, Vec<u8>)> {
    let mut packets = Vec::new();

    while let Some(packet) = reader.next_packet().expect("packetization should not fail") {
        packets.push((packet.pts.get() as u64, packet.dur.get(), packet.data.to_vec()));
    }

    packets
}

/// Decode every packet and return the interleaved f32 samples.
fn decode_all(reader: &mut WavReader<'_>) -> Vec<f32> {
    let track = reader.tracks()[0].clone();
    let params = match track.codec_params.expect("codec parameters are present") {
        CodecParameters::Audio(params) => params,
        _ => panic!("expected audio codec parameters"),
    };

    let mut decoder =
        MpaDecoder::try_new(&params, &AudioDecoderOptions::default()).expect("decoder creation");

    let mut samples = Vec::new();

    while let Some(packet) = reader.next_packet().expect("packetization should not fail") {
        let decoded = decoder.decode(&packet).expect("decoding should succeed");

        // copy_to_vec_interleaved resizes its destination to one packet's length, so use a fresh
        // vector per packet and append it afterwards.
        let mut packet_samples = Vec::new();
        decoded.copy_to_vec_interleaved(&mut packet_samples);
        samples.extend_from_slice(&packet_samples);
    }

    samples
}

#[test]
fn mpeglayer3_tag_packetizes_one_packet_per_frame() {
    let wav = mpeg_wav(WAVE_FORMAT_MPEGLAYER3, 1, MP3_STREAM);
    let mut reader = open(&wav);

    let track = &reader.tracks()[0];
    assert_eq!(track.num_frames, Some(MP3_NUM_SAMPLES));
    assert_eq!(track.duration, Some(Duration::from(MP3_NUM_SAMPLES)));
    assert_eq!(track.time_base.map(|tb| (tb.numer.get(), tb.denom.get())), Some((1, 44_100)));

    let packets = collect_packets(&mut reader);

    assert_eq!(packets.len(), MP3_NUM_FRAMES as usize);

    // Timestamps are the running count of decoded samples.
    for (i, (pts, dur, _)) in packets.iter().enumerate() {
        assert_eq!(*pts, i as u64 * 1152);
        assert_eq!(*dur, 1152);
    }

    // Each packet is exactly one frame: the 4-byte header word plus its body.
    let total_len: usize = packets.iter().map(|(_, _, data)| data.len()).sum();
    assert_eq!(total_len, MP3_STREAM.len());
}

#[test]
fn generic_mpeg_tag_uses_same_packetization() {
    let wav = mpeg_wav(WAVE_FORMAT_MPEG, 1, MP3_STREAM);
    let mut reader = open(&wav);

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), MP3_NUM_FRAMES as usize);
    assert_eq!(packets[0].1, 1152);
}

#[test]
fn extensible_mpeglayer3_subtype_is_supported() {
    #[rustfmt::skip]
    let guid = [
        0x55u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
        0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
    ];

    let mut ext = Vec::new();
    ext.extend_from_slice(&(22u16).to_le_bytes()); // cbSize
    ext.extend_from_slice(&0u16.to_le_bytes()); // valid bits per sample
    ext.extend_from_slice(&0x4u32.to_le_bytes()); // mono channel mask
    ext.extend_from_slice(&guid);

    let mut body = Vec::new();
    body.extend_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&44_100u32.to_le_bytes());
    body.extend_from_slice(&16_000u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&ext);

    let wav = riff_wave(&[
        chunk(b"fmt ", &body),
        fact_chunk(MP3_NUM_SAMPLES as u32),
        data_chunk(MP3_STREAM),
    ]);

    let mut reader = open(&wav);
    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), MP3_NUM_FRAMES as usize);
}

#[test]
fn decodes_to_real_audio_samples() {
    let wav = mpeg_wav(WAVE_FORMAT_MPEGLAYER3, 1, MP3_STREAM);
    let mut reader = open(&wav);

    let samples = decode_all(&mut reader);

    // 5 frames * 1152 samples, mono.
    assert_eq!(samples.len(), MP3_NUM_SAMPLES as usize);

    // A 440 Hz sine tone must produce non-silent output.
    assert!(samples.iter().any(|&s| s.abs() > 0.01));
}

#[test]
fn fact_fallback_when_frame_count_unavailable() {
    // Without a Fact chunk the seekable stream still gets an exact frame count.
    let wav = riff_wave(&[fmt_chunk(WAVE_FORMAT_MPEGLAYER3, 1, 44_100), data_chunk(MP3_STREAM)]);

    let reader = open(&wav);
    assert_eq!(reader.tracks()[0].num_frames, Some(MP3_NUM_SAMPLES));
}

#[test]
fn resyncs_past_garbage_before_first_frame() {
    let mut data = Vec::new();
    data.extend_from_slice(&[0u8; 16]);
    data.extend_from_slice(MP3_STREAM);

    let wav = mpeg_wav(WAVE_FORMAT_MPEGLAYER3, 1, &data);
    let mut reader = open(&wav);

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), MP3_NUM_FRAMES as usize);

    // The recovered packet stream must be the original frames.
    assert_eq!(packets[0].2, MP3_STREAM[..packets[0].2.len()]);
}

#[test]
fn truncated_final_frame_is_ignored() {
    // Drop the last 10 bytes so the final frame is incomplete.
    let data = &MP3_STREAM[..MP3_STREAM.len() - 10];
    let wav = mpeg_wav(WAVE_FORMAT_MPEGLAYER3, 1, data);
    let mut reader = open(&wav);

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), (MP3_NUM_FRAMES - 1) as usize);
    assert_eq!(reader.tracks()[0].num_frames, Some((MP3_NUM_FRAMES - 1) * 1152));
}

#[test]
fn unknown_format_tag_is_still_rejected() {
    let wav = mpeg_wav(WAVE_FORMAT_UNKNOWN, 1, MP3_STREAM);
    let mss = MediaSourceStream::new(Box::new(Cursor::new(wav)), Default::default());

    let result = WavReader::try_new(mss, FormatOptions::default());
    assert!(matches!(result, Err(Error::Unsupported(_))));
}

#[test]
fn seek_returns_frame_at_or_before_timestamp() {
    let wav = mpeg_wav(WAVE_FORMAT_MPEGLAYER3, 1, MP3_STREAM);
    let mut reader = open(&wav);

    // Requesting a frame boundary lands on it exactly.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::from(2 * 1152), track_id: 0 })
        .expect("seek should succeed");
    assert_eq!(seeked.actual_ts, Timestamp::from(2 * 1152));

    let packet = reader.next_packet().expect("a packet should follow the seek").expect("packet");
    assert_eq!(packet.pts, Timestamp::from(2 * 1152));

    // Requesting a non-boundary timestamp lands on the preceding frame.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::from(3000), track_id: 0 })
        .expect("seek should succeed");
    assert_eq!(seeked.actual_ts, Timestamp::from(2 * 1152));

    // A timestamp past the end is out of range.
    let result = reader.seek(
        SeekMode::Accurate,
        SeekTo::Timestamp { ts: Timestamp::from(MP3_NUM_SAMPLES as u32 + 1), track_id: 0 },
    );
    assert!(matches!(result, Err(Error::SeekError(SeekErrorKind::OutOfRange))));
}

#[test]
fn seeked_packet_stream_matches_linear_read() {
    let wav = mpeg_wav(WAVE_FORMAT_MPEGLAYER3, 1, MP3_STREAM);

    let linear = {
        let mut reader = open(&wav);
        collect_packets(&mut reader)
    };

    let mut reader = open(&wav);
    reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::from(3 * 1152), track_id: 0 })
        .expect("seek should succeed");

    let after_seek = collect_packets(&mut reader);

    assert_eq!(after_seek.len(), 2);
    assert_eq!(after_seek[0], linear[3]);
    assert_eq!(after_seek[1], linear[4]);
}

#[test]
fn forward_only_source_decodes_but_cannot_seek_backwards() {
    let wav = mpeg_wav(WAVE_FORMAT_MPEGLAYER3, 1, MP3_STREAM);

    let mss =
        MediaSourceStream::new(Box::new(ReadOnlySource::new(Cursor::new(wav))), Default::default());
    let mut reader = WavReader::try_new(mss, FormatOptions::default()).expect("probe should work");

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), MP3_NUM_FRAMES as usize);

    let result =
        reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::ZERO, track_id: 0 });
    assert!(matches!(result, Err(Error::SeekError(SeekErrorKind::ForwardOnly))));
}

#[test]
fn streaming_wave_with_unknown_length_decodes_to_graceful_eof() {
    // A streaming WAVE marks both the RIFF and data chunk lengths as unknown. Combined with a
    // forward-only source there is no end boundary or seek index, so the reader must stop cleanly
    // at the real EOF instead of propagating an unexpected-EOF error.
    //
    // The final frame keeps its 4-byte header but loses most of its body, forcing the truncation
    // to be detected while reading the frame body rather than while syncing to the next header.
    // Frame sizes in this stream are 417, 418, 418, 418, 418 bytes.
    let truncated = &MP3_STREAM[..417 + 418 + 418 + 418 + 10];

    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    body.extend_from_slice(&fmt_chunk(WAVE_FORMAT_MPEGLAYER3, 1, 44_100));
    body.extend_from_slice(b"data");
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    body.extend_from_slice(truncated);

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&u32::MAX.to_le_bytes());
    wav.extend_from_slice(&body);

    let mss =
        MediaSourceStream::new(Box::new(ReadOnlySource::new(Cursor::new(wav))), Default::default());
    let mut reader = WavReader::try_new(mss, FormatOptions::default()).expect("probe should work");

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), (MP3_NUM_FRAMES - 1) as usize);
    assert_eq!(packets.last().expect("at least one packet").0, (MP3_NUM_FRAMES - 2) * 1152);
}

#[test]
fn pcm_wave_is_unchanged() {
    // A tiny 4-sample mono s16 PCM stream exercises the non-MPEG packetization path.
    let pcm = [0i16, 1000, -1000, i16::MAX];
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&8_000u32.to_le_bytes());
    fmt.extend_from_slice(&16_000u32.to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes()); // block align
    fmt.extend_from_slice(&16u16.to_le_bytes());

    let mut data = Vec::new();
    for sample in pcm {
        data.extend_from_slice(&sample.to_le_bytes());
    }

    let wav = riff_wave(&[chunk(b"fmt ", &fmt), data_chunk(&data)]);
    let mut reader = open(&wav);

    let track = &reader.tracks()[0];
    assert_eq!(track.num_frames, Some(4));

    let packets = collect_packets(&mut reader);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].1, 4);
    assert_eq!(packets[0].2, data);
}
