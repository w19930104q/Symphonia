// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Packetization of MPEG audio streams embedded in a WAVE data chunk.
//!
//! Unlike PCM and the block-based WAVE codecs, MPEG audio is a self-framing stream: each frame
//! begins with an 11-bit sync word and carries its own size. The WAVE demuxer therefore re-syncs
//! to every frame and emits one packet per MPEG frame, matching the packetization of the native
//! MPEG elementary stream reader so that the same decoder input produces the same samples.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};
use symphonia_core::errors::{Error, Result, SeekErrorKind, decode_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::{MediaSourceStream, ReadBytes, SeekBuffered};
use symphonia_core::units::Duration;

use log::warn;

/// The length in bytes of a MPEG frame header word.
const MPEG_HEADER_LEN: usize = 4;

/// The MPEG audio version.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MpegVersion {
    /// Version 2.5
    Mpeg2p5,
    /// Version 2
    Mpeg2,
    /// Version 1
    Mpeg1,
}

/// The MPEG audio layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MpegLayer {
    /// Layer 1
    Layer1,
    /// Layer 2
    Layer2,
    /// Layer 3
    Layer3,
}

/// A parsed MPEG audio frame header.
#[derive(Copy, Clone, Debug)]
pub struct MpegFrameHeader {
    version: MpegVersion,
    layer: MpegLayer,
    sample_rate: u32,
    /// The size of the frame, including the optional CRC, but excluding the standard frame
    /// header.
    frame_size: u64,
}

impl MpegFrameHeader {
    /// The codec ID of the frame's MPEG layer.
    pub fn codec(&self) -> AudioCodecId {
        match self.layer {
            MpegLayer::Layer1 => CODEC_ID_MP1,
            MpegLayer::Layer2 => CODEC_ID_MP2,
            MpegLayer::Layer3 => CODEC_ID_MP3,
        }
    }

    /// The frame's sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The number of per-channel audio samples decoded from the frame.
    pub fn num_frames(&self) -> u64 {
        match self.layer {
            MpegLayer::Layer1 => 384,
            MpegLayer::Layer2 => 1152,
            // Layer 3 frames hold two granules for MPEG-1 and one for MPEG-2/2.5.
            MpegLayer::Layer3 => 576 * if self.version == MpegVersion::Mpeg1 { 2 } else { 1 },
        }
    }

    /// The total on-wire size of the frame, including the 4-byte header word.
    pub fn total_len(&self) -> u64 {
        MPEG_HEADER_LEN as u64 + self.frame_size
    }
}

/// Bit-rate lookup table for MPEG version 1 layer 1.
const BIT_RATES_MPEG1_L1: [u32; 15] = [
    0, 32_000, 64_000, 96_000, 128_000, 160_000, 192_000, 224_000, 256_000, 288_000, 320_000,
    352_000, 384_000, 416_000, 448_000,
];

/// Bit-rate lookup table for MPEG version 1 layer 2.
const BIT_RATES_MPEG1_L2: [u32; 15] = [
    0, 32_000, 48_000, 56_000, 64_000, 80_000, 96_000, 112_000, 128_000, 160_000, 192_000, 224_000,
    256_000, 320_000, 384_000,
];

/// Bit-rate lookup table for MPEG version 1 layer 3.
const BIT_RATES_MPEG1_L3: [u32; 15] = [
    0, 32_000, 40_000, 48_000, 56_000, 64_000, 80_000, 96_000, 112_000, 128_000, 160_000, 192_000,
    224_000, 256_000, 320_000,
];

/// Bit-rate lookup table for MPEG version 2 & 2.5 audio layer 1.
const BIT_RATES_MPEG2_L1: [u32; 15] = [
    0, 32_000, 48_000, 56_000, 64_000, 80_000, 96_000, 112_000, 128_000, 144_000, 160_000, 176_000,
    192_000, 224_000, 256_000,
];

/// Bit-rate lookup table for MPEG version 2 & 2.5 audio layers 2 & 3.
const BIT_RATES_MPEG2_L23: [u32; 15] = [
    0, 8_000, 16_000, 24_000, 32_000, 40_000, 48_000, 56_000, 64_000, 80_000, 96_000, 112_000,
    128_000, 144_000, 160_000,
];

