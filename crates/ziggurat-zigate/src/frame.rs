//! ZiGate v1 serial framing, as implemented by the Abeille plugin.
//!
//! Host → ZiGate (`AbeilleCmdQueue.class.php`):
//! `01 | esc(type:2) esc(len:2) esc(crc:1) esc(payload) | 03`, where `len` counts the
//! payload only and `crc` is the XOR of the type, length and payload bytes.
//!
//! ZiGate → host (`AbeilleSerialRead.php`, `AbeilleParser::protocolDatas`):
//! `01 | esc(type:2) esc(len:2) esc(crc:1) esc(payload) esc(lqi:1) | 03`, where `len`
//! counts the payload plus the trailing LQI byte, and the LQI is part of the CRC.
//!
//! Escaping: every byte below 0x10 is sent as `02, byte ^ 0x10`. Multi-byte fields are
//! big-endian.

pub const START: u8 = 0x01;
pub const ESC: u8 = 0x02;
pub const END: u8 = 0x03;

/// Largest payload accepted from the host. Abeille's biggest command (`0530` with a
/// full ZCL frame) stays well under this.
pub const MAX_PAYLOAD: usize = 512;

/// A decoded ZiGate message, without framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_type: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Fewer than the 5 header bytes between START and END
    TooShort,
    /// The length field does not match the received payload length
    BadLength {
        declared: u16,
        actual: usize,
    },
    BadCrc {
        expected: u8,
        actual: u8,
    },
    /// ESC immediately followed by END
    DanglingEscape,
    TooLong,
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort => write!(f, "frame shorter than its header"),
            Self::BadLength { declared, actual } => {
                write!(f, "length field {declared} but {actual} payload bytes")
            }
            Self::BadCrc { expected, actual } => {
                write!(f, "CRC {actual:02X}, expected {expected:02X}")
            }
            Self::DanglingEscape => write!(f, "escape byte at end of frame"),
            Self::TooLong => write!(f, "frame longer than {MAX_PAYLOAD} bytes"),
        }
    }
}

impl std::error::Error for FrameError {}

fn crc(msg_type: u16, len: u16, data: &[u8]) -> u8 {
    let [t0, t1] = msg_type.to_be_bytes();
    let [l0, l1] = len.to_be_bytes();
    data.iter().fold(t0 ^ t1 ^ l0 ^ l1, |acc, b| acc ^ b)
}

fn push_escaped(out: &mut Vec<u8>, byte: u8) {
    if byte < 0x10 {
        out.push(ESC);
        out.push(byte ^ 0x10);
    } else {
        out.push(byte);
    }
}

/// Frame a message sent to Abeille, with the trailing LQI byte it expects.
#[must_use]
pub fn encode_to_host(msg: &Message, lqi: u8) -> Vec<u8> {
    let len = u16::try_from(msg.payload.len() + 1).expect("payload fits a u16");
    let mut body = msg.payload.clone();
    body.push(lqi);
    let crc = crc(msg.msg_type, len, &body);

    let mut out = Vec::with_capacity(2 * (body.len() + 5) + 2);
    out.push(START);
    for byte in msg
        .msg_type
        .to_be_bytes()
        .into_iter()
        .chain(len.to_be_bytes())
        .chain([crc])
        .chain(body)
    {
        push_escaped(&mut out, byte);
    }
    out.push(END);
    out
}

/// Frame a message the way Abeille sends it to a ZiGate (no LQI). Used by tests and
/// by the debug client.
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub fn encode_from_host(msg: &Message) -> Vec<u8> {
    let len = u16::try_from(msg.payload.len()).expect("payload fits a u16");
    let crc = crc(msg.msg_type, len, &msg.payload);

    let mut out = Vec::with_capacity(2 * (msg.payload.len() + 5) + 2);
    out.push(START);
    for byte in msg
        .msg_type
        .to_be_bytes()
        .into_iter()
        .chain(len.to_be_bytes())
        .chain([crc])
        .chain(msg.payload.iter().copied())
    {
        push_escaped(&mut out, byte);
    }
    out.push(END);
    out
}

/// Parse the unescaped bytes between START and END of a host → ZiGate frame.
fn parse_from_host(raw: &[u8]) -> Result<Message, FrameError> {
    let [t0, t1, l0, l1, crc_byte, payload @ ..] = raw else {
        return Err(FrameError::TooShort);
    };
    let msg_type = u16::from_be_bytes([*t0, *t1]);
    let declared = u16::from_be_bytes([*l0, *l1]);
    if usize::from(declared) != payload.len() {
        return Err(FrameError::BadLength {
            declared,
            actual: payload.len(),
        });
    }
    let expected = crc(msg_type, declared, payload);
    if expected != *crc_byte {
        return Err(FrameError::BadCrc {
            expected,
            actual: *crc_byte,
        });
    }
    Ok(Message {
        msg_type,
        payload: payload.to_vec(),
    })
}

