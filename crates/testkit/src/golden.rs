//! Plain-text golden transcripts for hermetic ubus wire replay.

use std::fmt;

use ubus::wire::Codec;

use crate::canon::{self, CanonEvent, CanonEventKind, CanonTranscript};
use crate::wire_tap::TapDirection;

const FORMAT_HEADER: &str = "# ubus-golden-format: 1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Golden {
    pub test: String,
    pub ubus_rev: String,
    pub libubox_rev: String,
    pub transcript: CanonTranscript,
}

impl Golden {
    pub fn from_transcript(
        test: impl Into<String>,
        ubus_rev: impl Into<String>,
        libubox_rev: impl Into<String>,
        transcript: CanonTranscript,
    ) -> Self {
        Self {
            test: test.into(),
            ubus_rev: ubus_rev.into(),
            libubox_rev: libubox_rev.into(),
            transcript,
        }
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str(FORMAT_HEADER);
        out.push('\n');
        out.push_str(&format!("# test: {}\n", self.test));
        out.push_str(&format!("# ubus-rev: {}\n", self.ubus_rev));
        out.push_str(&format!("# libubox-rev: {}\n", self.libubox_rev));
        for ev in self.transcript.events() {
            out.push_str(&format!("{} {} ", ev.conn, format_dir(ev.dir)));
            match &ev.kind {
                CanonEventKind::Frame(frame) => {
                    out.push_str("bytes ");
                    out.push_str(&hex(&Codec::write_frame(frame)));
                }
                CanonEventKind::Eof => out.push_str("eof"),
            }
            out.push('\n');
        }
        out
    }

    pub fn parse(input: &str) -> Result<Self, GoldenParseError> {
        let mut lines = input.lines();
        expect_header(lines.next(), FORMAT_HEADER, 1)?;
        let test = parse_header_value(lines.next(), "# test: ", 2)?;
        let ubus_rev = parse_header_value(lines.next(), "# ubus-rev: ", 3)?;
        let libubox_rev = parse_header_value(lines.next(), "# libubox-rev: ", 4)?;

        let mut events = Vec::new();
        for (idx, line) in lines.enumerate() {
            let line_no = idx + 5;
            if line.trim().is_empty() {
                continue;
            }
            events.push(parse_event(line, line_no)?);
        }

        Ok(Self {
            test,
            ubus_rev,
            libubox_rev,
            transcript: canon::from_canonical_events(events),
        })
    }
}

impl fmt::Display for Golden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.serialize())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoldenParseError {
    line: usize,
    detail: String,
}

impl fmt::Display for GoldenParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "golden parse error line {}: {}", self.line, self.detail)
    }
}

impl std::error::Error for GoldenParseError {}

fn expect_header(
    line: Option<&str>,
    expected: &str,
    line_no: usize,
) -> Result<(), GoldenParseError> {
    match line {
        Some(line) if line == expected => Ok(()),
        Some(line) => Err(parse_err(
            line_no,
            format!("expected {expected:?}, got {line:?}"),
        )),
        None => Err(parse_err(line_no, "missing header")),
    }
}

fn parse_header_value(
    line: Option<&str>,
    prefix: &'static str,
    line_no: usize,
) -> Result<String, GoldenParseError> {
    let line = line.ok_or_else(|| parse_err(line_no, format!("missing {prefix:?} header")))?;
    line.strip_prefix(prefix)
        .map(ToOwned::to_owned)
        .ok_or_else(|| parse_err(line_no, format!("expected {prefix:?} header")))
}