/// Returns true if the provided frame header word begins with the MPEG audio sync word.
#[inline(always)]
fn is_synced(header: u32) -> bool {
    (header & 0xffe0_0000) == 0xffe0_0000
}

/// Quick validity check for a candidate frame header word.
#[inline]
fn check_header(header: u32) -> bool {
    // Version (0x1 is reserved).
    if (header >> 19) & 0x3 == 0x1 {
        return false;
    }
    // Layer (0x0 is reserved).
    if (header >> 17) & 0x3 == 0x0 {
        return false;
    }
    // Bitrate index 0 (free) and 0xf (reserved) are not supported.
    if (header >> 12) & 0xf == 0xf {
        return false;
    }
    // Sample rate (0x3 is reserved).
    if (header >> 10) & 0x3 == 0x3 {
        return false;
    }
    true
}

/// Synchronize the stream to the next MPEG frame header word. On success the stream is positioned
/// immediately after the header word.
fn sync_frame(reader: &mut MediaSourceStream<'_>) -> Result<u32> {
    let mut sync = 0u32;

    loop {
        while !is_synced(sync) {
            sync = (sync << 8) | u32::from(reader.read_u8()?);
        }

        if check_header(sync) {
            break;
        }

        sync = (sync << 8) | u32::from(reader.read_u8()?);
    }

    Ok(sync)
}

/// Parse a synced MPEG frame header word into an [`MpegFrameHeader`].
pub fn parse_frame_header(header: u32) -> Result<MpegFrameHeader> {
    // The MPEG audio header is structured as follows:
    //
    // 0b1111_1111 0b111v_vlly 0brrrr_hhpx 0bmmmm_coee
    // where:
    //     vv   = version, ll = layer      , y = crc
    //     rrrr = bitrate, hh = sample rate, p = padding , x  = private bit
    //     mmmm = mode   , c  = copyright  , o = original, ee = emphasis

    let version = match (header & 0x18_0000) >> 19 {
        0b00 => MpegVersion::Mpeg2p5,
        0b10 => MpegVersion::Mpeg2,
        0b11 => MpegVersion::Mpeg1,
        _ => return decode_error("wav (mpeg): invalid MPEG version"),
    };

    let layer = match (header & 0x6_0000) >> 17 {
        0b01 => MpegLayer::Layer3,
        0b10 => MpegLayer::Layer2,
        0b11 => MpegLayer::Layer1,
        _ => return decode_error("wav (mpeg): invalid MPEG layer"),
    };

    let bitrate_index = ((header & 0xf000) >> 12) as usize;

    // "Free" bit-rate streams do not use a fixed frame size derivable from the header and are not
    // supported. Index 0xf is reserved. Both are also rejected up-front by check_header, but guard
    // the table lookup explicitly since parse_frame_header may be called directly.
    if bitrate_index == 0 {
        return unsupported_error("wav (mpeg): free bit-rate is not supported");
    }
    else if bitrate_index == 0xf {
        return decode_error("wav (mpeg): invalid bit-rate index");
    }

    let bitrate = match (version, layer) {
        (MpegVersion::Mpeg1, MpegLayer::Layer1) => BIT_RATES_MPEG1_L1[bitrate_index],
        (MpegVersion::Mpeg1, MpegLayer::Layer2) => BIT_RATES_MPEG1_L2[bitrate_index],
        (MpegVersion::Mpeg1, MpegLayer::Layer3) => BIT_RATES_MPEG1_L3[bitrate_index],
        (_, MpegLayer::Layer1) => BIT_RATES_MPEG2_L1[bitrate_index],
        (_, _) => BIT_RATES_MPEG2_L23[bitrate_index],
    };

    let sample_rate = match (((header & 0xc00) >> 10) as usize, version) {
        (0b00, MpegVersion::Mpeg1) => 44_100,
        (0b01, MpegVersion::Mpeg1) => 48_000,
        (0b10, MpegVersion::Mpeg1) => 32_000,
        (0b00, MpegVersion::Mpeg2) => 22_050,
        (0b01, MpegVersion::Mpeg2) => 24_000,
        (0b10, MpegVersion::Mpeg2) => 16_000,
        (0b00, MpegVersion::Mpeg2p5) => 11_025,
        (0b01, MpegVersion::Mpeg2p5) => 12_000,
        (0b10, MpegVersion::Mpeg2p5) => 8_000,
        _ => return decode_error("wav (mpeg): invalid sample rate"),
    };

    let has_padding = header & 0x200 != 0;

    // Constants provided for size calculation in ISO/IEC 11172 section 2.4.3.1.
    let factor = match layer {
        MpegLayer::Layer1 => 12,
        MpegLayer::Layer2 => 144,
        MpegLayer::Layer3 if version == MpegVersion::Mpeg1 => 144,
        MpegLayer::Layer3 => 72,
    };

    // For layer 1 a slot is 4 bytes, for layers 2 and 3 a slot is 1 byte.
    let slot_size = match layer {
        MpegLayer::Layer1 => 4,
        _ => 1,
    };

    // Calculate the total frame size in number of slots.
    let frame_size_slots =
        factor * bitrate as usize / sample_rate as usize + usize::from(has_padding);

    // The frame size excludes the 4-byte header word, matching the native MPEG demuxer.
    let frame_size = (frame_size_slots * slot_size) as u64 - MPEG_HEADER_LEN as u64;

    Ok(MpegFrameHeader { version, layer, sample_rate, frame_size })
}

