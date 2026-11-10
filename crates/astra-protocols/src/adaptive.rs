//! Bounded copy buffers. Capacity grows only after completed full transfers;
//! idle storage is released only when no application bytes remain buffered.
use std::{
    future::{poll_fn, Future},
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, Sleep},
};

pub const MIN_SIZE: usize = 4096;
const IDLE: Duration = Duration::from_secs(30);
const BURST: Duration = Duration::from_millis(100);

pub struct Buffer {
    bytes: Box<[u8]>,
    max: usize,
    streak: u8,
    burst: Instant,
    last: Instant,
    timer: Option<Pin<Box<Sleep>>>,
}
impl Buffer {
    pub fn new(max: usize) -> Self {
        assert!(max >= MIN_SIZE);
        astra_observe::gauge!("buffer.copy.bytes", "转发缓冲已分配", "bytes").add(MIN_SIZE as u64);
        Self {
            bytes: vec![0; MIN_SIZE].into_boxed_slice(),
            max,
            streak: 0,
            burst: Instant::now(),
            last: Instant::now(),
            timer: None,
        }
    }
    pub fn bytes(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    fn release(&mut self) {
        astra_observe::gauge!("buffer.copy.bytes", "转发缓冲已分配", "bytes")
            .subtract(self.bytes.len() as u64);
        self.bytes = Box::new([]);
        self.timer = None;
    }
    fn resize(&mut self, size: usize) {
        let old = self.bytes.len();
        self.bytes = vec![0; size].into_boxed_slice();
        let gauge = astra_observe::gauge!("buffer.copy.bytes", "转发缓冲已分配", "bytes");
        if size > old {
            gauge.add((size - old) as u64);
            astra_observe::counter!("buffer.copy.grows", "转发缓冲扩容次数").add(1);
        } else {
            gauge.subtract((old - size) as u64);
            astra_observe::counter!("buffer.copy.shrinks", "转发缓冲收缩次数").add(1);
        }
    }
    /// Call only after every byte in this batch has been accepted by the writer.
    pub fn completed(&mut self, size: usize) {
        let now = Instant::now();
        self.last = now;
        if self.bytes.len() == self.max {
            return;
        }
        if size != self.bytes.len() {
            self.streak = 0;
            return;
        }
        if self.streak == 0 || now.duration_since(self.burst) > BURST {
            self.streak = 0;
            self.burst = now;
        }
        self.streak += 1;
        if self.streak == 4 {
            self.resize((self.bytes.len() * 2).min(self.max));
            self.streak = 0;
        }
    }
    // Only polled while empty. The deadline is registered lazily once reads park.
    fn poll_idle(&mut self, cx: &mut Context<'_>) {
        if self.bytes.len() == MIN_SIZE {
            return;
        }
        let deadline = self.last + IDLE;
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        if timer.deadline() != deadline {
            timer.as_mut().reset(deadline);
        }
        if timer.as_mut().poll(cx).is_ready() {
            self.resize(MIN_SIZE);
            self.streak = 0;
            self.timer = None;
        }
    }
    pub async fn read<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        reader: &mut R,
    ) -> io::Result<usize> {
        poll_fn(|cx| {
            let mut buf = ReadBuf::new(&mut self.bytes);
            match Pin::new(&mut *reader).poll_read(cx, &mut buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.filled().len())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => {
                    self.poll_idle(cx);
                    Poll::Pending
                }
            }
        })
        .await
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        astra_observe::gauge!("buffer.copy.bytes", "转发缓冲已分配", "bytes")
            .subtract(self.bytes.len() as u64);
    }
}

struct Direction {
    buffer: Buffer,
    pos: usize,
    end: usize,
    total: u64,
    eof: bool,
    flush: bool,
    shutting_down: bool,
    done: bool,
}
impl Direction {
    fn new(max: usize) -> Self {
        Self {
            buffer: Buffer::new(max),
            pos: 0,
            end: 0,
            total: 0,
            eof: false,
            flush: false,
            shutting_down: false,
            done: false,
        }
    }
    fn poll<R: AsyncRead + Unpin + ?Sized, W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        reader: &mut R,
        writer: &mut W,
    ) -> Poll<io::Result<u64>> {
        if self.done {
            return Poll::Ready(Ok(self.total));
        }
        // Bound individual I/O attempts, including interrupted and partial writes.
        // A ready partial write can continue without a scheduler round trip.
        let mut attempts = 0;
        loop {
            if attempts == 128 {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if self.pos == self.end && !self.eof {
                attempts += 1;
                let mut buf = ReadBuf::new(self.buffer.bytes());
                match Pin::new(&mut *reader).poll_read(cx, &mut buf) {
                    Poll::Ready(Ok(())) => {
                        self.end = buf.filled().len();
                        self.pos = 0;
                        self.eof = self.end == 0;
                    }
                    Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => {
                        self.buffer.poll_idle(cx);
                        if self.flush {
                            std::task::ready!(Pin::new(&mut *writer).poll_flush(cx))?;
                            self.flush = false;
                        }
                        return Poll::Pending;
                    }
                }
            }
            while self.pos < self.end {
                if attempts == 128 {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                attempts += 1;
                let n =
                    match std::task::ready!(Pin::new(&mut *writer)
                        .poll_write(cx, &self.buffer.bytes()[self.pos..self.end]))
                    {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        result => result?,
                    };
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                self.pos += n;
                self.total += n as u64;
                self.flush = true;
            }
            if self.end > 0 {
                self.buffer.completed(self.end);
                self.pos = 0;
                self.end = 0;
            }
            if self.eof {
                if !self.shutting_down {
                    std::task::ready!(Pin::new(&mut *writer).poll_flush(cx))?;
                    self.shutting_down = true;
                }
                std::task::ready!(Pin::new(&mut *writer).poll_shutdown(cx))?;
                self.buffer.release();
                self.done = true;
                return Poll::Ready(Ok(self.total));
            }
        }
    }
}

/// Independent EOF in each direction; cancellation drops both owned buffers.
pub async fn copy_bidirectional<
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
>(
    a: &mut A,
    b: &mut B,
    max: usize,
) -> io::Result<(u64, u64)> {
    let mut up = Direction::new(max);
    let mut down = Direction::new(max);
    poll_fn(|cx| {
        let u = up.poll(cx, a, b)?;
        let d = down.poll(cx, b, a)?;
        Poll::Ready(Ok((std::task::ready!(u), std::task::ready!(d))))
    })
    .await
}

#[cfg(test)]
#[path = "test/adaptive_test.rs"]
mod tests;
