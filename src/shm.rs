use std::io;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::{Dispatch, QueueHandle};

/// A `wl_buffer` backed by memory shared with the compositor.
pub struct ShmBuffer {
    pub wl_buffer: WlBuffer,
    mapping: Mapping,
    /// The compositor still reads from this buffer.
    pub busy: bool,
    /// Rows that differ from the background.
    pub dirty_rows: Vec<std::ops::Range<usize>>,
}

struct Mapping {
    pointer: NonNull<u32>,
    pixel_count: usize,
}

impl ShmBuffer {
    pub fn new<State, UserData>(
        shm: &WlShm,
        width: usize,
        height: usize,
        queue_handle: &QueueHandle<State>,
        user_data: UserData,
    ) -> io::Result<Self>
    where
        State: Dispatch<wayland_client::protocol::wl_shm_pool::WlShmPool, ()>
            + Dispatch<WlBuffer, UserData>
            + 'static,
        UserData: Send + Sync + 'static,
    {
        let too_large = || io::Error::other("buffer is too large");
        let pixel_count = width.checked_mul(height).ok_or_else(too_large)?;
        let byte_count = pixel_count.checked_mul(4).ok_or_else(too_large)?;
        let wire_size = i32::try_from(byte_count).map_err(|_| too_large())?;
        let wire_width = i32::try_from(width).map_err(|_| too_large())?;
        let wire_height = i32::try_from(height).map_err(|_| too_large())?;
        if pixel_count == 0 {
            return Err(io::Error::other("buffer is empty"));
        }

        let fd = create_memfd(byte_count)?;
        let mapping = Mapping::new(&fd, pixel_count)?;

        let pool = shm.create_pool(fd.as_fd(), wire_size, queue_handle, ());
        let wl_buffer = pool.create_buffer(
            0,
            wire_width,
            wire_height,
            wire_width * 4,
            Format::Xrgb8888,
            queue_handle,
            user_data,
        );
        pool.destroy();

        Ok(Self {
            wl_buffer,
            mapping,
            busy: false,
            dirty_rows: Vec::new(),
        })
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        // SAFETY: The mapping holds `pixel_count` pixels and lives as long as `self`. The
        // compositor only reads while `busy`, and tearing is harmless for pixels.
        unsafe {
            std::slice::from_raw_parts_mut(self.mapping.pointer.as_ptr(), self.mapping.pixel_count)
        }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.wl_buffer.destroy();
    }
}

impl Mapping {
    fn new(fd: &OwnedFd, pixel_count: usize) -> io::Result<Self> {
        use std::os::fd::AsRawFd;

        // SAFETY: Mapping a freshly sized memfd has no other side effects.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                pixel_count * 4,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let pointer = NonNull::new(pointer.cast::<u32>())
            .ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self {
            pointer,
            pixel_count,
        })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: The pointer and length come from the `mmap` call in `Mapping::new`.
        unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.pixel_count * 4) };
    }
}

fn create_memfd(byte_count: usize) -> io::Result<OwnedFd> {
    // SAFETY: The name is a valid nul terminated string.
    let raw_fd = unsafe { libc::memfd_create(c"glitchlock".as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `memfd_create` returned a new file descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let length =
        libc::off_t::try_from(byte_count).map_err(|_| io::Error::other("buffer is too large"))?;
    // SAFETY: `fd` is a valid memfd.
    if unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), length) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}