/// Synchronize and parse the next MPEG frame header, returning the raw header word as well. On
/// success the stream is positioned immediately after the header word, ready for the frame body
/// to be read.
fn read_frame(reader: &mut MediaSourceStream<'_>) -> Result<(u32, MpegFrameHeader)> {
    loop {
        let sync = sync_frame(reader)?;

        match parse_frame_header(sync) {
            Ok(header) => break Ok((sync, header)),
            // A plausible sync word may still parse to an invalid header. Resync from the byte
            // after it and keep scanning.
            Err(Error::DecodeError(err)) => warn!("wav (mpeg): skipping invalid frame: {err}"),
            Err(err) => return Err(err),
        }
    }
}

/// Packetizes an MPEG audio stream embedded in a WAVE data chunk.
///
/// Each emitted packet contains exactly one MPEG frame, which is the packetization expected by the
/// MPEG audio decoder. Packet timestamps are the running count of decoded samples.
pub struct MpegPacketizer {
    /// The absolute position of the first byte of the data chunk.
    data_start_pos: u64,
    /// The exclusive end position of the data chunk, or `None` if the length is unknown.
    data_end_pos: Option<u64>,
    /// The timestamp of the next packet to emit.
    next_ts: Timestamp,
}

impl MpegPacketizer {
    pub fn new(data_start_pos: u64, data_end_pos: Option<u64>) -> Self {
        MpegPacketizer { data_start_pos, data_end_pos, next_ts: Timestamp::ZERO }
    }

    /// Peek the first frame header in the data chunk without consuming it. The stream must be
    /// positioned at the start of the data chunk.
    pub fn peek_first_header(
        reader: &mut MediaSourceStream<'_>,
        data_end_pos: Option<u64>,
    ) -> Result<MpegFrameHeader> {
        let start_pos = reader.pos();

        let (_, header) = read_frame(reader)?;

        // Rewind to the start of the frame so it can be packetized later.
        reader.seek_buffered(start_pos);

        // A frame claiming to extend past the data chunk cannot be the real first frame.
        if let Some(end_pos) = data_end_pos {
            if start_pos + header.total_len() > end_pos {
                return decode_error("wav (mpeg): first frame exceeds data chunk");
            }
        }

        Ok(header)
    }

    /// Read the next MPEG frame and return it as a packet, or `None` at the end of the data chunk.
    pub fn next_packet(&mut self, reader: &mut MediaSourceStream<'_>) -> Result<Option<Packet>> {
        let end_pos = self.data_end_pos.unwrap_or(u64::MAX);

        if reader.pos() >= end_pos {
            return Ok(None);
        }

        let frame_pos = reader.pos();

        let (header_word, header) = match read_frame(reader) {
            Ok(frame) => frame,
            // An unexpected EOF while syncing means the stream ended without a complete frame.
            Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };

        // A frame that runs past the end of the chunk is a truncated final frame.
        if frame_pos + header.total_len() > end_pos {
            return Ok(None);
        }

        // Read the frame body, including the optional CRC. The header word is prepended so the
        // packet is laid out exactly like one from the native MPEG demuxer.
        let body_len = header.frame_size as usize;

        let mut packet = vec![0u8; MPEG_HEADER_LEN + body_len];
        packet[..MPEG_HEADER_LEN].copy_from_slice(&header_word.to_be_bytes());

        match reader.read_buf_exact(&mut packet[MPEG_HEADER_LEN..]) {
            Ok(()) => {}
            // The body of a final frame may be truncated when the data chunk length is unknown
            // (e.g. a streaming WAVE). Treat it as the end of the stream rather than failing.
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(Error::IoError(err)),
        }

        let pts = self.next_ts;
        let dur = Duration::from(header.num_frames());

        self.next_ts = match self.next_ts.checked_add(dur) {
            Some(ts) => ts,
            None => return Ok(None),
        };

        Ok(Some(Packet::new(0, pts, dur, packet)))
    }

