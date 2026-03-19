//! Wire protocol: length-prefixed bincode frames over a byte stream.
//!
//! Format: `[4 bytes: u32 LE length] [N bytes: bincode payload]`
//!
//! Works over any `AsyncRead + AsyncWrite` transport (Unix socket, TCP, named pipe).

use serde::{de::DeserializeOwned, Serialize};
use std::io;

/// Maximum frame size (1 MB). Prevents OOM from malformed length prefixes.
const MAX_FRAME_SIZE: u32 = 1_048_576;

/// Write a serializable value as a length-prefixed bincode frame.
///
/// Uses synchronous I/O (suitable for both sync and tokio `spawn_blocking`).
pub fn write_frame<W: io::Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let payload =
        bincode::serialize(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

/// Read a length-prefixed bincode frame and deserialize.
///
/// Returns `None` on clean EOF (peer closed connection).
pub fn read_frame<R: io::Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {} bytes (max {})", len, MAX_FRAME_SIZE),
        ));
    }

    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;

    let value = bincode::deserialize(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EngineCommand;
    use std::io::Cursor;

    #[test]
    fn round_trip_command() {
        let cmd = EngineCommand::ReadChannel { channel: 1113 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &cmd).unwrap();

        let mut cursor = Cursor::new(&buf);
        let decoded: Option<EngineCommand> = read_frame(&mut cursor).unwrap();
        match decoded.unwrap() {
            EngineCommand::ReadChannel { channel } => assert_eq!(channel, 1113),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn round_trip_all_commands() {
        let commands = vec![
            EngineCommand::Shutdown,
            EngineCommand::ReadChannel { channel: 612 },
            EngineCommand::WriteChannel {
                channel: 2001,
                value: 3.14,
                level: 17,
            },
            EngineCommand::RelinquishLevel {
                channel: 2001,
                level: 8,
            },
            EngineCommand::GetWriteLevels { channel: 2001 },
            EngineCommand::ConvertValue {
                channel: 1100,
                raw: 2048.0,
            },
            EngineCommand::ListChannels,
            EngineCommand::ListTables,
            EngineCommand::ListPolls,
            EngineCommand::Status,
            EngineCommand::PollNow,
            EngineCommand::ReloadConfig,
        ];

        for cmd in commands {
            let mut buf = Vec::new();
            write_frame(&mut buf, &cmd).unwrap();

            let mut cursor = Cursor::new(&buf);
            let decoded: Option<EngineCommand> = read_frame(&mut cursor).unwrap();
            assert!(decoded.is_some(), "failed to decode: {:?}", cmd);
        }
    }

    #[test]
    fn eof_returns_none() {
        let buf: Vec<u8> = vec![];
        let mut cursor = Cursor::new(&buf);
        let result: Option<EngineCommand> = read_frame(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn oversized_frame_rejected() {
        let len = (MAX_FRAME_SIZE + 1).to_le_bytes();
        let mut cursor = Cursor::new(len.to_vec());
        let result: io::Result<Option<EngineCommand>> = read_frame(&mut cursor);
        assert!(result.is_err());
    }

    // ── Edge-case IPC protocol tests ─────────────────────────

    #[test]
    fn test_frame_partial_length_prefix() {
        // Only 2 of 4 length bytes, then EOF
        let buf = vec![0x10, 0x00];
        let mut cursor = Cursor::new(buf);
        let result: io::Result<Option<EngineCommand>> = read_frame(&mut cursor);
        // Should return None (EOF during length read) or UnexpectedEof
        match result {
            Ok(None) => {} // clean EOF
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {} // also acceptable
            other => panic!("expected EOF/None, got: {:?}", other),
        }
    }

    #[test]
    fn test_frame_length_mismatch() {
        // Write length=100 but only 50 bytes of payload
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&[0xAB; 50]); // only 50 of promised 100 bytes

        let mut cursor = Cursor::new(buf);
        let result: io::Result<Option<EngineCommand>> = read_frame(&mut cursor);
        // Should fail with UnexpectedEof, not hang
        assert!(result.is_err(), "should error on truncated payload");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn test_frame_corrupted_bincode() {
        // Valid length prefix but garbage payload that won't deserialize
        let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0x01, 0x02];
        let mut buf = Vec::new();
        buf.extend_from_slice(&(garbage.len() as u32).to_le_bytes());
        buf.extend_from_slice(&garbage);

        let mut cursor = Cursor::new(buf);
        let result: io::Result<Option<EngineCommand>> = read_frame(&mut cursor);
        assert!(result.is_err(), "corrupted bincode should error");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_frame_zero_length() {
        // Length = 0 means zero payload bytes
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes());

        let mut cursor = Cursor::new(buf);
        let result: io::Result<Option<EngineCommand>> = read_frame(&mut cursor);
        // Zero bytes can't deserialize to an EngineCommand — should be InvalidData
        assert!(result.is_err(), "zero-length frame should error on deserialize");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_frame_max_size_boundary() {
        // Exactly at MAX_FRAME_SIZE — length is accepted (even though payload will be garbage)
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAX_FRAME_SIZE.to_le_bytes());
        // Don't actually allocate 1MB of payload — just verify the length check passes
        // by checking that we get UnexpectedEof (payload too short) not InvalidData (frame too large)
        let mut cursor = Cursor::new(buf);
        let result: io::Result<Option<EngineCommand>> = read_frame(&mut cursor);
        assert!(result.is_err());
        // The error should be UnexpectedEof (payload missing), NOT "frame too large"
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
            "max size should be accepted; error should be missing payload"
        );

        // MAX + 1 should be rejected with InvalidData ("frame too large")
        let mut buf2 = Vec::new();
        buf2.extend_from_slice(&(MAX_FRAME_SIZE + 1).to_le_bytes());
        let mut cursor2 = Cursor::new(buf2);
        let result2: io::Result<Option<EngineCommand>> = read_frame(&mut cursor2);
        assert!(result2.is_err());
        assert_eq!(
            result2.unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "max+1 should be rejected as too large"
        );
    }

    #[test]
    fn test_frame_rapid_roundtrip() {
        let mut buf = Vec::new();
        let count = 1000;

        // Write 1000 frames
        for i in 0..count {
            let cmd = EngineCommand::ReadChannel { channel: i };
            write_frame(&mut buf, &cmd).unwrap();
        }

        // Read all 1000 back
        let mut cursor = Cursor::new(&buf);
        for i in 0..count {
            let decoded: Option<EngineCommand> = read_frame(&mut cursor).unwrap();
            match decoded.unwrap() {
                EngineCommand::ReadChannel { channel } => assert_eq!(channel, i),
                other => panic!("unexpected at {}: {:?}", i, other),
            }
        }

        // Next read should return None (EOF)
        let tail: Option<EngineCommand> = read_frame(&mut cursor).unwrap();
        assert!(tail.is_none());
    }

    #[test]
    fn test_frame_all_command_variants() {
        let commands = vec![
            EngineCommand::Shutdown,
            EngineCommand::ReadChannel { channel: 0 },
            EngineCommand::ReadChannel { channel: u32::MAX },
            EngineCommand::WriteChannel {
                channel: 2001,
                value: f64::NAN, // edge case: NaN
                level: 0,
            },
            EngineCommand::WriteChannel {
                channel: 9999,
                value: f64::INFINITY,
                level: 255,
            },
            EngineCommand::RelinquishLevel {
                channel: 1,
                level: 17,
            },
            EngineCommand::GetWriteLevels { channel: 42 },
            EngineCommand::ConvertValue {
                channel: 1100,
                raw: -999.999,
            },
            EngineCommand::ListChannels,
            EngineCommand::ListTables,
            EngineCommand::ListPolls,
            EngineCommand::Status,
            EngineCommand::PollNow,
            EngineCommand::ReloadConfig,
            EngineCommand::GetHistory {
                channel: 1113,
                since_secs: 0,
                limit: usize::MAX,
            },
        ];

        for cmd in &commands {
            let mut buf = Vec::new();
            write_frame(&mut buf, cmd).unwrap();
            let mut cursor = Cursor::new(&buf);
            let decoded: Option<EngineCommand> = read_frame(&mut cursor).unwrap();
            assert!(decoded.is_some(), "failed to round-trip: {:?}", cmd);
        }
    }

    #[test]
    fn test_frame_all_response_variants() {
        use crate::types::{
            ChannelInfo, EngineResponse, HistoryEntry, PollInfo, StatusInfo, WriteLevelInfo,
        };

        let responses = vec![
            EngineResponse::Ok,
            EngineResponse::Value {
                channel: 1113,
                status: "ok".into(),
                raw: 2048.0,
                cur: 72.5,
            },
            EngineResponse::Channels(vec![ChannelInfo {
                id: 1,
                label: "test".into(),
                channel_type: "Analog".into(),
                direction: "Input".into(),
                enabled: true,
                status: "ok".into(),
                cur: 0.0,
                raw: 0.0,
            }]),
            EngineResponse::Channels(vec![]), // empty list
            EngineResponse::Tables(vec!["table1".into(), "table2".into()]),
            EngineResponse::Tables(vec![]),
            EngineResponse::Polls(vec![PollInfo {
                channel: 1100,
                last_cur: 68.2,
                last_status: "ok".into(),
            }]),
            EngineResponse::Status(StatusInfo {
                uptime_secs: 86400,
                channel_count: 140,
                poll_count: 12345,
                table_count: 16,
                poll_interval_ms: 1000,
            }),
            EngineResponse::WriteLevels(vec![WriteLevelInfo {
                level: 8,
                level_dis: "Manual Override".into(),
                val: Some(72.0),
                who: "cli".into(),
            }]),
            EngineResponse::History(vec![HistoryEntry {
                ts: 1709500000,
                cur: 72.5,
                raw: 2048.0,
                status: "ok".into(),
            }]),
            EngineResponse::History(vec![]),
            EngineResponse::Error("something went wrong".into()),
            EngineResponse::Error(String::new()), // empty error
        ];

        for resp in &responses {
            let mut buf = Vec::new();
            write_frame(&mut buf, resp).unwrap();
            let mut cursor = Cursor::new(&buf);
            let decoded: Option<EngineResponse> = read_frame(&mut cursor).unwrap();
            assert!(decoded.is_some(), "failed to round-trip: {:?}", resp);
        }
    }

    #[test]
    fn test_frame_back_to_back() {
        let cmd1 = EngineCommand::Status;
        let cmd2 = EngineCommand::ReadChannel { channel: 42 };
        let cmd3 = EngineCommand::WriteChannel {
            channel: 2001,
            value: 3.14,
            level: 8,
        };

        let mut buf = Vec::new();
        write_frame(&mut buf, &cmd1).unwrap();
        write_frame(&mut buf, &cmd2).unwrap();
        write_frame(&mut buf, &cmd3).unwrap();

        let mut cursor = Cursor::new(&buf);

        // Read all 3 in order
        let d1: EngineCommand = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(d1, EngineCommand::Status));

        let d2: EngineCommand = read_frame(&mut cursor).unwrap().unwrap();
        match d2 {
            EngineCommand::ReadChannel { channel } => assert_eq!(channel, 42),
            other => panic!("expected ReadChannel, got {:?}", other),
        }

        let d3: EngineCommand = read_frame(&mut cursor).unwrap().unwrap();
        match d3 {
            EngineCommand::WriteChannel {
                channel,
                value,
                level,
            } => {
                assert_eq!(channel, 2001);
                assert!((value - 3.14).abs() < f64::EPSILON);
                assert_eq!(level, 8);
            }
            other => panic!("expected WriteChannel, got {:?}", other),
        }

        // EOF after 3 frames
        let d4: Option<EngineCommand> = read_frame(&mut cursor).unwrap();
        assert!(d4.is_none());
    }

    #[test]
    fn test_frame_interleaved_command_response() {
        use crate::types::EngineResponse;

        // Write command, then response, then command — verify mixed types work
        let mut buf = Vec::new();
        let cmd = EngineCommand::Status;
        let resp = EngineResponse::Ok;
        let cmd2 = EngineCommand::Shutdown;

        write_frame(&mut buf, &cmd).unwrap();
        write_frame(&mut buf, &resp).unwrap();
        write_frame(&mut buf, &cmd2).unwrap();

        let mut cursor = Cursor::new(&buf);

        let d1: EngineCommand = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(d1, EngineCommand::Status));

        let d2: EngineResponse = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(d2, EngineResponse::Ok));

        let d3: EngineCommand = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(d3, EngineCommand::Shutdown));
    }

    #[test]
    fn test_frame_single_byte_at_a_time() {
        // Write a frame, then read it from a cursor that has all data
        // This verifies read_exact works correctly on small payloads
        let cmd = EngineCommand::Shutdown;
        let mut buf = Vec::new();
        write_frame(&mut buf, &cmd).unwrap();

        // Verify the frame is quite small (Shutdown has no fields)
        assert!(buf.len() < 20, "Shutdown frame should be small, got {} bytes", buf.len());

        let mut cursor = Cursor::new(&buf);
        let decoded: EngineCommand = read_frame(&mut cursor).unwrap().unwrap();
        assert!(matches!(decoded, EngineCommand::Shutdown));
    }

    #[test]
    fn test_frame_large_string_payload() {
        use crate::types::EngineResponse;

        // Response with a very large error string (100KB)
        let big_msg = "X".repeat(100_000);
        let resp = EngineResponse::Error(big_msg.clone());

        let mut buf = Vec::new();
        write_frame(&mut buf, &resp).unwrap();

        let mut cursor = Cursor::new(&buf);
        let decoded: EngineResponse = read_frame(&mut cursor).unwrap().unwrap();
        match decoded {
            EngineResponse::Error(msg) => assert_eq!(msg.len(), 100_000),
            other => panic!("expected Error, got {:?}", other),
        }
    }
}
