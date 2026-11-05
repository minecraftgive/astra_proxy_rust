//! Synchronous caller API backed by cancellable overlapped named-pipe I/O.
//! Every operation drains its completion before releasing the OS buffers.
use super::{wide, Handle, MAX_FRAME};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    ffi::OsStr,
    io,
    ptr::{null, null_mut},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES,
        FILE_READ_DATA, FILE_WRITE_DATA, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT, SYNCHRONIZE,
    },
    System::{
        Threading::{CreateEventW, WaitForSingleObject},
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

pub(super) struct Pipe(Handle);
impl Pipe {
    pub(super) fn open(name: &OsStr) -> io::Result<Self> {
        let name = wide(name)?;
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(Handle(handle)))
        }
    }
    pub(super) fn handle(&self) -> HANDLE {
        self.0 .0
    }

    pub(super) fn send<T: Serialize>(&self, value: &T, deadline: Instant) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        if bytes.is_empty() || bytes.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "service request exceeds 64 KiB",
            ));
        }
        self.write_all(&(bytes.len() as u32).to_le_bytes(), deadline)?;
        self.write_all(&bytes, deadline)
    }
    pub(super) fn receive<T: DeserializeOwned>(&self, deadline: Instant) -> io::Result<T> {
        let mut header = [0; 4];
        self.read_exact(&mut header, deadline)?;
        let length = u32::from_le_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid service frame length",
            ));
        }
        let mut bytes = vec![0; length];
        self.read_exact(&mut bytes, deadline)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }
    pub(super) fn read_exact(&self, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let length = self.transfer(Buffer::Read(bytes), deadline)?;
            if length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Windows service disconnected",
                ));
            }
            bytes = &mut bytes[length..];
        }
        Ok(())
    }
    pub(super) fn write_all(&self, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let length = self.transfer(Buffer::Write(bytes), deadline)?;
            if length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Windows service stopped receiving",
                ));
            }
            bytes = &bytes[length..];
        }
        Ok(())
    }

    fn transfer(&self, buffer: Buffer<'_>, deadline: Instant) -> io::Result<usize> {
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = Handle(event);
        let mut operation = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        let mut transferred = 0;
        let ok = unsafe {
            match buffer {
                Buffer::Read(bytes) => ReadFile(
                    self.handle(),
                    bytes.as_mut_ptr(),
                    bytes.len() as u32,
                    null_mut(),
                    &mut operation,
                ),
                Buffer::Write(bytes) => WriteFile(
                    self.handle(),
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    null_mut(),
                    &mut operation,
                ),
            }
        };
        if ok != 0 {
            return if unsafe { GetOverlappedResult(self.handle(), &operation, &mut transferred, 0) }
                != 0
            {
                Ok(transferred as usize)
            } else {
                Err(io::Error::last_os_error())
            };
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let milliseconds = remaining
            .as_millis()
            .saturating_add(u128::from(
                !remaining.subsec_nanos().is_multiple_of(1_000_000),
            ))
            .min(u32::MAX as u128 - 1) as u32;
        let wait = unsafe { WaitForSingleObject(event.0, milliseconds) };
        if wait == WAIT_OBJECT_0 {
            if unsafe { GetOverlappedResult(self.handle(), &operation, &mut transferred, 0) } != 0 {
                return Ok(transferred as usize);
            }
            return Err(io::Error::last_os_error());
        }
        let error = if wait == WAIT_TIMEOUT {
            timed_out()
        } else {
            io::Error::last_os_error()
        };
        // Cancel only this operation, then wait for the kernel to stop touching
        // its OVERLAPPED and buffers. No worker thread survives a timeout.
        unsafe {
            CancelIoEx(self.handle(), &operation);
        }
        if unsafe { GetOverlappedResult(self.handle(), &operation, &mut transferred, 1) } != 0 {
            // Completion won the race with cancellation: preserve its bytes.
            return Ok(transferred as usize);
        }
        Err(error)
    }
}
enum Buffer<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}
fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Windows service request timed out")
}

#[cfg(test)]
#[path = "../test/windows_service_client_io_test.rs"]
mod tests;
