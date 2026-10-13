// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! macOS: IOKit NVMe SMART user-client plug-in (as used by smartmontools'
//! `os_darwin.cpp`). Only Identify and Get Log Page are exposed by Apple.

use std::ffi::{c_char, c_void, CStr, CString};
use std::io;
use std::path::Path;
use std::ptr;

use super::{Device, LOG_PAGE_LEN};

type IoObject = u32;
type KernReturn = i32;
type CFTypeRef = *const c_void;
type CFUUIDRef = *const c_void;
type CFAllocatorRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes([u8; 16]);

#[repr(C)]
struct IOCFPlugInInterface {
    _reserved: *mut c_void,
    query_interface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "C" fn(*mut c_void) -> u32,
    release: unsafe extern "C" fn(*mut c_void) -> u32,
    // version, revision, Probe, Start, Stop follow; unused.
}

#[repr(C)]
struct IONVMeSMARTInterface {
    _reserved: *mut c_void,
    query_interface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "C" fn(*mut c_void) -> u32,
    release: unsafe extern "C" fn(*mut c_void) -> u32,
    version: u16,
    revision: u16,
    smart_read_data: unsafe extern "C" fn(*mut c_void, *mut u8) -> KernReturn,
    get_identify_data: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> KernReturn,
    _reserved0: u64,
    _reserved1: u64,
    get_log_page: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32) -> KernReturn,
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOBSDNameMatching(main_port: u32, options: u32, name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> IoObject;
    fn IORegistryEntryGetParentEntry(
        entry: IoObject,
        plane: *const c_char,
        parent: *mut IoObject,
    ) -> KernReturn;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFTypeRef,
        alloc: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    fn IOObjectRelease(o: IoObject) -> KernReturn;
    fn IOCreatePlugInInterfaceForService(
        service: IoObject,
        plugin_type: CFUUIDRef,
        interface_type: CFUUIDRef,
        out: *mut *mut *mut IOCFPlugInInterface,
        score: *mut i32,
    ) -> KernReturn;
    fn IODestroyPlugInInterface(i: *mut *mut IOCFPlugInInterface) -> KernReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, s: *const c_char, enc: u32) -> CFTypeRef;
    fn CFRelease(t: CFTypeRef);
    fn CFUUIDGetConstantUUIDWithBytes(
        alloc: CFAllocatorRef,
        b0: u8,
        b1: u8,
        b2: u8,
        b3: u8,
        b4: u8,
        b5: u8,
        b6: u8,
        b7: u8,
        b8: u8,
        b9: u8,
        b10: u8,
        b11: u8,
        b12: u8,
        b13: u8,
        b14: u8,
        b15: u8,
    ) -> CFUUIDRef;
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

const SMART_USER_CLIENT_TYPE: [u8; 16] = [
    0xAA, 0x0F, 0xA6, 0xF9, 0xC2, 0xD6, 0x45, 0x7F, 0xB1, 0x0B, 0x59, 0xA1, 0x32, 0x53, 0x29, 0x2F,
];
const SMART_INTERFACE: [u8; 16] = [
    0xCC, 0xD1, 0xDB, 0x19, 0xFD, 0x9A, 0x4D, 0xAF, 0xBF, 0x95, 0x12, 0x45, 0x4B, 0x23, 0x0A, 0xB6,
];
const CF_PLUGIN_INTERFACE: [u8; 16] = [
    0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F,
];

unsafe fn uuid(b: [u8; 16]) -> CFUUIDRef {
    CFUUIDGetConstantUUIDWithBytes(
        ptr::null(),
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15],
    )
}

/// Walk up the IOService plane until a node advertises "NVMe SMART Capable".
unsafe fn smart_node(bsd: &str) -> io::Result<IoObject> {
    let name = CString::new(bsd).unwrap();
    let mut entry = IOServiceGetMatchingService(0, IOBSDNameMatching(0, 0, name.as_ptr()));
    if entry == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no IOKit entry for {bsd}"),
        ));
    }
    let key = CFStringCreateWithCString(
        ptr::null(),
        c"NVMe SMART Capable".as_ptr(),
        K_CF_STRING_ENCODING_UTF8,
    );
    let plane = c"IOService";
    loop {
        let prop = IORegistryEntryCreateCFProperty(entry, key, ptr::null(), 0);
        if !prop.is_null() {
            CFRelease(prop);
            CFRelease(key);
            return Ok(entry);
        }
        let mut parent = 0;
        let kr = IORegistryEntryGetParentEntry(entry, plane.as_ptr(), &mut parent);
        IOObjectRelease(entry);
        if kr != 0 || parent == 0 {
            CFRelease(key);
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "device is not NVMe SMART capable",
            ));
        }
        entry = parent;
    }
}

pub fn get_log_page(dev: &Device, lid: u8, out: &mut [u8]) -> io::Result<()> {
    unsafe {
        let node = smart_node(&dev.path)?;
        let mut plugin: *mut *mut IOCFPlugInInterface = ptr::null_mut();
        let mut score = 0;
        let kr = IOCreatePlugInInterfaceForService(
            node,
            uuid(SMART_USER_CLIENT_TYPE),
            uuid(CF_PLUGIN_INTERFACE),
            &mut plugin,
            &mut score,
        );
        IOObjectRelease(node);
        if kr != 0 || plugin.is_null() {
            return Err(io::Error::other(format!(
                "IOCreatePlugInInterfaceForService failed: {kr:#x}"
            )));
        }
        let mut smart: *mut *mut IONVMeSMARTInterface = ptr::null_mut();
        let hr = ((**plugin).query_interface)(
            plugin.cast(),
            CFUUIDBytes(SMART_INTERFACE),
            (&mut smart as *mut *mut *mut IONVMeSMARTInterface).cast(),
        );
        if hr != 0 || smart.is_null() {
            IODestroyPlugInInterface(plugin);
            return Err(io::Error::other(format!("QueryInterface failed: {hr:#x}")));
        }
        let numd = (out.len() / 4 - 1) as u32;
        let kr = ((**smart).get_log_page)(smart.cast(), out.as_mut_ptr().cast(), lid as u32, numd);
        ((**smart).release)(smart.cast());
        IODestroyPlugInInterface(plugin);
        if kr != 0 {
            return Err(io::Error::other(format!("GetLogPage failed: {kr:#x}")));
        }
        Ok(())
    }
}

pub fn read_health(dev: &Device) -> io::Result<[u8; LOG_PAGE_LEN]> {
    let mut b = [0u8; LOG_PAGE_LEN];
    get_log_page(dev, 0x02, &mut b)?;
    Ok(b)
}

pub fn device_for_path(path: &Path) -> io::Result<Device> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let from = unsafe { CStr::from_ptr(st.f_mntfromname.as_ptr()) }.to_string_lossy();
    let bsd = from.strip_prefix("/dev/").unwrap_or(&from).to_string();
    Ok(Device { path: bsd })
}
