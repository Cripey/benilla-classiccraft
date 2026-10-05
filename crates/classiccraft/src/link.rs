//! Where the bridge's shared memory and the per-user data live, per platform (2026-10-04, Windows
//! groundwork). The Fabric mod mirrors this in `McwowShm` / `McwowPaths`; keep both in step.
//!
//! - Shared memory: on Linux a file in `/dev/shm` (RAM-backed), elsewhere on Unix one in `/tmp`;
//!   on Windows a named, pagefile-backed mapping `Local\<name>` (a file there would be written
//!   back to disk all the time). `CLASSICCRAFT_SHM_DIR` overrides the Unix directory.
//! - Data (music, dances, the weapon resource pack): `CLASSICCRAFT_DATA_DIR`, else
//!   `%APPDATA%\classiccraft` on Windows, `$XDG_DATA_HOME/classiccraft` or
//!   `~/.local/share/classiccraft` elsewhere.

use std::path::PathBuf;

/// The per-user data directory (not created here).
#[allow(dead_code)] // each binary uses its own part of this module
pub fn data_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLASSICCRAFT_DATA_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("classiccraft"));
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("classiccraft"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/classiccraft"))
}

/// How a link is shown in logs: its file path, or its mapping name on Windows.
#[allow(dead_code)]
pub fn describe(name: &str) -> String {
    if cfg!(windows) { format!("Local\\{name}") } else { shm_dir().join(name).display().to_string() }
}

#[allow(dead_code)]
fn shm_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLASSICCRAFT_SHM_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(if cfg!(target_os = "linux") { "/dev/shm" } else { "/tmp" })
}

/// A mapped link, at least the size it was opened with; derefs to its bytes.
#[allow(dead_code)]
pub struct SharedMap(imp::Map);

impl std::ops::Deref for SharedMap {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for SharedMap {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

/// Map the link `name` (e.g. `classiccraft_v1.shm`). `create`: make it if missing and grow it to
/// `size` (never shrunk: the other side may hold it mapped). Without `create` a missing link or one
/// smaller than `size` is an error (its creator hasn't set it up yet).
#[allow(dead_code)]
pub fn open(name: &str, size: usize, create: bool) -> std::io::Result<SharedMap> {
    imp::open(name, size, create).map(SharedMap)
}

#[cfg(unix)]
mod imp {
    use std::fs::OpenOptions;
    use std::io::{Error, ErrorKind};

    pub type Map = memmap2::MmapMut;

    pub fn open(name: &str, size: usize, create: bool) -> std::io::Result<Map> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(super::shm_dir().join(name))?;
        let len = file.metadata()?.len();
        if len < size as u64 {
            if !create {
                return Err(Error::new(ErrorKind::NotFound, "link not set up yet"));
            }
            file.set_len(size as u64)?;
        }
        // SAFETY: the link is shared on purpose; callers access it within the protocol's fixed
        // layout, and concurrent writers are the protocol's (seqlocks, rings) concern.
        unsafe { memmap2::MmapMut::map_mut(&file) }
    }
}

#[cfg(windows)]
mod imp {
    use std::io::{Error, ErrorKind};
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
    };

    pub struct Map {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        len: usize,
    }

    // SAFETY: the view is plain shared memory owned by this value.
    unsafe impl Send for Map {}
    unsafe impl Sync for Map {}

    impl std::ops::Deref for Map {
        type Target = [u8];
        fn deref(&self) -> &[u8] {
            // SAFETY: `view` maps `len` bytes for as long as `self` lives.
            unsafe { std::slice::from_raw_parts(self.view.Value.cast::<u8>(), self.len) }
        }
    }

    impl std::ops::DerefMut for Map {
        fn deref_mut(&mut self) -> &mut [u8] {
            // SAFETY: as above, and the mapping is read-write.
            unsafe { std::slice::from_raw_parts_mut(self.view.Value.cast::<u8>(), self.len) }
        }
    }

    impl Drop for Map {
        fn drop(&mut self) {
            // SAFETY: both were returned by the calls in `open` and are released once.
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }

    pub fn open(name: &str, size: usize, create: bool) -> std::io::Result<Map> {
        let wide: Vec<u16> = std::ffi::OsStr::new(&format!("Local\\{name}"))
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: plain Win32 calls on a NUL-terminated name; every handle/view is checked and
        // owned by the returned `Map` (or released on the error paths).
        unsafe {
            let handle = if create {
                // An existing mapping of that name is opened instead (at its own size).
                let size = size as u64;
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    (size >> 32) as u32,
                    size as u32,
                    wide.as_ptr(),
                )
            } else {
                OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr())
            };
            if handle.is_null() {
                return Err(Error::last_os_error());
            }
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if view.Value.is_null() {
                let e = Error::last_os_error();
                CloseHandle(handle);
                return Err(e);
            }
            let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            let len = if VirtualQuery(view.Value, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) == 0 {
                0
            } else {
                info.RegionSize
            };
            let map = Map { handle, view, len };
            if len < size {
                return Err(Error::new(ErrorKind::InvalidData, "link smaller than expected"));
            }
            Ok(map)
        }
    }
}
