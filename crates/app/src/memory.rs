//! Hands unused process memory back to the operating system.
//!
//! Dropping a message's decoded images and GPU textures frees them inside the process, but
//! Windows keeps those pages in the working set until it is trimmed. Asking for a trim lets the
//! reported memory fall once a message is closed.

#[cfg(windows)]
pub fn release_unused_memory() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

    // SAFETY: the current-process pseudo-handle is always valid. Passing `usize::MAX` for both
    // limits asks the OS to remove as many pages as possible from the working set.
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
pub fn release_unused_memory() {}