/// Incremental decoder for the byte stream Abeille writes to the gateway.
///
/// Bytes outside a START..END pair are ignored, like the real firmware does.
#[derive(Debug, Default)]
pub struct HostDecoder {
    buf: Vec<u8>,
    in_frame: bool,
    escaped: bool,
}

impl HostDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte; returns a result once a complete frame has been received.
    pub fn push(&mut self, byte: u8) -> Option<Result<Message, FrameError>> {
        match byte {
            START => {
                // A new START always resynchronises, dropping any partial frame
                self.buf.clear();
                self.in_frame = true;
                self.escaped = false;
                None
            }
            _ if !self.in_frame => None,
            END => {
                self.in_frame = false;
                let result = if self.escaped {
                    Err(FrameError::DanglingEscape)
                } else {
                    parse_from_host(&self.buf)
                };
                self.buf.clear();
                self.escaped = false;
                Some(result)
            }
            ESC => {
                self.escaped = true;
                None
            }
            _ => {
                let byte = if self.escaped { byte ^ 0x10 } else { byte };
                self.escaped = false;
                if self.buf.len() >= MAX_PAYLOAD + 5 {
                    self.in_frame = false;
                    self.buf.clear();
                    return Some(Err(FrameError::TooLong));
                }
                self.buf.push(byte);
                None
            }
        }
    }

    /// Feed a chunk of bytes, collecting every completed frame.
    pub fn extend(&mut self, bytes: &[u8]) -> Vec<Result<Message, FrameError>> {
        bytes.iter().filter_map(|&b| self.push(b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(msg_type: u16, payload: &[u8]) -> Message {
        Message {
            msg_type,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn get_version_matches_abeille() {
        // Abeille's zgGetVersion: type 0010, empty payload, CRC = 00^10^00^00 = 10
        assert_eq!(
            encode_from_host(&msg(0x0010, &[])),
            [0x01, 0x02, 0x10, 0x10, 0x02, 0x10, 0x02, 0x10, 0x10, 0x03]
        );
    }

    #[test]
    fn host_frame_round_trip() {
        // 0530 raw APS request with bytes below 0x10 that must be escaped
        let original = msg(
            0x0530,
            &[
                0x02, 0x2A, 0xAE, 0x01, 0x01, 0x07, 0x02, 0x01, 0x04, 0x02, 0x30, 0x03, 0x10, 0x05,
                0x00,
            ],
        );
        let mut decoder = HostDecoder::new();
        let frames = decoder.extend(&encode_from_host(&original));
        assert_eq!(frames, vec![Ok(original)]);
    }

    #[test]
    fn decoder_ignores_noise_and_resyncs() {
        let first = msg(0x0049, &[0xFF, 0xFC, 0xFE, 0x00]);
        let second = msg(0x0014, &[]);
        let mut stream = vec![0xAA, 0x55];
        // A truncated frame interrupted by a new START must be dropped silently
        stream.extend_from_slice(&encode_from_host(&first)[..4]);
        stream.extend(encode_from_host(&first));
        stream.extend(encode_from_host(&second));

        let mut decoder = HostDecoder::new();
        assert_eq!(decoder.extend(&stream), vec![Ok(first), Ok(second)]);
    }

    #[test]
    fn decoder_rejects_bad_crc() {
        let mut frame = encode_from_host(&msg(0x0024, &[]));
        // With no payload the CRC (0x24, sent unescaped) is the last byte before END
        let crc_index = frame.len() - 2;
        assert_eq!(frame[crc_index], 0x24);
        frame[crc_index] = 0x25;
        let mut decoder = HostDecoder::new();
        assert_eq!(
            decoder.extend(&frame),
            vec![Err(FrameError::BadCrc {
                expected: 0x24,
                actual: 0x25
            })]
        );
    }

    #[test]
    fn host_bound_frame_counts_and_checksums_lqi() {
        // 8010 version 0004-0323 with LQI 0xFF
        let frame = encode_to_host(&msg(0x8010, &[0x00, 0x04, 0x03, 0x23]), 0xFF);
        let unescaped: Vec<u8> = {
            let mut out = Vec::new();
            let mut esc = false;
            for &b in &frame[1..frame.len() - 1] {
                if b == ESC {
                    esc = true;
                } else {
                    out.push(if esc { b ^ 0x10 } else { b });
                    esc = false;
                }
            }
            out
        };
        // type, len = 4 payload + 1 LQI, crc, payload, lqi
        assert_eq!(&unescaped[..4], &[0x80, 0x10, 0x00, 0x05]);
        let crc = unescaped[2..]
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2)
            .fold(0x80 ^ 0x10, |acc, (_, b)| acc ^ b);
        assert_eq!(unescaped[4], crc);
        assert_eq!(&unescaped[5..], &[0x00, 0x04, 0x03, 0x23, 0xFF]);
        assert!(
            frame
                .iter()
                .skip(1)
                .take(frame.len() - 2)
                .all(|&b| b >= 0x10 || b == ESC)
        );
    }
}
