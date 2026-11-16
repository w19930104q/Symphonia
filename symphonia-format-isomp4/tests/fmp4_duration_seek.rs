// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Regression test: for fragmented MP4 streams, the duration of each track must be
//! consistent with the track's addressable range (the range of timestamps that can be
//! seeked to and read back as packets).
//!
//! The fixed sample used here is a fragmented MP4 with two audio tracks whose `mdhd`
//! durations are 0 (as produced by common packagers), and whose `sidx` atoms declare
//! durations that intentionally do NOT match the actual fragment contents. The only
//! source of truth that is consistent with seeking is the accumulation of the `trun`
//! durations across all `moof` atoms.

use std::io::Cursor;

use symphonia_core::errors::{Error, SeekErrorKind};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::{TimeBase, Timestamp};

use symphonia_format_isomp4::IsoMp4Reader;

// ---------------------------------------------------------------------------------------------
// Fixed fMP4 sample builder.
// ---------------------------------------------------------------------------------------------

fn atom(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(payload);
    out
}

fn full_atom(typ: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + payload.len());
    body.push(version);
    body.extend_from_slice(&flags.to_be_bytes()[1..]);
    body.extend_from_slice(payload);
    atom(typ, &body)
}

fn ftyp() -> Vec<u8> {
    atom(b"ftyp", b"isom\0\0\0\0isomiso6")
}

fn mvhd() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification time
    p.extend_from_slice(&1000u32.to_be_bytes()); // movie timescale
    p.extend_from_slice(&0u32.to_be_bytes()); // duration (0 for fragmented)
    p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // preferred rate
    p.extend_from_slice(&0x0100u16.to_be_bytes()); // preferred volume
    full_atom(b"mvhd", 0, 0, &p)
}

fn tkhd(track_id: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification time
    p.extend_from_slice(&track_id.to_be_bytes()); // track id
    p.extend_from_slice(&0u32.to_be_bytes()); // reserved
    p.extend_from_slice(&0u32.to_be_bytes()); // duration (0 for fragmented)
    p.extend_from_slice(&[0u8; 8]); // reserved
    p.extend_from_slice(&0u16.to_be_bytes()); // layer
    p.extend_from_slice(&0u16.to_be_bytes()); // alternate group
    p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
    full_atom(b"tkhd", 0, 0x3, &p)
}

fn mdhd(timescale: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // creation time
    p.extend_from_slice(&0u32.to_be_bytes()); // modification time
    p.extend_from_slice(&timescale.to_be_bytes()); // media timescale
    p.extend_from_slice(&0u32.to_be_bytes()); // duration (0 for fragmented)
    p.extend_from_slice(&0x55c4u16.to_be_bytes()); // language: und
    p.extend_from_slice(&0u16.to_be_bytes()); // quality
    full_atom(b"mdhd", 0, 0, &p)
}

fn hdlr() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes()); // pre-defined
    p.extend_from_slice(b"soun"); // handler type
    p.extend_from_slice(&[0u8; 12]); // reserved
    p.extend_from_slice(b"SoundHandler\0");
    full_atom(b"hdlr", 0, 0, &p)
}

fn smhd() -> Vec<u8> {
    full_atom(b"smhd", 0, 0, &[0u8; 4])
}

/// A `sowt` (PCM signed 16-bit little-endian) audio sample entry.
fn stsd(sample_rate: u32) -> Vec<u8> {
    let mut entry = Vec::new();
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    entry.extend_from_slice(&0u16.to_be_bytes()); // version
    entry.extend_from_slice(&0u16.to_be_bytes()); // revision
    entry.extend_from_slice(&0u32.to_be_bytes()); // vendor
    entry.extend_from_slice(&2u16.to_be_bytes()); // channels
    entry.extend_from_slice(&16u16.to_be_bytes()); // sample size
    entry.extend_from_slice(&0u16.to_be_bytes()); // compression id
    entry.extend_from_slice(&0u16.to_be_bytes()); // packet size
    entry.extend_from_slice(&(sample_rate << 16).to_be_bytes()); // sample rate (16.16)

    let entry = atom(b"sowt", &entry);

    let mut p = Vec::new();
    p.extend_from_slice(&1u32.to_be_bytes()); // entry count
    p.extend_from_slice(&entry);
    full_atom(b"stsd", 0, 0, &p)
}

