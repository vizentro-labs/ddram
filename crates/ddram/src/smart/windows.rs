// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Windows: `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageDeviceProtocolSpecificProperty`
//! (inbox StorNVMe driver, Windows 10+).

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use super::{Device, LOG_PAGE_LEN};

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;
const PROPERTY_STANDARD_QUERY: u32 = 0;
const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const PROTOCOL_DATA_SIZE: usize = 40;
const QUERY_HEADER: usize = 8;

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(path: &[u16], access: u32) -> io::Result<Handle> {
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(h))
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn get32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

pub fn get_log_page(dev: &Device, lid: u8, out: &mut [u8]) -> io::Result<()> {
    let h = open(&wide(std::ffi::OsStr::new(&dev.path)), 0)
        .or_else(|_| open(&wide(std::ffi::OsStr::new(&dev.path)), GENERIC_READ))?;
    let len = QUERY_HEADER + PROTOCOL_DATA_SIZE + out.len();
    let mut buf = vec![0u8; len];
    put32(&mut buf, 0, STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY);
    put32(&mut buf, 4, PROPERTY_STANDARD_QUERY);
    let p = QUERY_HEADER;
    put32(&mut buf, p, PROTOCOL_TYPE_NVME);
    put32(&mut buf, p + 4, NVME_DATA_TYPE_LOG_PAGE);
    put32(&mut buf, p + 8, lid as u32);
    put32(&mut buf, p + 12, 0);
    put32(&mut buf, p + 16, PROTOCOL_DATA_SIZE as u32);
    put32(&mut buf, p + 20, out.len() as u32);
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            buf.as_ptr().cast(),
            len as u32,
            buf.as_mut_ptr().cast(),
            len as u32,
            &mut ret,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // Output: STORAGE_PROTOCOL_DATA_DESCRIPTOR { Version, Size, ProtocolSpecificData }.
    let (ver, size) = (get32(&buf, 0), get32(&buf, 4));
    let off = get32(&buf, 8 + 16) as usize;
    let dlen = get32(&buf, 8 + 20) as usize;
    if ver != (8 + PROTOCOL_DATA_SIZE) as u32
        || size != ver
        || off < PROTOCOL_DATA_SIZE
        || dlen < out.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected protocol data descriptor",
        ));
    }
    let start = 8 + off;
    if start + out.len() > buf.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short log page"));
    }
    out.copy_from_slice(&buf[start..start + out.len()]);
    Ok(())
}

pub fn read_health(dev: &Device) -> io::Result<[u8; LOG_PAGE_LEN]> {
    let mut b = [0u8; LOG_PAGE_LEN];
    get_log_page(dev, 0x02, &mut b)?;
    Ok(b)
}

pub fn device_for_path(path: &Path) -> io::Result<Device> {
    let p = wide(path.as_os_str());
    let mut mount = [0u16; 512];
    if unsafe { GetVolumePathNameW(p.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut vol = [0u16; 128];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(mount.as_ptr(), vol.as_mut_ptr(), vol.len() as u32)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Strip the trailing backslash: open the volume, not its root directory.
    let mut n = vol.iter().position(|&c| c == 0).unwrap_or(vol.len());
    if n > 0 && vol[n - 1] == b'\\' as u16 {
        n -= 1;
    }
    let mut vpath = vol[..n].to_vec();
    vpath.push(0);
    let h = open(&vpath, 0)?;
    // VOLUME_DISK_EXTENTS { DWORD NumberOfDiskExtents; DISK_EXTENT Extents[] } with
    // DISK_EXTENT { DWORD DiskNumber; LARGE_INTEGER StartingOffset; LARGE_INTEGER ExtentLength } (24 B, 8-aligned).
    let mut out = [0u8; 8 + 24 * 8];
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            ptr::null(),
            0,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut ret,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if get32(&out, 0) == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "volume has no disk extents",
        ));
    }
    let disk = get32(&out, 8);
    Ok(Device {
        path: format!(r"\\.\PhysicalDrive{disk}"),
    })
}