    /// Seek to the frame whose timestamp is at or before the requested timestamp.
    ///
    /// MPEG frames are self-framing but do not have a fixed size, so seeking is implemented by
    /// re-syncing to frames from the start of the data chunk and accumulating their durations.
    pub fn seek(
        &mut self,
        reader: &mut MediaSourceStream<'_>,
        required_ts: Timestamp,
    ) -> Result<SeekedTo> {
        reader.seek(SeekFrom::Start(self.data_start_pos))?;

        let end_pos = self.data_end_pos.unwrap_or(u64::MAX);
        let mut actual_ts = Timestamp::ZERO;

        loop {
            let frame_pos = reader.pos();

            if frame_pos >= end_pos {
                break;
            }

            let (_, header) = match read_frame(reader) {
                Ok(frame) => frame,
                // Reaching the end of the source while syncing ends the scan.
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(err) => return Err(err),
            };

            // A truncated final frame ends the scan.
            if frame_pos + header.total_len() > end_pos {
                reader.seek_buffered(frame_pos);
                break;
            }

            let frame_dur = Duration::from(header.num_frames());
            let next_ts = actual_ts
                .checked_add(frame_dur)
                .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

            if next_ts > required_ts {
                // The next frame would start after the requested timestamp, so seek back to the
                // current frame.
                reader.seek_buffered(frame_pos);
                break;
            }

            actual_ts = next_ts;

            // Skip the frame body.
            reader.ignore_bytes(header.frame_size)?;
        }

        self.next_ts = actual_ts;

        Ok(SeekedTo { track_id: 0, actual_ts, required_ts })
    }