fn stbl(sample_rate: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&stsd(sample_rate));
    // Empty sample tables. All samples are provided by fragments.
    p.extend_from_slice(&full_atom(b"stts", 0, 0, &0u32.to_be_bytes()));
    p.extend_from_slice(&full_atom(b"stsc", 0, 0, &0u32.to_be_bytes()));
    p.extend_from_slice(&full_atom(b"stsz", 0, 0, &[0u8; 8]));
    p.extend_from_slice(&full_atom(b"stco", 0, 0, &0u32.to_be_bytes()));
    atom(b"stbl", &p)
}

fn trak(track_id: u32, timescale: u32) -> Vec<u8> {
    let mut minf = Vec::new();
    minf.extend_from_slice(&smhd());
    minf.extend_from_slice(&stbl(timescale));

    let mut mdia = Vec::new();
    mdia.extend_from_slice(&mdhd(timescale));
    mdia.extend_from_slice(&hdlr());
    mdia.extend_from_slice(&atom(b"minf", &minf));

    let mut p = Vec::new();
    p.extend_from_slice(&tkhd(track_id));
    p.extend_from_slice(&atom(b"mdia", &mdia));
    atom(b"trak", &p)
}

fn trex(track_id: u32, default_dur: u32, default_size: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&track_id.to_be_bytes());
    p.extend_from_slice(&1u32.to_be_bytes()); // default sample description index
    p.extend_from_slice(&default_dur.to_be_bytes()); // default sample duration
    p.extend_from_slice(&default_size.to_be_bytes()); // default sample size
    p.extend_from_slice(&0u32.to_be_bytes()); // default sample flags
    full_atom(b"trex", 0, 0, &p)
}

fn moov() -> Vec<u8> {
    let mut mvex = Vec::new();
    mvex.extend_from_slice(&trex(TRACK1.id, TRACK1.default_dur, TRACK1.default_size));
    mvex.extend_from_slice(&trex(TRACK2.id, TRACK2.default_dur, TRACK2.default_size));

    let mut p = Vec::new();
    p.extend_from_slice(&mvhd());
    p.extend_from_slice(&trak(TRACK1.id, TRACK1.timescale));
    p.extend_from_slice(&trak(TRACK2.id, TRACK2.timescale));
    p.extend_from_slice(&atom(b"mvex", &mvex));
    atom(b"moov", &p)
}

/// A sidx atom whose declared duration intentionally does NOT match the actual
/// fragment contents. The demuxer must not trust it over the fragments themselves.
fn sidx(track_id: u32, timescale: u32, wrong_duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&track_id.to_be_bytes()); // reference id
    p.extend_from_slice(&timescale.to_be_bytes()); // timescale
    p.extend_from_slice(&0u32.to_be_bytes()); // earliest presentation time
    p.extend_from_slice(&0u32.to_be_bytes()); // first offset
    p.extend_from_slice(&0u16.to_be_bytes()); // reserved
    p.extend_from_slice(&1u16.to_be_bytes()); // reference count
    p.extend_from_slice(&1000u32.to_be_bytes()); // reference (type 0, size 1000)
    p.extend_from_slice(&wrong_duration.to_be_bytes()); // subsegment duration
    p.extend_from_slice(&0u32.to_be_bytes()); // SAP
    full_atom(b"sidx", 0, 0, &p)
}

fn tfhd(track_id: u32, default_dur: u32, default_size: u32) -> Vec<u8> {
    // Flags: default-base-is-moof | default-sample-duration-present |
    // default-sample-size-present.
    const FLAGS: u32 = 0x02_0000 | 0x8 | 0x10;

    let mut p = Vec::new();
    p.extend_from_slice(&track_id.to_be_bytes());
    p.extend_from_slice(&default_dur.to_be_bytes());
    p.extend_from_slice(&default_size.to_be_bytes());
    full_atom(b"tfhd", 0, FLAGS, &p)
}

/// A trun atom. If `durations` is `Some`, per-sample durations are recorded explicitly,
/// otherwise all samples use the default duration from the tfhd atom.
fn trun(data_offset: i32, sample_count: u32, durations: Option<&[u32]>) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&sample_count.to_be_bytes());
    p.extend_from_slice(&data_offset.to_be_bytes());

    let flags = match durations {
        Some(durs) => {
            assert_eq!(durs.len(), sample_count as usize);
            for dur in durs {
                p.extend_from_slice(&dur.to_be_bytes());
            }
            // Flags: data-offset-present | sample-duration-present.
            0x1 | 0x100
        }
        None => 0x1,
    };

    full_atom(b"trun", 0, flags, &p)
}

