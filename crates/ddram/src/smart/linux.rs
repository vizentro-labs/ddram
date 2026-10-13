// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Linux: NVMe admin passthrough (`NVME_IOCTL_ADMIN_CMD`), requires CAP_SYS_ADMIN.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use super::{Device, LOG_PAGE_LEN};

#[repr(C)]
#[derive(Default)]
struct NvmePassthruCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

const _: () = assert!(std::mem::size_of::<NvmePassthruCmd>() == 72);

// _IOWR('N', 0x41, struct nvme_admin_cmd)
const NVME_IOCTL_ADMIN_CMD: libc::c_ulong =
    (3 << 30) | (72 << 16) | ((b'N' as libc::c_ulong) << 8) | 0x41;

pub fn get_log_page(dev: &Device, lid: u8, buf: &mut [u8]) -> io::Result<()> {
    assert!(buf.len() % 4 == 0 && !buf.is_empty());
    let f = File::open(&dev.path)?;
    let numd = (buf.len() / 4 - 1) as u32;
    let mut cmd = NvmePassthruCmd {
        opcode: 0x02,
        nsid: 0xFFFF_FFFF,
        addr: buf.as_mut_ptr() as u64,
        data_len: buf.len() as u32,
        cdw10: ((numd & 0xffff) << 16) | lid as u32,
        cdw11: numd >> 16,
        ..Default::default()
    };
    let r = unsafe { libc::ioctl(f.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, &mut cmd) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if r > 0 {
        return Err(io::Error::other(format!("NVMe status {r:#x}")));
    }
    Ok(())
}

pub fn read_health(dev: &Device) -> io::Result<[u8; LOG_PAGE_LEN]> {
    let mut b = [0u8; LOG_PAGE_LEN];
    get_log_page(dev, 0x02, &mut b)?;
    Ok(b)
}

/// Resolve a block device sysfs directory to its NVMe controller name.
fn nvme_controller(sys: &Path) -> Option<String> {
    let real = fs::canonicalize(sys).ok()?;
    // .../nvme/nvme0/nvme0n1[/nvme0n1p1] or .../nvme-subsystem/nvme-subsys0/nvme0n1
    for comp in real.components().rev() {
        let s = comp.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix("nvme") {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return Some(s.into_owned());
            }
        }
    }
    // Namespace under a subsystem: follow its `device` link to the controller.
    let mut dir = real.clone();
    if dir.join("partition").exists() {
        dir.pop();
    }
    let dev = fs::canonicalize(dir.join("device")).ok()?;
    let name = dev.file_name()?.to_string_lossy().into_owned();
    name.starts_with("nvme").then_some(name)
}

pub fn device_for_path(path: &Path) -> io::Result<Device> {
    let md = fs::metadata(path)?;
    let dev = md.dev();
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    let sys = PathBuf::from(format!("/sys/dev/block/{maj}:{min}"));
    if let Some(c) = nvme_controller(&sys) {
        return Ok(Device {
            path: format!("/dev/{c}"),
        });
    }
    // Device-mapper / md: look through slaves.
    let mut dir = fs::canonicalize(&sys)?;
    if dir.join("partition").exists() {
        dir.pop();
    }
    if let Ok(rd) = fs::read_dir(dir.join("slaves")) {
        for e in rd.flatten() {
            if let Some(c) = nvme_controller(&e.path()) {
                return Ok(Device {
                    path: format!("/dev/{c}"),
                });
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} is not on an NVMe device ({maj}:{min})", path.display()),
    ))
}
