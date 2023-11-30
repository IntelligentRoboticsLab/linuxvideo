//! Streaming I/O.

use std::ffi::c_void;
use std::fs::File;
use std::mem;
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::os::raw::c_int;
use std::os::unix::prelude::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::{io, slice};

use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};

use crate::buf_type::BufType;
use crate::raw;
use crate::shared::{BufFlag, Memory};

enum AllocType {
    /// The buffer was `mmap`ped into our address space, use `munmap` to free it.
    Mmap,
}

pub struct Frame {
    index: u32,
    buffer: Buffer,
    stream: Arc<Mutex<(u32, ReadStream)>>,
}

impl Deref for Frame {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let mut guard = self.stream.lock().unwrap();
        guard.0 += 1;

        match guard.1.enqueue_buf(self.index) {
            Ok(_) => (),
            Err(_) => eprintln!("Frame: Destructor could not enqueue the buffer"),
        }
    }
}

pub struct FrameProvider {
    stream: Arc<Mutex<(u32, ReadStream)>>,
}

impl FrameProvider {
    pub fn fetch_frame(&mut self) -> io::Result<Frame> {
        let mut guard = self.stream.lock().unwrap();

        if guard.0 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "All buffers are currently in use",
            ));
        }

        let (index, buffer) = guard.1.dequeue_buf()?;
        let buffer = buffer.clone();
        guard.0 -= 1;

        Ok(Frame {
            index,
            buffer,
            stream: self.stream.clone(),
        })
    }
}

impl FrameProvider {
    pub(crate) fn new(stream: ReadStream, num_buffers: u32) -> Self {
        FrameProvider {
            stream: Arc::new(Mutex::new((num_buffers, stream))),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Buffer {
    /// Pointer in our address space where this buffer is mapped or allocated.
    ptr: *mut c_void,
    /// Size of the buffer in bytes.
    length: u32,
    queued: bool,
}

impl Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { slice::from_raw_parts(self.ptr as *const u8, self.length as usize) }
    }
}

/// Owns all buffers allocated or mapped for a device stream.
struct Buffers {
    ty: AllocType,
    /// The buffer index equals its index in this vector.
    buffers: Vec<Buffer>,
}

unsafe impl Send for Buffers {}
unsafe impl Sync for Buffers {}

/// Number of buffers we request by default.
pub(super) const DEFAULT_BUFFER_COUNT: u32 = 2;

impl Buffers {
    fn allocate(
        fd: c_int,
        buf_type: BufType,
        mem_type: Memory,
        mut buffer_count: u32,
    ) -> io::Result<Self> {
        let alloc_type = match mem_type {
            Memory::MMAP => AllocType::Mmap,
            _ => unimplemented!("only `mmap` memory type is currently supported"),
        };

        let mut req_bufs: raw::RequestBuffers = unsafe { mem::zeroed() };
        req_bufs.count = buffer_count;
        req_bufs.type_ = buf_type;
        req_bufs.memory = mem_type;

        unsafe {
            raw::reqbufs(fd, &mut req_bufs)?;
        }

        log::debug!("{:?}", req_bufs);

        if req_bufs.count < buffer_count {
            log::trace!("failed to allocate {buffer_count} buffers (driver only allocated {0}), using {0} instead", req_bufs.count);
            buffer_count = req_bufs.count;
        }

        // Query the buffer locations and map them into our process.
        let mut buffers = Vec::with_capacity(buffer_count as usize);
        for i in 0..buffer_count {
            let mut buf: raw::Buffer = unsafe { mem::zeroed() };
            buf.type_ = buf_type;
            buf.memory = mem_type;
            buf.index = i;

            unsafe {
                raw::querybuf(fd, &mut buf)?;
            }

            // NB: buffer sizes are usually `PixFormat::size_image(_)` rounded up to whole pages
            let ptr = unsafe {
                mmap(
                    None,
                    NonZeroUsize::try_from(buf.length as usize)
                        .expect("V4L2 returned buffer size of 0"),
                    // XXX is PROT_WRITE allowed for `ReadStream`s?
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_SHARED,
                    fd,
                    buf.m.offset.into(),
                )?
            };

            assert_eq!(buf.index, i);
            assert_eq!(buf.index as usize, buffers.len());

            buffers.push(Buffer {
                ptr,
                length: buf.length,
                queued: false,
            });
        }

        Ok(Self {
            ty: alloc_type,
            buffers,
        })
    }
}

impl Drop for Buffers {
    fn drop(&mut self) {
        for buffer in &self.buffers {
            match self.ty {
                AllocType::Mmap => unsafe {
                    munmap(buffer.ptr, buffer.length as usize).ok();
                },
            }
        }
    }
}

/// A stream that reads data from a V4L2 device.
pub struct ReadStream {
    file: File,
    buffers: Buffers,
    buf_type: BufType,
    mem_type: Memory,
}

impl ReadStream {
    pub(crate) fn new(
        file: File,
        buf_type: BufType,
        mem_type: Memory,
        buffer_count: u32,
    ) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        let buffers = Buffers::allocate(fd, buf_type, mem_type, buffer_count)?;

