//! SQLite on Windows: the `winsqlite3.dll` that ships with Windows 10 and later, loaded from
//! System32 only (a DLL planted next to the executable is never picked up).

use std::ffi::{CStr, c_void};
use std::ptr::null_mut;
use std::sync::Arc;

use daemon::sqlite::SqliteLibrary;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

pub struct Winsqlite3 {
    module: HMODULE,
}

// SAFETY: a module handle is a process-wide value; the library is never unloaded.
unsafe impl Send for Winsqlite3 {}
unsafe impl Sync for Winsqlite3 {}

impl Winsqlite3 {
    pub fn load() -> Result<Arc<dyn SqliteLibrary>, String> {
        let name: Vec<u16> = "winsqlite3.dll\0".encode_utf16().collect();
        // SAFETY: `name` is NUL-terminated.
        let module = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err(format!("cannot load winsqlite3.dll from System32: {}", std::io::Error::last_os_error()));
        }
        Ok(Arc::new(Winsqlite3 { module }))
    }
}

impl SqliteLibrary for Winsqlite3 {
    fn symbol(&self, name: &CStr) -> *const c_void {
        // SAFETY: the module stays loaded; `name` is NUL-terminated.
        match unsafe { GetProcAddress(self.module, name.as_ptr().cast()) } {
            Some(f) => f as *const c_void,
            None => std::ptr::null(),
        }
    }

    fn describe(&self) -> String {
        "winsqlite3.dll".into()
    }
}
