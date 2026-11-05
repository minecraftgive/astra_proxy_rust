//! XTLS-Vision padding wrapper for VLESS.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use meow_common::ProxyConn;
use rand::RngCore;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::conn::VlessConn;

const UUID_LEN: usize = 16;
const PADDING_HEADER_LEN: usize = UUID_LEN + 1 + 2 + 2;
const COMMAND_PADDING_CONTINUE: u8 = 0x00;
const COMMAND_PADDING_END: u8 = 0x01;
const COMMAND_PADDING_DIRECT: u8 = 0x02;
const TLS_HANDSHAKE: u8 = 0x16;
const TLS_APPLICATION_DATA: u8 = 0x17;
const TLS_MAJOR: u8 = 0x03;
const TLS_CLIENT_HELLO: u8 = 0x01;
const TLS_SERVER_HELLO: u8 = 0x02;
const TLS13_SUPPORTED_VERSIONS_EXT: [u8; 6] = [0x00, 0x2b, 0x00, 0x02, 0x03, 0x04];
const TLS13_CIPHER_SUITES: [u16; 4] = [0x1301, 0x1302, 0x1303, 0x1304];
const TLS_FILTER_PACKETS: usize = 8;

enum ReadState {
    Header {
        buf: Vec<u8>,
        need_uuid: bool,
    },
    /// Terminal clean EOF — latched so repeated `poll_read`s stay cheap and
    /// do not re-emit the mid-frame debug notes.
    Eof,
    Content {
        command: u8,
        remaining_content: usize,
        remaining_padding: usize,
    },
    Padding {
        command: u8,
        remaining_padding: usize,
    },
    Through,
}

struct PendingWrite {
    frame: Vec<u8>,
    pos: usize,
    consumed: usize,
    command: u8,
    end_padding_after_drain: bool,
}

pub struct VisionConn {
    inner: VlessConn,
    user_uuid: [u8; UUID_LEN],
    read_state: ReadState,
    write_pending: Option<PendingWrite>,
    write_padding: bool,
    write_sent_uuid: bool,
    write_seen_tls: bool,
    write_direct_enabled: bool,
    /// The transport can switch writes to raw (plain TLS or REALITY over
    /// the socket).  Without it the uplink ends padding with END, which a
    /// Vision server accepts, instead of DIRECT (issue #495).
    raw_write_capable: bool,
    read_tls_filter: ServerHelloFilter,
    vision_entered: bool,
}

impl VisionConn {
    pub fn new(mut inner: VlessConn, user_uuid: [u8; UUID_LEN]) -> Self {
        let raw_write_capable = inner.supports_raw_passthrough();
        Self {
            inner,
            user_uuid,
            read_state: ReadState::Header {
                buf: Vec::with_capacity(PADDING_HEADER_LEN),
                need_uuid: true,
            },
            write_pending: None,
            write_padding: true,
            write_sent_uuid: false,
            write_seen_tls: false,
            write_direct_enabled: false,
            raw_write_capable,
            read_tls_filter: ServerHelloFilter::new(),
            vision_entered: false,
        }
    }

    #[allow(dead_code)]
    pub fn vision_entered(&self) -> bool {
        self.vision_entered
    }

