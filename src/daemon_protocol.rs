use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::model::{DirectoryListing, FileListing, FilePreview};

pub const PROTOCOL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 28;
pub const MAX_FRAME_PAYLOAD: usize = 8 * 1024 * 1024;
pub const DATA_CHUNK_SIZE: usize = 64 * 1024;
pub const INITIAL_STREAM_WINDOW: u32 = 1024 * 1024;
pub const COMPRESSION_THRESHOLD: usize = 4 * 1024;

const MAGIC: [u8; 4] = *b"MXL1";
const FLAG_COMPRESSED_LZ4: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Request = 1,
    Response = 2,
    OpenStream = 3,
    Data = 4,
    WindowUpdate = 5,
    CloseStream = 6,
    Heartbeat = 7,
    Error = 8,
}

impl TryFrom<u8> for FrameKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            1 => Self::Request,
            2 => Self::Response,
            3 => Self::OpenStream,
            4 => Self::Data,
            5 => Self::WindowUpdate,
            6 => Self::CloseStream,
            7 => Self::Heartbeat,
            8 => Self::Error,
            other => bail!("unsupported muxloom frame kind {other}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub flags: u8,
    pub stream_id: u32,
    pub request_id: u64,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: FrameKind, stream_id: u32, request_id: u64, payload: Vec<u8>) -> Self {
        Self {
            kind,
            flags: 0,
            stream_id,
            request_id,
            payload,
        }
    }

    pub fn data(stream_id: u32, request_id: u64, payload: &[u8], compress: bool) -> Self {
        if compress && payload.len() >= COMPRESSION_THRESHOLD {
            let compressed = lz4_flex::compress_prepend_size(payload);
            if compressed.len() < payload.len() {
                return Self {
                    kind: FrameKind::Data,
                    flags: FLAG_COMPRESSED_LZ4,
                    stream_id,
                    request_id,
                    payload: compressed,
                };
            }
        }
        Self::new(FrameKind::Data, stream_id, request_id, payload.to_vec())
    }

    pub fn decoded_payload(&self) -> Result<Vec<u8>> {
        if self.flags & !FLAG_COMPRESSED_LZ4 != 0 {
            bail!("muxloom frame has unsupported flags {:#x}", self.flags);
        }
        if self.flags & FLAG_COMPRESSED_LZ4 == 0 {
            return Ok(self.payload.clone());
        }
        lz4_flex::decompress_size_prepended(&self.payload)
            .context("failed to decompress muxloom LZ4 frame")
    }

    pub fn window_update(stream_id: u32, credit: u32) -> Self {
        Self::new(
            FrameKind::WindowUpdate,
            stream_id,
            0,
            credit.to_be_bytes().to_vec(),
        )
    }

    pub fn window_credit(&self) -> Result<u32> {
        if self.kind != FrameKind::WindowUpdate || self.payload.len() != 4 {
            bail!("invalid stream window update");
        }
        Ok(u32::from_be_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ]))
    }

    pub fn json<T: Serialize>(
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
        value: &T,
    ) -> Result<Self> {
        let payload = serde_json::to_vec(value).context("failed to encode daemon message")?;
        if payload.len() > MAX_FRAME_PAYLOAD {
            bail!("daemon message exceeds maximum frame size");
        }
        Ok(Self::new(kind, stream_id, request_id, payload))
    }

    pub fn decode_json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.payload).context("failed to decode daemon message")
    }

    pub fn read_from(reader: &mut impl Read) -> Result<Option<Self>> {
        let mut header = [0_u8; HEADER_LEN];
        if !read_exact_or_eof(reader, &mut header)? {
            return Ok(None);
        }
        if header[..4] != MAGIC {
            bail!("invalid muxloom frame magic");
        }
        let version = u16::from_be_bytes([header[4], header[5]]);
        if version != PROTOCOL_VERSION {
            bail!("unsupported muxloom protocol version {version}");
        }
        let kind = FrameKind::try_from(header[6])?;
        let flags = header[7];
        let stream_id = u32::from_be_bytes(header[8..12].try_into().unwrap());
        let request_id = u64::from_be_bytes(header[12..20].try_into().unwrap());
        let payload_len = u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize;
        if header[24..28] != [0, 0, 0, 0] {
            bail!("muxloom frame has non-zero reserved bytes");
        }
        if payload_len > MAX_FRAME_PAYLOAD {
            bail!("muxloom frame payload is too large: {payload_len}");
        }
        let mut payload = vec![0; payload_len];
        reader
            .read_exact(&mut payload)
            .context("truncated muxloom frame payload")?;
        Ok(Some(Self {
            kind,
            flags,
            stream_id,
            request_id,
            payload,
        }))
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<()> {
        if self.payload.len() > MAX_FRAME_PAYLOAD {
            bail!("muxloom frame payload is too large: {}", self.payload.len());
        }
        let mut header = [0_u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        header[6] = self.kind as u8;
        header[7] = self.flags;
        header[8..12].copy_from_slice(&self.stream_id.to_be_bytes());
        header[12..20].copy_from_slice(&self.request_id.to_be_bytes());
        header[20..24].copy_from_slice(&(self.payload.len() as u32).to_be_bytes());
        writer
            .write_all(&header)
            .context("failed to write muxloom frame header")?;
        writer
            .write_all(&self.payload)
            .context("failed to write muxloom frame payload")?;
        writer.flush().context("failed to flush muxloom frame")
    }
}

fn read_exact_or_eof(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<bool> {
    let mut offset = 0;
    while offset < buffer.len() {
        match reader.read(&mut buffer[offset..])? {
            0 if offset == 0 => return Ok(false),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated muxloom frame header",
                ));
            }
            read => offset += read,
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum DaemonRequest {
    Hello {
        client_version: String,
        protocol_version: u16,
    },
    Ping,
    Status,
    ProbeExecutables {
        executables: Vec<String>,
    },
    ListSessions,
    Launch {
        session_id: String,
        kind: String,
        path: String,
        label: String,
        executable: String,
        args: Vec<String>,
        environment: Vec<(String, String)>,
        created_at: u64,
        columns: u16,
        rows: u16,
    },
    Resize {
        session_id: String,
        columns: u16,
        rows: u16,
    },
    ReadHistory {
        session_id: String,
        offset_from_bottom: usize,
        lines: usize,
    },
    SearchHistory {
        session_id: String,
        query: String,
        max_matches: usize,
    },
    ListDirectory {
        path: String,
    },
    ListFiles {
        path: String,
    },
    PreviewFile {
        path: String,
        limit: usize,
    },
    Archive {
        session_id: String,
    },
    Delete {
        session_id: String,
    },
    RunShell {
        script: String,
        environment: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum DaemonResponse {
    Hello {
        daemon_version: String,
        protocol_version: u16,
        pid: u32,
        capabilities: Vec<String>,
    },
    Pong {
        unix_time_ms: u64,
    },
    Status {
        pid: u32,
        uptime_ms: u64,
        clients: usize,
    },
    Executables {
        available: Vec<String>,
    },
    Sessions {
        sessions: Vec<DaemonSession>,
    },
    Launched {
        session: DaemonSession,
    },
    Ack,
    HistoryComplete {
        total_lines: usize,
        columns: u16,
        rows: u16,
        offset_from_bottom: usize,
    },
    HistoryMatches {
        matches: Vec<DaemonHistoryMatch>,
    },
    Directory {
        listing: DirectoryListing,
    },
    Files {
        listing: FileListing,
    },
    Preview {
        preview: FilePreview,
    },
    ShellComplete {
        exit_code: i32,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonSession {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub label: String,
    pub created_at: u64,
    pub pid: Option<u32>,
    pub dead: bool,
    pub archived: bool,
    pub recap: Option<String>,
    pub working: bool,
    pub needs_attention: bool,
    pub attention_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum OpenStream {
    Pty {
        session_id: String,
        columns: u16,
        rows: u16,
    },
    File {
        path: String,
        offset: u64,
        length: Option<u64>,
    },
    Media {
        path: String,
        offset: u64,
        length: Option<u64>,
    },
    Upload {
        path: String,
        size: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamOpened {
    pub initial_window: u32,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonHistoryMatch {
    pub recap: bool,
    pub line_number: usize,
    pub text: String,
}

pub mod stream {
    pub const STDOUT: u32 = 1;
    pub const STDERR: u32 = 2;
    pub const HISTORY: u32 = 3;
    pub const PTY_BASE: u32 = 1024;
    pub const FILE_BASE: u32 = 1 << 20;
    pub const MEDIA_BASE: u32 = 1 << 24;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_preserves_routing_fields_and_binary_payload() {
        let frame = Frame {
            kind: FrameKind::Data,
            flags: 3,
            stream_id: 42,
            request_id: 9001,
            payload: vec![0, 1, 2, 255],
        };
        let mut bytes = Vec::new();
        frame.write_to(&mut bytes).unwrap();
        let decoded = Frame::read_from(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn rejects_oversized_frames_before_allocating_payload() {
        let mut bytes = vec![0; HEADER_LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes[6] = FrameKind::Data as u8;
        bytes[20..24].copy_from_slice(&((MAX_FRAME_PAYLOAD + 1) as u32).to_be_bytes());
        assert!(Frame::read_from(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn json_messages_are_versioned_independently_from_frames() {
        let request = DaemonRequest::Hello {
            client_version: "0.3.0".into(),
            protocol_version: PROTOCOL_VERSION,
        };
        let frame = Frame::json(FrameKind::Request, 0, 7, &request).unwrap();
        assert_eq!(frame.decode_json::<DaemonRequest>().unwrap(), request);
    }

    #[test]
    fn repetitive_data_is_compressed_but_small_interactive_data_is_not() {
        let large = vec![b'x'; COMPRESSION_THRESHOLD * 4];
        let frame = Frame::data(stream::FILE_BASE, 4, &large, true);
        assert_ne!(frame.flags & FLAG_COMPRESSED_LZ4, 0);
        assert!(frame.payload.len() < large.len());
        assert_eq!(frame.decoded_payload().unwrap(), large);

        let input = Frame::data(stream::PTY_BASE, 5, b"ls\r", true);
        assert_eq!(input.flags, 0);
        assert_eq!(input.decoded_payload().unwrap(), b"ls\r");
    }
}
