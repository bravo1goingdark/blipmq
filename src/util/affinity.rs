// Platform/thread affinity utilities

#[cfg(all(windows, feature = "affinity"))]
pub fn set_current_thread_affinity(core_index: usize) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
    let width = core::mem::size_of::<usize>() * 8;
    let bit = core_index % width;
    let mask: usize = 1usize << bit;
    unsafe {
        let _ = SetThreadAffinityMask(GetCurrentThread(), mask);
    }
}

#[cfg(not(all(windows, feature = "affinity")))]
pub fn set_current_thread_affinity(_core_index: usize) {
    // no-op on non-Windows or when feature disabled
}
