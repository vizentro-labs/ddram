// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Backing store: the file on the SSD that holds non-resident blocks.
//!
//! I/O is always whole blocks at block-aligned offsets into page-aligned
//! frames, which satisfies the alignment rules of `O_DIRECT` (Linux),
//! `F_NOCACHE` (macOS) and `FILE_FLAG_NO_BUFFERING` (Windows). Bypassing the OS
//! page cache matters: otherwise every block would be cached twice (frame +
//! page cache) and the RAM budget would be meaningless.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub struct Backing {
    file: File,
    pub path: PathBuf,
    pub len: u64,
    pub direct: bool,
    pub read_only: bool,
}

impl Backing {
    /// Open (creating if needed) a read/write backing file of at least `len` bytes.
    pub fn open(path: &Path, len: u64, direct: bool) -> io::Result<Backing> {
        let (file, direct) = open_file(path, true, direct)?;
        if file.metadata()?.len() < len {
            file.set_len(len)?;
        }
        preallocate(&file, len);
        Ok(Backing {
            file,
            path: path.to_path_buf(),
            len,
            direct,
            read_only: false,
        })
    }

    /// Open an existing file read-only (e.g. model weights or game assets).
    pub fn open_read_only(path: &Path, direct: bool) -> io::Result<Backing> {
        let (file, direct) = open_file(path, false, direct)?;
        let len = file.metadata()?.len();
        Ok(Backing {
            file,
            path: path.to_path_buf(),
            len,
            direct,
            read_only: true,
        })
    }

    /// Read a block. Bytes past end-of-file are zero-filled.
    pub fn read_block(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            if pos >= self.len {
                break;
            }
            match read_at(&self.file, &mut buf[done..], pos) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        buf[done..].fill(0);
        Ok(())
    }

    /// Read consecutive blocks starting at `off` into separate frames
    /// (`preadv` on Unix: one request for the whole run).
    pub fn read_blocks(&self, off: u64, bs: usize, frames: &[*mut u8]) -> io::Result<()> {
        #[cfg(unix)]
        if frames.len() > 1 && off + (frames.len() * bs) as u64 <= self.len {
            use std::os::unix::io::AsRawFd;
            let iov: Vec<libc::iovec> = frames
                .iter()
                .map(|&p| libc::iovec {
                    iov_base: p.cast(),
                    iov_len: bs,
                })
                .collect();
            let want = frames.len() * bs;
            let n = loop {
                let r = unsafe {
                    libc::preadv(
                        self.file.as_raw_fd(),
                        iov.as_ptr(),
                        iov.len() as _,
                        off as libc::off_t,
                    )
                };
                if r >= 0 {
                    break r as usize;
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            };
            if n == want {
                return Ok(());
            }
            // Short read: finish block by block.
            for (i, &p) in frames.iter().enumerate().skip(n / bs) {
                let buf = unsafe { std::slice::from_raw_parts_mut(p, bs) };
                self.read_block(off + (i * bs) as u64, buf)?;
            }
            return Ok(());
        }
        for (i, &p) in frames.iter().enumerate() {
            let buf = unsafe { std::slice::from_raw_parts_mut(p, bs) };
            self.read_block(off + (i * bs) as u64, buf)?;
        }
        Ok(())
    }

    /// Write consecutive blocks starting at `off` from separate frames
    /// (`pwritev` on Unix: one request for the whole run).
    pub fn write_blocks(&self, off: u64, bs: usize, frames: &[*const u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read-only backing",
            ));
        }
        #[cfg(unix)]
        if frames.len() > 1 {
            use std::os::unix::io::AsRawFd;
            let iov: Vec<libc::iovec> = frames
                .iter()
                .map(|&p| libc::iovec {
                    iov_base: p as *mut _,
                    iov_len: bs,
                })
                .collect();
            let n = loop {
                let r = unsafe {
                    libc::pwritev(
                        self.file.as_raw_fd(),
                        iov.as_ptr(),
                        iov.len() as _,
                        off as libc::off_t,
                    )
                };
                if r >= 0 {
                    break r as usize;
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            };
            // Short write: finish block by block.
            for (i, &p) in frames.iter().enumerate().skip(n / bs) {
                let buf = unsafe { std::slice::from_raw_parts(p, bs) };
                self.write_block(off + (i * bs) as u64, buf)?;
            }
            return Ok(());
        }
        for (i, &p) in frames.iter().enumerate() {
            let buf = unsafe { std::slice::from_raw_parts(p, bs) };
            self.write_block(off + (i * bs) as u64, buf)?;
        }
        Ok(())
    }

    pub fn write_block(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read-only backing",
            ));
        }
        let mut done = 0usize;
        while done < buf.len() {
            match write_at(&self.file, &buf[done..], off + done as u64) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    pub fn file(&self) -> &File {
        &self.file
    }
}

/// Reserve disk blocks for the whole file up front. On Linux/ext4, a direct
/// write into a hole must allocate blocks and takes the inode lock
/// exclusively, which stalls every concurrent direct read (measured: 10k vs
/// 46k random reads/s on 4 threads while dirty blocks were being written).
#[allow(unused_variables)]
fn preallocate(f: &File, len: u64) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, len as libc::off_t) };
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let mut fs = libc::fstore_t {
            fst_flags: libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: len as libc::off_t,
            fst_bytesalloc: 0,
        };
        unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PREALLOCATE, &mut fs) };
    }
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, off)
}
#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(f, buf, off)
}
#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, off)
}
#[cfg(windows)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(f, buf, off)
}

fn base_options(write: bool) -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true);
    if write {
        o.write(true).create(true);
    }
    o
}

#[cfg(target_os = "linux")]
fn open_file(path: &Path, write: bool, direct: bool) -> io::Result<(File, bool)> {
    use std::os::unix::fs::OpenOptionsExt;
    if direct {
        let mut o = base_options(write);
        o.custom_flags(libc::O_DIRECT);
        match o.open(path) {
            Ok(f) => return Ok((f, true)),
            // tmpfs and some filesystems reject O_DIRECT; fall back to buffered.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
            Err(e) => return Err(e),
        }
    }
    let f = base_options(write).open(path)?;
    if direct {
        // Buffered fallback: at least tell the kernel not to cache aggressively.
        use std::os::unix::io::AsRawFd;
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM) };
    }
    Ok((f, false))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_file(path: &Path, write: bool, direct: bool) -> io::Result<(File, bool)> {
    use std::os::unix::io::AsRawFd;
    let f = base_options(write).open(path)?;
    let mut ok = false;
    if direct {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            ok = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1) } == 0;
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            let _ = f.as_raw_fd();
        }
    }
    Ok((f, ok))
}

#[cfg(windows)]
fn open_file(path: &Path, write: bool, direct: bool) -> io::Result<(File, bool)> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_NO_BUFFERING, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let mut o = base_options(write);
    o.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    if direct {
        o.custom_flags(FILE_FLAG_NO_BUFFERING);
    }
    Ok((o.open(path)?, direct))
}