    /// Count the number of decoded samples in the data chunk by walking every MPEG frame header.
    ///
    /// The stream is rewound to its original position afterwards. This is used for seekable
    /// streams where the exact frame count is preferable to the Fact chunk estimate. A truncated
    /// final frame simply ends the count rather than failing the whole scan.
    pub fn count_samples(reader: &mut MediaSourceStream<'_>, data_len: u64) -> Result<u64> {
        let start_pos = reader.pos();
        let end_pos = start_pos + data_len;

        let mut total = 0u64;

        let result = (|| -> Result<u64> {
            while reader.pos() < end_pos {
                let frame_pos = reader.pos();

                let (_, header) = match read_frame(reader) {
                    Ok(frame) => frame,
                    // A truncated final frame ends the scan.
                    Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        break;
                    }
                    Err(err) => return Err(err),
                };

                // Stop on a frame that would run past the end of the chunk rather than trusting a
                // false sync in trailing bytes.
                if frame_pos + header.total_len() > end_pos {
                    break;
                }

                match reader.ignore_bytes(header.frame_size) {
                    Ok(()) => total += header.num_frames(),
                    // The body of the final frame is truncated.
                    Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        break;
                    }
                    Err(err) => return Err(Error::IoError(err)),
                }
            }

            Ok(total)
        })();

        // Always restore the original position.
        reader.seek_buffered(start_pos);

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};

    /// Build a frame header word from the standard MPEG audio header bit fields.
    fn header_word(
        version: MpegVersion,
        layer: MpegLayer,
        bitrate_index: u32,
        sample_rate_index: u32,
        padding: u32,
    ) -> u32 {
        let version_id = match version {
            MpegVersion::Mpeg2p5 => 0b00,
            MpegVersion::Mpeg2 => 0b10,
            MpegVersion::Mpeg1 => 0b11,
        };

        let layer_id = match layer {
            MpegLayer::Layer1 => 0b11,
            MpegLayer::Layer2 => 0b10,
            MpegLayer::Layer3 => 0b01,
        };

        0xffe0_0000
            | (version_id << 19)
            | (layer_id << 17)
            | (bitrate_index << 12)
            | (sample_rate_index << 10)
            | (padding << 9)
    }

    #[test]
    fn mpeg1_layer3_frame_geometry() {
        // 128 kbps, 44.1 kHz, no padding: the canonical 418-byte MP3 frame.
        let header =
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer3, 0b1001, 0b00, 0))
                .expect("valid header");

        assert_eq!(header.codec(), CODEC_ID_MP3);
        assert_eq!(header.sample_rate(), 44_100);
        assert_eq!(header.num_frames(), 1152);
        // 144 * 128_000 / 44_100 truncates to 417 slots on the wire, of which 4 bytes are the
        // header word. This matches real-world 128 kbps MPEG-1 layer-3 frames.
        assert_eq!(header.total_len(), 417);
        assert_eq!(header.frame_size, 413);
    }

    #[test]
    fn mpeg1_layer3_padding_adds_one_byte() {
        let unpadded =
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer3, 0b1001, 0b00, 0))
                .expect("valid header");
        let padded =
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer3, 0b1001, 0b00, 1))
                .expect("valid header");

        assert_eq!(padded.total_len(), unpadded.total_len() + 1);
        assert_eq!(padded.num_frames(), unpadded.num_frames());
    }

    #[test]
    fn mpeg2_layer3_frame_geometry() {
        // 64 kbps, 22.05 kHz: MPEG-2 layer 3 frames hold one 576-sample granule.
        let header =
            parse_frame_header(header_word(MpegVersion::Mpeg2, MpegLayer::Layer3, 0b1000, 0b00, 0))
                .expect("valid header");

        assert_eq!(header.codec(), CODEC_ID_MP3);
        assert_eq!(header.sample_rate(), 22_050);
        assert_eq!(header.num_frames(), 576);
        // 72 * 64_000 / 22_050 truncates to 208 slots, minus the 4-byte header.
        assert_eq!(header.total_len(), 208);
    }

    #[test]
    fn mpeg1_layer2_frame_geometry() {
        // 192 kbps (index 0b1010 in the layer-2 table), 48 kHz: 144 * 192000 / 48000 = 576 bytes.
        let header =
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer2, 0b1010, 0b01, 0))
                .expect("valid header");

        assert_eq!(header.codec(), CODEC_ID_MP2);
        assert_eq!(header.sample_rate(), 48_000);
        assert_eq!(header.num_frames(), 1152);
        // 144 * 192_000 / 48_000 = 576 slots.
        assert_eq!(header.total_len(), 576);
    }

    #[test]
    fn mpeg1_layer1_uses_four_byte_slots() {
        // 384 kbps, 44.1 kHz layer 1: 12 * 384000 / 44100 = 104 slots of 4 bytes, minus header.
        let header =
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer1, 0b1100, 0b00, 0))
                .expect("valid header");

        assert_eq!(header.codec(), CODEC_ID_MP1);
        assert_eq!(header.num_frames(), 384);
        assert_eq!(header.total_len(), 104 * 4);
    }

    #[test]
    fn free_bitrate_is_rejected() {
        let result =
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer3, 0b0000, 0b00, 0));

        assert!(matches!(result, Err(Error::Unsupported(_))));
    }

    #[test]
    fn reserved_bitrate_and_sample_rate_are_rejected() {
        // Bit-rate index 0xf is reserved.
        assert!(
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer3, 0b1111, 0b00, 0))
                .is_err()
        );

        // Sample-rate index 0x3 is reserved.
        assert!(
            parse_frame_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer3, 0b1001, 0b11, 0))
                .is_err()
        );
    }

    #[test]
    fn check_header_rejects_reserved_version_and_layer() {
        // Version id 0b01 is reserved, even though the sync word is present.
        assert!(!check_header(0xffe0_0000 | (0b01 << 19) | (0b01 << 17)));
        // Layer id 0b00 is reserved.
        assert!(!check_header(0xffe0_0000 | (0b11 << 19)));
        // A well-formed header passes.
        assert!(check_header(header_word(MpegVersion::Mpeg1, MpegLayer::Layer3, 0b1001, 0b00, 0)));
    }
}