    fn drain_pending_write(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<usize>>> {
        let Some(pending) = &mut self.write_pending else {
            return Poll::Ready(Ok(None));
        };

        while pending.pos < pending.frame.len() {
            match Pin::new(&mut self.inner).poll_write(cx, &pending.frame[pending.pos..])? {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(0) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "vision: zero write during frame drain",
                    )));
                }
                Poll::Ready(n) => pending.pos += n,
            }
        }

        let pending = self.write_pending.take().expect("pending checked above");
        if pending.end_padding_after_drain {
            self.write_padding = false;
            if pending.command == COMMAND_PADDING_DIRECT {
                if !self.enable_inner_raw_write_passthrough() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "vision: DIRECT requested but transport cannot switch to raw passthrough",
                    )));
                }
                tracing::debug!("XTLS Vision direct write passthrough enabled");
            }
        }
        Poll::Ready(Ok(Some(pending.consumed)))
    }

    fn build_write_frame(&mut self, buf: &[u8]) {
        let contains_tls_handshake = contains_tls_client_hello(buf);
        let starts_tls_app_data =
            buf.len() >= 3 && buf[0] == TLS_APPLICATION_DATA && buf[1] == TLS_MAJOR;
        if contains_tls_handshake {
            self.write_seen_tls = true;
            self.vision_entered = true;
        }

        let padding_tls = self.write_seen_tls;
        let mut command = COMMAND_PADDING_CONTINUE;
        let mut end_after_drain = false;
        if starts_tls_app_data && self.write_seen_tls {
            command = if self.write_direct_enabled && self.raw_write_capable {
                COMMAND_PADDING_DIRECT
            } else {
                if self.write_direct_enabled {
                    tracing::debug!("XTLS Vision: transport cannot write raw, sending END");
                }
                COMMAND_PADDING_END
            };
            end_after_drain = true;
        } else if !self.write_seen_tls && !contains_tls_handshake {
            command = COMMAND_PADDING_END;
            end_after_drain = true;
        }

        let frame = build_padding_frame(
            command,
            (!self.write_sent_uuid).then_some(&self.user_uuid),
            buf,
            padding_tls,
        );
        tracing::debug!(
            command,
            content_len = buf.len(),
            frame_len = frame.len(),
            padding_tls,
            contains_tls_handshake,
            starts_tls_app_data,
            "XTLS Vision write padding"
        );
        self.write_sent_uuid = true;
        self.write_pending = Some(PendingWrite {
            frame,
            pos: 0,
            consumed: buf.len(),
            command,
            end_padding_after_drain: end_after_drain,
        });
    }

    fn enable_inner_raw_read_passthrough(&mut self) -> bool {
        self.inner.enable_raw_read_passthrough()
    }

    fn enable_inner_raw_write_passthrough(&mut self) -> bool {
        self.inner.enable_raw_write_passthrough()
    }

    fn filter_server_tls(&mut self, chunk: &[u8]) {
        if !self.write_direct_enabled && self.read_tls_filter.observe(chunk) {
            self.write_direct_enabled = true;
            tracing::debug!("XTLS Vision found TLS 1.3, direct enabled");
        }
    }
}

fn contains_tls_client_hello(buf: &[u8]) -> bool {
    buf.windows(6)
        .any(|w| w[0] == TLS_HANDSHAKE && w[1] == TLS_MAJOR && w[5] == TLS_CLIENT_HELLO)
}

struct ServerHelloFilter {
    packets_left: usize,
    expected_len: Option<usize>,
    buffer: Vec<u8>,
    done: bool,
}

impl ServerHelloFilter {
    fn new() -> Self {
        Self {
            packets_left: TLS_FILTER_PACKETS,
            expected_len: None,
            buffer: Vec::new(),
            done: false,
        }
    }

    fn observe(&mut self, chunk: &[u8]) -> bool {
        if self.done || self.packets_left == 0 || chunk.is_empty() {
            return false;
        }
        self.packets_left -= 1;

        if self.expected_len.is_none() {
            self.buffer.extend_from_slice(chunk);
            let Some(pos) = find_tls_server_hello_start(&self.buffer) else {
                return false;
            };
            if pos > 0 {
                self.buffer.drain(..pos);
            }
            let record_len = u16::from_be_bytes([self.buffer[3], self.buffer[4]]) as usize;
            self.expected_len = Some(5 + record_len);
        } else {
            self.buffer.extend_from_slice(chunk);
        }

        let expected_len = self.expected_len.unwrap_or(self.buffer.len());
        if self.buffer.len() < expected_len {
            return false;
        }

        self.done = true;
        let hello = &self.buffer[..expected_len];
        let Some(cipher) = tls_server_hello_cipher_suite(hello) else {
            return false;
        };
        TLS13_CIPHER_SUITES.contains(&cipher)
            && hello
                .windows(TLS13_SUPPORTED_VERSIONS_EXT.len())
                .any(|w| w == TLS13_SUPPORTED_VERSIONS_EXT)
    }
}

fn find_tls_server_hello_start(buf: &[u8]) -> Option<usize> {
    buf.windows(6).position(|w| {
        w[0] == TLS_HANDSHAKE && w[1] == TLS_MAJOR && w[2] == TLS_MAJOR && w[5] == TLS_SERVER_HELLO
    })
}