struct TrackSpec {
    id: u32,
    timescale: u32,
    default_dur: u32,
    default_size: u32,
}

struct TrunSpec {
    sample_count: u32,
    durations: Option<Vec<u32>>,
}

/// Builds one moof + mdat pair. Each track contributes one traf with one trun. The sample
/// data of both tracks is placed in the single mdat following the moof, track 1 first.
fn fragment(seq: u32, t1: &TrunSpec, t2: &TrunSpec) -> Vec<u8> {
    let t1_data_len = t1.sample_count * TRACK1.default_size;
    let t2_data_len = t2.sample_count * TRACK2.default_size;

    // Build the moof once to determine its size, then rebuild it with the computed data
    // offsets. The offsets do not affect the size of the moof.
    let build_moof = |t1_offset: i32, t2_offset: i32| {
        let traf1 = {
            let mut p = Vec::new();
            p.extend_from_slice(&tfhd(TRACK1.id, TRACK1.default_dur, TRACK1.default_size));
            p.extend_from_slice(&trun(t1_offset, t1.sample_count, t1.durations.as_deref()));
            atom(b"traf", &p)
        };

        let traf2 = {
            let mut p = Vec::new();
            p.extend_from_slice(&tfhd(TRACK2.id, TRACK2.default_dur, TRACK2.default_size));
            p.extend_from_slice(&trun(t2_offset, t2.sample_count, t2.durations.as_deref()));
            atom(b"traf", &p)
        };

        let mut moof = Vec::new();
        moof.extend_from_slice(&full_atom(b"mfhd", 0, 0, &seq.to_be_bytes()));
        moof.extend_from_slice(&traf1);
        moof.extend_from_slice(&traf2);
        atom(b"moof", &moof)
    };

    let moof_len = build_moof(0, 0).len() as i32;

    // Data offsets are relative to the start of the moof atom (default-base-is-moof).
    let t1_offset = moof_len + 8;
    let t2_offset = t1_offset + t1_data_len as i32;

    let moof = build_moof(t1_offset, t2_offset);

    let mut mdat_payload = vec![0u8; (t1_data_len + t2_data_len) as usize];
    // Fill track 1's data with a non-zero pattern so the packets of the two tracks differ.
    for (i, byte) in mdat_payload[..t1_data_len as usize].iter_mut().enumerate() {
        *byte = (i & 0xff) as u8;
    }

    let mut out = moof;
    out.extend_from_slice(&atom(b"mdat", &mdat_payload));
    out
}

const TRACK1: TrackSpec =
    TrackSpec { id: 1, timescale: 48_000, default_dur: 1024, default_size: 4 };

const TRACK2: TrackSpec =
    TrackSpec { id: 2, timescale: 44_100, default_dur: 1024, default_size: 4 };

/// Expected total duration of each track in its own timescale units. Both tracks are
/// exactly 3 seconds long.
const TRACK1_DURATION: u64 = 144_000;
const TRACK2_DURATION: u64 = 132_300;

/// Expected number of samples (packets) of each track.
const TRACK1_SAMPLES: u64 = 140;
const TRACK2_SAMPLES: u64 = 130;

/// Expected timestamp of the last sample of each track.
const TRACK1_LAST_TS: i64 = 142_336;
const TRACK2_LAST_TS: i64 = 132_096;

/// Builds the fixed fragmented MP4 sample.
///
/// Track 1 (id=1, timescale 48000): fragments of 60, 60, and 20 samples. The last
/// fragment records explicit per-sample durations (19 x 1024 + 1664).
///
/// Track 2 (id=2, timescale 44100): fragments of 55, 55, and 20 samples. The last
/// fragment records explicit per-sample durations (19 x 1024 + 204).
fn make_sample() -> Vec<u8> {
    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov());
    // The sidx durations intentionally disagree with the fragments.
    file.extend_from_slice(&sidx(TRACK1.id, TRACK1.timescale, 999_999));
    file.extend_from_slice(&sidx(TRACK2.id, TRACK2.timescale, 888_888));

    // Fragment 1.
    file.extend_from_slice(&fragment(
        1,
        &TrunSpec { sample_count: 60, durations: None },
        &TrunSpec { sample_count: 55, durations: None },
    ));

    // Fragment 2.
    file.extend_from_slice(&fragment(
        2,
        &TrunSpec { sample_count: 60, durations: None },
        &TrunSpec { sample_count: 55, durations: None },
    ));

    // Fragment 3, with explicit per-sample durations.
    let mut t1_durs = vec![1024u32; 19];
    t1_durs.push(1664);

    let mut t2_durs = vec![1024u32; 19];
    t2_durs.push(204);

    file.extend_from_slice(&fragment(
        3,
        &TrunSpec { sample_count: 20, durations: Some(t1_durs) },
        &TrunSpec { sample_count: 20, durations: Some(t2_durs) },
    ));

    file
}

