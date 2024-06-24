// SPDX-License-Identifier: MPL-2.0

core::arch::global_asm!(include_str!("copy_with_recovery.S"));

extern "C" {
    /// Copies `size` bytes from `src` to `dst`. This function is work with exception handling
    /// and can recover from page fault. The source and destination must not overlap.
    ///
    /// Returns number of bytes that failed to copy.
    pub(crate) fn copy_with_recovery(dst: *mut u8, src: *const u8, size: usize) -> usize;
}