        let mut this = Self {
            file,
            buffers,
            buf_type,
            mem_type,
        };
        this.enqueue_all()?;
        this.stream_on()?;

        Ok(this)
    }

    fn enqueue(&mut self, index: u32) -> io::Result<()> {
        let mut buf: raw::Buffer = unsafe { mem::zeroed() };
        buf.type_ = self.buf_type;
        buf.memory = self.mem_type;
        buf.index = index;

        unsafe {
            raw::qbuf(self.file.as_raw_fd(), &mut buf)?;
        }

        self.buffers.buffers[index as usize].queued = true;

        Ok(())
    }

    pub(crate) fn enqueue_buf(&mut self, index: u32) -> io::Result<()> {
        self.enqueue(index)
    }

    fn enqueue_all(&mut self) -> io::Result<()> {
        for i in 0..self.buffers.buffers.len() {
            if !self.buffers.buffers[i].queued {
                self.enqueue(i as u32)?;
            }
        }
        Ok(())
    }

    /// Starts streaming.
    ///
    /// This function can potentially block for a noticeable amount of time.
    fn stream_on(&mut self) -> io::Result<()> {
        unsafe {
            raw::streamon(self.file.as_raw_fd(), &self.buf_type)?;
        }

        Ok(())
    }

    // XXX to publicly expose this, we have to handle the fact that it dequeues all buffers
    fn stream_off(&mut self) -> io::Result<()> {
        unsafe {
            raw::streamoff(self.file.as_raw_fd(), &self.buf_type)?;
        }

        for b in &mut self.buffers.buffers {
            b.queued = false;
        }

        Ok(())
    }

    /// Dequeues a buffer, passes it to `cb`, then enqueues it again.
    ///
    /// If `cb` returns an error, this function will still try to enqueue the buffer again. If that
    /// fails, the error that occurred during enqueuing will be returned, if it succeeds, the error
    /// returned by `cb` will be returned.
    pub fn dequeue<T>(
        &mut self,
        cb: impl FnOnce(ReadBufferView<'_>) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut buf: raw::Buffer = unsafe { mem::zeroed() };
        buf.type_ = self.buf_type;
        buf.memory = self.mem_type;

        unsafe {
            raw::dqbuf(self.file.as_raw_fd(), &mut buf)?;
        }

        let buffer = &mut self.buffers.buffers[buf.index as usize];
        buffer.queued = false;
        let data =
            unsafe { slice::from_raw_parts(buffer.ptr as *const u8, buffer.length as usize) };
        let view = ReadBufferView {
            flags: buf.flags,
            data,
            bytesused: buf.bytesused as usize,
        };

        let res = cb(view);
        // XXX not sure if we should short-circuit here

        self.enqueue(buf.index)?;

        res
    }

    pub(crate) fn dequeue_buf(&mut self) -> io::Result<(u32, &mut Buffer)> {
        let mut buf: raw::Buffer = unsafe { mem::zeroed() };
        buf.type_ = self.buf_type;
        buf.memory = self.mem_type;

        unsafe {
            raw::dqbuf(self.file.as_raw_fd(), &mut buf)?;
        }

        let buffer = &mut self.buffers.buffers[buf.index as usize];
        buffer.queued = false;

        Ok((buf.index, buffer))
    }

    pub fn into_frame_provider(self) -> FrameProvider {
        let num_buffers = self.buffers.buffers.len() as u32;

        FrameProvider::new(self, num_buffers)
    }

    /// Tests whether the next call to [`ReadStream::dequeue`] will block.
    ///
    /// If this returns `false`, a filled buffer is already available and the next call to
    /// [`ReadStream::dequeue`] will not block, but finish immediately. If this returns `true`,
    /// the next call will block until the next buffer is available.
    pub fn will_block(&self) -> io::Result<bool> {
        for i in 0..self.buffers.buffers.len() {
            let mut buf: raw::Buffer = unsafe { mem::zeroed() };
            buf.type_ = self.buf_type;
            buf.memory = self.mem_type;
            buf.index = i as u32;

            unsafe {
                raw::querybuf(self.file.as_raw_fd(), &mut buf)?;
            }

            if buf.flags.contains(BufFlag::DONE) {
                // A buffer is marked `DONE`, so it will be returned immediately when calling
                // `dequeue`.
                return Ok(false);
            }
        }

        Ok(true)
    }
}

impl Drop for ReadStream {
    fn drop(&mut self) {
        // Turn off the stream to dequeue all buffers.
        // This must be done before `Buffers` can be dropped safely, at least for userptr I/O.
        self.stream_off().ok();
    }
}

impl AsRawFd for ReadStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// Immutable view into a dequeued (filled) read buffer.
///
/// Dereferences to a byte slice.
pub struct ReadBufferView<'a> {
    flags: BufFlag,
    data: &'a [u8],
    bytesused: usize,
}