fn parse_event(line: &str, line_no: usize) -> Result<CanonEvent, GoldenParseError> {
    let mut parts = line.split_whitespace();
    let conn = parts
        .next()
        .ok_or_else(|| parse_err(line_no, "missing conn"))?
        .parse::<usize>()
        .map_err(|e| parse_err(line_no, format!("bad conn: {e}")))?;
    let dir = parse_dir(
        parts
            .next()
            .ok_or_else(|| parse_err(line_no, "missing direction"))?,
        line_no,
    )?;
    let kind = match parts
        .next()
        .ok_or_else(|| parse_err(line_no, "missing event kind"))?
    {
        "bytes" => {
            let hex = parts
                .next()
                .ok_or_else(|| parse_err(line_no, "missing bytes hex"))?;
            if parts.next().is_some() {
                return Err(parse_err(line_no, "trailing tokens after bytes event"));
            }
            let bytes = parse_hex(hex, line_no)?;
            let (frame, consumed) =
                Codec::read_frame(&bytes).map_err(|e| parse_err(line_no, format!("{e:?}")))?;
            if consumed != bytes.len() {
                return Err(parse_err(
                    line_no,
                    format!(
                        "frame consumed {consumed} byte(s), line has {} byte(s)",
                        bytes.len()
                    ),
                ));
            }
            CanonEventKind::Frame(frame)
        }
        "eof" => {
            if parts.next().is_some() {
                return Err(parse_err(line_no, "trailing tokens after eof event"));
            }
            CanonEventKind::Eof
        }
        other => return Err(parse_err(line_no, format!("unknown event kind {other:?}"))),
    };

    Ok(CanonEvent { conn, dir, kind })
}

fn parse_dir(s: &str, line_no: usize) -> Result<TapDirection, GoldenParseError> {
    match s {
        "c2b" => Ok(TapDirection::ClientToBroker),
        "b2c" => Ok(TapDirection::BrokerToClient),
        other => Err(parse_err(line_no, format!("bad direction {other:?}"))),
    }
}

fn format_dir(dir: TapDirection) -> &'static str {
    match dir {
        TapDirection::ClientToBroker => "c2b",
        TapDirection::BrokerToClient => "b2c",
    }
}

fn parse_hex(s: &str, line_no: usize) -> Result<Vec<u8>, GoldenParseError> {
    if !s.len().is_multiple_of(2) {
        return Err(parse_err(line_no, "hex string has odd length"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_value(bytes[i]).ok_or_else(|| parse_err(line_no, "bad hex digit"))?;
        let lo = hex_value(bytes[i + 1]).ok_or_else(|| parse_err(line_no, "bad hex digit"))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn parse_err(line: usize, detail: impl Into<String>) -> GoldenParseError {
    GoldenParseError {
        line,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use ubus::envelope::{EnvBuilder, UbusAttr};
    use ubus::wire::{Frame, Header, MsgType, PROTO_VERSION};

    use super::*;

    fn frame(seq: u16, peer: u32) -> Frame {
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, peer);
        Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Data,
                seq,
                peer,
            },
            payload: env.finish(),
        }
    }

    #[test]
    fn serialize_parse_round_trip() {
        let transcript = canon::from_canonical_events(vec![
            CanonEvent {
                conn: 0,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Frame(frame(7, 1)),
            },
            CanonEvent {
                conn: 0,
                dir: TapDirection::ClientToBroker,
                kind: CanonEventKind::Eof,
            },
        ]);
        let golden =
            Golden::from_transcript("ubusd_handshake sample", "24864e7", "17f527f", transcript);

        let parsed = Golden::parse(&golden.serialize()).unwrap();
        assert_eq!(parsed, golden);
    }

    #[test]
    fn golden_diff_ignores_cross_connection_global_interleave() {
        let conn0 = frame(7, 1);
        let conn1 = frame(8, 2);
        let left = canon::from_canonical_events(vec![
            CanonEvent {
                conn: 0,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Frame(conn0.clone()),
            },
            CanonEvent {
                conn: 1,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Frame(conn1.clone()),
            },
            CanonEvent {
                conn: 0,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Eof,
            },
            CanonEvent {
                conn: 1,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Eof,
            },
        ]);
        let right = canon::from_canonical_events(vec![
            CanonEvent {
                conn: 1,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Frame(conn1),
            },
            CanonEvent {
                conn: 0,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Frame(conn0),
            },
            CanonEvent {
                conn: 1,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Eof,
            },
            CanonEvent {
                conn: 0,
                dir: TapDirection::BrokerToClient,
                kind: CanonEventKind::Eof,
            },
        ]);

        assert_eq!(canon::diff_events(&left, &right), None);
    }
}