// ---------------------------------------------------------------------------------------------
// Test.
// ---------------------------------------------------------------------------------------------

fn open(data: Vec<u8>) -> IsoMp4Reader<'static> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    IsoMp4Reader::try_new(mss, FormatOptions::default()).expect("failed to open fMP4 sample")
}

#[test]
fn fmp4_track_durations_match_addressable_range() {
    let mut reader = open(make_sample());

    // The duration and frame count of each track must be derived from the fragments and
    // must equal the end of the addressable timestamp range. Neither the (zero) mdhd
    // durations nor the (incorrect) sidx durations may be used.
    let tracks = reader.tracks();
    assert_eq!(tracks.len(), 2);

    let track1 = &tracks[0];
    assert_eq!(track1.id, TRACK1.id);
    assert_eq!(track1.time_base, Some(TimeBase::from_recip(TRACK1.timescale.try_into().unwrap())));
    assert_eq!(track1.duration.map(|d| d.get()), Some(TRACK1_DURATION));
    assert_eq!(track1.num_frames, Some(TRACK1_DURATION));

    let track2 = &tracks[1];
    assert_eq!(track2.id, TRACK2.id);
    assert_eq!(track2.time_base, Some(TimeBase::from_recip(TRACK2.timescale.try_into().unwrap())));
    assert_eq!(track2.duration.map(|d| d.get()), Some(TRACK2_DURATION));
    assert_eq!(track2.num_frames, Some(TRACK2_DURATION));

    // Read all packets. The number of packets per track must equal the number of samples
    // declared by the fragments, and the end of the last packet must equal the track's
    // duration.
    let mut counts = [0u64; 2];
    let mut last_ends = [0u64; 2];

    while let Some(packet) = reader.next_packet().expect("failed to read packet") {
        let idx = match packet.track_id {
            id if id == TRACK1.id => 0,
            id if id == TRACK2.id => 1,
            id => panic!("unexpected track id {id}"),
        };

        counts[idx] += 1;
        last_ends[idx] = last_ends[idx].max(packet.pts.get() as u64 + packet.dur.get());
    }

    assert_eq!(counts, [TRACK1_SAMPLES, TRACK2_SAMPLES]);
    assert_eq!(last_ends, [TRACK1_DURATION, TRACK2_DURATION]);

    // Seek to the start of the stream.
    let seeked = reader
        .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: TRACK1.id })
        .expect("seek to start failed");
    assert_eq!(seeked.actual_ts, Timestamp::new(0));

    // Seek to a fragment boundary.
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(61_440), track_id: TRACK1.id },
        )
        .expect("seek to fragment boundary failed");
    assert_eq!(seeked.actual_ts, Timestamp::new(61_440));

    // Seek to the last addressable timestamp of track 1. This must land on the last
    // sample of the track.
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new(TRACK1_DURATION as i64 - 1),
                track_id: TRACK1.id,
            },
        )
        .expect("seek to end of track 1 failed");
    assert_eq!(seeked.actual_ts, Timestamp::new(TRACK1_LAST_TS));

    // Seek to the last addressable timestamp of track 2.
    let seeked = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new(TRACK2_DURATION as i64 - 1),
                track_id: TRACK2.id,
            },
        )
        .expect("seek to end of track 2 failed");
    assert_eq!(seeked.actual_ts, Timestamp::new(TRACK2_LAST_TS));

    // Seeking to exactly the duration of a track is out-of-range since no sample contains
    // that timestamp.
    for (track_id, duration) in [(TRACK1.id, TRACK1_DURATION), (TRACK2.id, TRACK2_DURATION)] {
        let result = reader.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(duration as i64), track_id },
        );

        assert!(
            matches!(result, Err(Error::SeekError(SeekErrorKind::OutOfRange))),
            "seek to ts={duration} of track {track_id} should be out-of-range, got {result:?}",
        );
    }
}