fn tls_server_hello_cipher_suite(record: &[u8]) -> Option<u16> {
    if record.len() < 46 || !matches!(find_tls_server_hello_start(record), Some(0)) {
        return None;
    }
    let session_id_len = *record.get(43)? as usize;
    let cipher_offset = 44 + session_id_len;
    let bytes = record.get(cipher_offset..cipher_offset + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn build_padding_frame(
    command: u8,
    user_uuid: Option<&[u8; UUID_LEN]>,
    content: &[u8],
    padding_tls: bool,
) -> Vec<u8> {
    let padding_len = if content.len() < 900 {
        let mut rng = rand::rng();
        if padding_tls {
            (rng.next_u32() as usize % 500) + 900 - content.len()
        } else {
            rng.next_u32() as usize % 256
        }
    } else {
        0
    };

    let header_len = if user_uuid.is_some() {
        PADDING_HEADER_LEN
    } else {
        PADDING_HEADER_LEN - UUID_LEN
    };
    let mut frame = Vec::with_capacity(header_len + content.len() + padding_len);
    if let Some(uuid) = user_uuid {
        frame.extend_from_slice(uuid);
    }
    frame.push(command);
    frame.extend_from_slice(&(content.len() as u16).to_be_bytes());
    frame.extend_from_slice(&(padding_len as u16).to_be_bytes());
    frame.extend_from_slice(content);
    frame.resize(frame.len() + padding_len, 0);
    frame
}

impl AsyncRead for VisionConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let state = std::mem::replace(&mut self.read_state, ReadState::Through);
            match state {
                ReadState::Eof => {
                    self.read_state = ReadState::Eof;
                    return Poll::Ready(Ok(()));
                }
                ReadState::Through => {
                    self.read_state = ReadState::Through;
                    return Pin::new(&mut self.inner).poll_read(cx, buf);
                }
                ReadState::Header {
                    buf: mut h,
                    need_uuid,
                } => {
                    let need = if need_uuid {
                        PADDING_HEADER_LEN
                    } else {
                        PADDING_HEADER_LEN - UUID_LEN
                    };
                    while h.len() < need {
                        let mut tmp = [0u8; PADDING_HEADER_LEN];
                        let want = (need - h.len()).min(tmp.len());
                        let mut rb = ReadBuf::new(&mut tmp[..want]);
                        match Pin::new(&mut self.inner).poll_read(cx, &mut rb) {
                            Poll::Pending => {
                                self.read_state = ReadState::Header { buf: h, need_uuid };
                                return Poll::Pending;
                            }
                            Poll::Ready(Err(e)) => {
                                self.read_state = ReadState::Header { buf: h, need_uuid };
                                return Poll::Ready(Err(e));
                            }
                            Poll::Ready(Ok(())) => {
                                let n = rb.filled().len();
                                if n == 0 {
                                    // Vision has no end-of-stream marker: a
                                    // peer finishes by simply closing the
                                    // transport, possibly mid-padding. Xray
                                    // and mihomo surface this as io.EOF, so
                                    // report a clean EOF and let the relay
                                    // take the graceful shutdown path.
                                    if h.is_empty() && need_uuid {
                                        tracing::debug!(
                                            "vision: EOF before first padding \
                                             frame (server closed without data)"
                                        );
                                    } else if !h.is_empty() {
                                        tracing::debug!(
                                            partial = h.len(),
                                            need,
                                            "vision: EOF mid padding header \
                                             (treated as clean close)"
                                        );
                                    }
                                    self.read_state = ReadState::Eof;
                                    return Poll::Ready(Ok(()));
                                }
                                h.extend_from_slice(rb.filled());
                            }
                        }
                    }

                    let offset = if need_uuid {
                        if h[..UUID_LEN] != self.user_uuid {
                            self.read_state = ReadState::Header { buf: h, need_uuid };
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "vision: server responded with unknown UUID",
                            )));
                        }
                        UUID_LEN
                    } else {
                        0
                    };
                    let command = h[offset];
                    let content_len = u16::from_be_bytes([h[offset + 1], h[offset + 2]]) as usize;
                    let padding_len = u16::from_be_bytes([h[offset + 3], h[offset + 4]]) as usize;
                    tracing::debug!(
                        command,
                        content_len,
                        padding_len,
                        need_uuid,
                        "XTLS Vision read padding"
                    );
                    self.read_state = ReadState::Content {
                        command,
                        remaining_content: content_len,
                        remaining_padding: padding_len,
                    };
                }
                ReadState::Content {
                    command,
                    mut remaining_content,
                    remaining_padding,
                } => {
                    if remaining_content == 0 {
                        self.read_state = ReadState::Padding {
                            command,
                            remaining_padding,
                        };
                        continue;
                    }
                    if buf.remaining() == 0 {
                        self.read_state = ReadState::Content {
                            command,
                            remaining_content,
                            remaining_padding,
                        };
                        return Poll::Ready(Ok(()));
                    }

                    let mut tmp = vec![0u8; remaining_content.min(buf.remaining()).min(8192)];
                    let mut rb = ReadBuf::new(&mut tmp);
                    match Pin::new(&mut self.inner).poll_read(cx, &mut rb) {
                        Poll::Pending => {
                            self.read_state = ReadState::Content {
                                command,
                                remaining_content,
                                remaining_padding,
                            };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => {
                            self.read_state = ReadState::Content {
                                command,
                                remaining_content,
                                remaining_padding,
                            };
                            return Poll::Ready(Err(e));
                        }
                        Poll::Ready(Ok(())) => {
                            let n = rb.filled().len();
                            if n == 0 {
                                // See the Header-state EOF arm: a mid-frame
                                // FIN is still a clean close — the bytes
                                // already delivered remain valid.
                                tracing::debug!(
                                    command,
                                    remaining_content,
                                    "vision: EOF mid padded content \
                                     (treated as clean close)"
                                );
                                self.read_state = ReadState::Eof;
                                return Poll::Ready(Ok(()));
                            }
                            remaining_content -= n;
                            self.filter_server_tls(rb.filled());
                            buf.put_slice(rb.filled());
                            self.read_state = if remaining_content == 0 {
                                ReadState::Padding {
                                    command,
                                    remaining_padding,
                                }
                            } else {
                                ReadState::Content {
                                    command,
                                    remaining_content,
                                    remaining_padding,
                                }
                            };
                            return Poll::Ready(Ok(()));
                        }
                    }
                }
                ReadState::Padding {
                    command,
                    mut remaining_padding,
                } => {
                    while remaining_padding > 0 {
                        let mut tmp = [0u8; 1024];
                        let want = remaining_padding.min(tmp.len());
                        let mut rb = ReadBuf::new(&mut tmp[..want]);
                        match Pin::new(&mut self.inner).poll_read(cx, &mut rb) {
                            Poll::Pending => {
                                self.read_state = ReadState::Padding {
                                    command,
                                    remaining_padding,
                                };
                                return Poll::Pending;
                            }
                            Poll::Ready(Err(e)) => {
                                self.read_state = ReadState::Padding {
                                    command,
                                    remaining_padding,
                                };
                                return Poll::Ready(Err(e));
                            }
                            Poll::Ready(Ok(())) => {
                                let n = rb.filled().len();
                                if n == 0 {
                                    tracing::debug!(
                                        command,
                                        remaining_padding,
                                        "vision: EOF mid padding \
                                         (treated as clean close)"
                                    );
                                    self.read_state = ReadState::Eof;
                                    return Poll::Ready(Ok(()));
                                }
                                remaining_padding -= n;
                            }
                        }
                    }

                    self.read_state = match command {
                        COMMAND_PADDING_CONTINUE => ReadState::Header {
                            buf: Vec::with_capacity(PADDING_HEADER_LEN - UUID_LEN),
                            need_uuid: false,
                        },
                        COMMAND_PADDING_END => ReadState::Through,
                        COMMAND_PADDING_DIRECT => {
                            if !self.enable_inner_raw_read_passthrough() {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::Unsupported,
                                    "vision: server sent DIRECT but this transport cannot switch \
                                     reads to raw (Vision needs raw TCP with tls or REALITY)",
                                )));
                            }
                            tracing::debug!("XTLS Vision direct passthrough enabled");
                            ReadState::Through
                        }
                        other => {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("vision: unknown padding command {other}"),
                            )));
                        }
                    };
                }
            }
        }
    }
}

impl AsyncWrite for VisionConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Poll::Ready(done) = this.drain_pending_write(cx) {
            if let Some(n) = done? {
                return Poll::Ready(Ok(n));
            }
        } else {
            return Poll::Pending;
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if !this.write_padding {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        // The padding frame carries u16 content/padding length fields:
        // chunk large writes so those fields cannot wrap and
        // desynchronise the peer's vision decoder.
        let chunk_len = buf.len().min(16 * 1024);
        this.build_write_frame(&buf[..chunk_len]);
        match this.drain_pending_write(cx) {
            Poll::Ready(Ok(Some(n))) => Poll::Ready(Ok(n)),
            Poll::Ready(Ok(None)) => Poll::Ready(Ok(0)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Poll::Ready(done) = self.drain_pending_write(cx) {
            done?;
        } else {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Poll::Ready(done) = self.drain_pending_write(cx) {
            done?;
        } else {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Unpin for VisionConn {}

impl ProxyConn for VisionConn {}

#[cfg(test)]
#[path = "../test/vless_vision_test.rs"]
mod tests;