impl<'a> ReadBufferView<'a> {
    /// Returns whether the error flag for this buffer is set.
    ///
    /// If this returns `true`, the application should expect data corruption in the buffer data.
    #[inline]
    pub fn is_error(&self) -> bool {
        self.flags.contains(BufFlag::ERROR)
    }

    /// Returns a reference to the *entire* backing buffer.
    ///
    /// [`ReadBufferView`] dereferences to the *used* portion of the buffer. For fixed-size
    /// (uncompressed) image formats, the *used* portion is typically equal to the entire buffer,
    /// but for compressed formats like MJPEG, the backing buffer, which can be access with this
    /// method, is usually a lot larger than the actual data of interest.
    ///
    /// Normally, this method does not need to be used, as only the used portion of the buffer is
    /// needed.
    #[inline]
    pub fn raw_buffer(&self) -> &'a [u8] {
        self.data
    }
}

impl Deref for ReadBufferView<'_> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.data[..self.bytesused]
    }
}

/// A stream that writes to a V4L2 device.
pub struct WriteStream {
    file: File,
    buffers: Buffers,
    next_unqueued_buffer: Option<usize>,
    buf_type: BufType,
    mem_type: Memory,
}

impl WriteStream {
    pub(crate) fn new(
        file: File,
        buf_type: BufType,
        mem_type: Memory,
        buffer_count: u32,
    ) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        let buffers = Buffers::allocate(fd, buf_type, mem_type, buffer_count)?;

        Ok(Self {
            file,
            buffers,
            next_unqueued_buffer: Some(0),
            buf_type,
            mem_type,
        })
    }

    fn enqueue_buffer(&mut self, index: u32) -> io::Result<()> {
        let mut buf: raw::Buffer = unsafe { mem::zeroed() };
        buf.type_ = self.buf_type;
        buf.memory = self.mem_type;
        buf.index = index;

        unsafe {
            raw::qbuf(self.file.as_raw_fd(), &mut buf)?;
        }

        self.buffers.buffers[index as usize].queued = true;

        Ok(())
    }

    /// Passes a non-queued buffer to `cb` to fill it with data, then enqueues it for outputting.
    ///
    /// If no unqueued buffer is available, one is dequeued first (which may block until one is
    /// available).
    pub fn enqueue<T>(
        &mut self,
        cb: impl FnOnce(WriteBufferView<'_>) -> io::Result<T>,
    ) -> io::Result<T> {
        let buf_index = match self.next_unqueued_buffer {
            Some(i) => i,
            None => {
                // All buffers are enqueued with the driver. Dequeue one.
                let mut buf: raw::Buffer = unsafe { mem::zeroed() };
                buf.type_ = self.buf_type;
                buf.memory = self.mem_type;

                unsafe {
                    raw::dqbuf(self.file.as_raw_fd(), &mut buf)?;
                }

                let buf_index = buf.index as usize;
                self.buffers.buffers[buf_index].queued = false;
                buf_index
            }
        };

        let buffer = &mut self.buffers.buffers[buf_index];
        assert!(!buffer.queued);

        let data =
            unsafe { slice::from_raw_parts_mut(buffer.ptr as *mut u8, buffer.length as usize) };
        let view = WriteBufferView { data };
        match cb(view) {
            Ok(val) => match self.enqueue_buffer(buf_index as u32) {
                Ok(()) => {
                    match self.next_unqueued_buffer {
                        Some(i) => {
                            if i + 1 == self.buffers.buffers.len() {
                                // Out of buffers we know are unqueued.
                                self.next_unqueued_buffer = None;
                            } else {
                                // Next buffer was never enqueued, so use that next.
                                self.next_unqueued_buffer = Some(i + 1);
                            }
                        }
                        None => {
                            // Do nothing, next call will dequeue.
                        }
                    }
                    Ok(val)
                }
                Err(e) => {
                    // `buf_index` is definitely unqueued now.
                    self.next_unqueued_buffer = Some(buf_index);
                    Err(e)
                }
            },
            Err(e) => {
                // `buf_index` is definitely unqueued now.
                self.next_unqueued_buffer = Some(buf_index);
                Err(e)
            }
        }
    }
}

/// Mutable view into an unqueued write buffer.
///
/// Dereferences to a byte slice.
pub struct WriteBufferView<'a> {
    data: &'a mut [u8],
}

impl Deref for WriteBufferView<'_> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl DerefMut for WriteBufferView<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_types_are_send_sync() {
        fn assert<T: Send + Sync>() {}

        assert::<WriteStream>();
        assert::<ReadStream>();
        assert::<WriteBufferView<'_>>();
        assert::<ReadBufferView<'_>>();
    }
}
