// SPDX-License-Identifier: MPL-2.0

#![allow(unused_variables)]

use core::{any::TypeId, marker::PhantomData};

use align_ext::AlignExt;
use inherit_methods_macro::inherit_methods;
use pod::Pod;

use crate::{
    arch::x86::mm::copy_with_recovery,
    mm::{
        kspace::{KERNEL_BASE_VADDR, KERNEL_END_VADDR},
        MAX_USERSPACE_VADDR,
    },
    prelude::*,
    Error,
};

/// A trait that enables reading/writing data from/to a VM object,
/// e.g., [`VmSpace`], [`FrameVec`], and [`Frame`].
///
/// # Concurrency
///
/// The methods may be executed by multiple concurrent reader and writer
/// threads. In this case, if the results of concurrent reads or writes
/// desire predictability or atomicity, the users should add extra mechanism
/// for such properties.
///
/// [`VmSpace`]: crate::mm::VmSpace
/// [`FrameVec`]: crate::mm::FrameVec
/// [`Frame`]: crate::mm::Frame
pub trait VmIo: Send + Sync {
    /// Reads a specified number of bytes at a specified offset into a given buffer.
    ///
    /// # No short reads
    ///
    /// On success, the output `buf` must be filled with the requested data
    /// completely. If, for any reason, the requested data is only partially
    /// available, then the method shall return an error.
    fn read_bytes(&self, offset: usize, buf: &mut [u8]) -> Result<()>;

    /// Reads a value of a specified type at a specified offset.
    fn read_val<T: Pod>(&self, offset: usize) -> Result<T> {
        let mut val = T::new_uninit();
        self.read_bytes(offset, val.as_bytes_mut())?;
        Ok(val)
    }

    /// Reads a slice of a specified type at a specified offset.
    ///
    /// # No short reads
    ///
    /// Similar to [`read_bytes`].
    ///
    /// [`read_bytes`]: VmIo::read_bytes
    fn read_slice<T: Pod>(&self, offset: usize, slice: &mut [T]) -> Result<()> {
        let len_in_bytes = core::mem::size_of_val(slice);
        let ptr = slice as *mut [T] as *mut u8;
        // SAFETY: the slice can be transmuted to a writable byte slice since the elements
        // are all Plain-Old-Data (Pod) types.
        let buf = unsafe { core::slice::from_raw_parts_mut(ptr, len_in_bytes) };
        self.read_bytes(offset, buf)
    }

    /// Writes a specified number of bytes from a given buffer at a specified offset.
    ///
    /// # No short writes
    ///
    /// On success, the input `buf` must be written to the VM object entirely.
    /// If, for any reason, the input data can only be written partially,
    /// then the method shall return an error.
    fn write_bytes(&self, offset: usize, buf: &[u8]) -> Result<()>;

    /// Writes a value of a specified type at a specified offset.
    fn write_val<T: Pod>(&self, offset: usize, new_val: &T) -> Result<()> {
        self.write_bytes(offset, new_val.as_bytes())?;
        Ok(())
    }

    /// Writes a slice of a specified type at a specified offset.
    ///
    /// # No short write
    ///
    /// Similar to [`write_bytes`].
    ///
    /// [`write_bytes`]: VmIo::write_bytes
    fn write_slice<T: Pod>(&self, offset: usize, slice: &[T]) -> Result<()> {
        let len_in_bytes = core::mem::size_of_val(slice);
        let ptr = slice as *const [T] as *const u8;
        // SAFETY: the slice can be transmuted to a readable byte slice since the elements
        // are all Plain-Old-Data (Pod) types.
        let buf = unsafe { core::slice::from_raw_parts(ptr, len_in_bytes) };
        self.write_bytes(offset, buf)
    }

    /// Writes a sequence of values given by an iterator (`iter`) from the specified offset (`offset`).
    ///
    /// The write process stops until the VM object does not have enough remaining space
    /// or the iterator returns `None`. If any value is written, the function returns `Ok(nr_written)`,
    /// where `nr_written` is the number of the written values.
    ///
    /// The offset of every value written by this method is aligned to the `align`-byte boundary.
    /// Naturally, when `align` equals to `0` or `1`, then the argument takes no effect:
    /// the values will be written in the most compact way.
    ///
    /// # Example
    ///
    /// Initializes an VM object with the same value can be done easily with `write_values`.
    ///
    /// ```
    /// use core::iter::self;
    ///
    /// let _nr_values = vm_obj.write_vals(0, iter::repeat(0_u32), 0).unwrap();
    /// ```
    ///
    /// # Panics
    ///
    /// This method panics if `align` is greater than two,
    /// but not a power of two, in release mode.
    fn write_vals<'a, T: Pod + 'a, I: Iterator<Item = &'a T>>(
        &self,
        offset: usize,
        iter: I,
        align: usize,
    ) -> Result<usize> {
        let mut nr_written = 0;

        let (mut offset, item_size) = if (align >> 1) == 0 {
            // align is 0 or 1
            (offset, core::mem::size_of::<T>())
        } else {
            // align is more than 2
            (
                offset.align_up(align),
                core::mem::size_of::<T>().align_up(align),
            )
        };

        for item in iter {
            match self.write_val(offset, item) {
                Ok(_) => {
                    offset += item_size;
                    nr_written += 1;
                }
                Err(e) => {
                    if nr_written > 0 {
                        return Ok(nr_written);
                    }
                    return Err(e);
                }
            }
        }

        Ok(nr_written)
    }
}

macro_rules! impl_vmio_pointer {
    ($typ:ty,$from:tt) => {
        #[inherit_methods(from = $from)]
        impl<T: VmIo> VmIo for $typ {
            fn read_bytes(&self, offset: usize, buf: &mut [u8]) -> Result<()>;
            fn read_val<F: Pod>(&self, offset: usize) -> Result<F>;
            fn read_slice<F: Pod>(&self, offset: usize, slice: &mut [F]) -> Result<()>;
            fn write_bytes(&self, offset: usize, buf: &[u8]) -> Result<()>;
            fn write_val<F: Pod>(&self, offset: usize, new_val: &F) -> Result<()>;
            fn write_slice<F: Pod>(&self, offset: usize, slice: &[F]) -> Result<()>;
        }
    };
}

impl_vmio_pointer!(&T, "(**self)");
impl_vmio_pointer!(&mut T, "(**self)");
impl_vmio_pointer!(Box<T>, "(**self)");
impl_vmio_pointer!(Arc<T>, "(**self)");

/// A marker structure used for [`VmReader`] and [`VmWriter`],
/// representing their operated memory scope is in user space.
pub struct UserSpace;

/// A marker structure used for [`VmReader`] and [`VmWriter`],
/// representing their operated memory scope is in kernel space.
pub struct KernelSpace;

/// Copies `len` bytes from `src` to `dst`.
/// Moves `src` and `dst` to the end of copied range.
///
/// Returns the number of successfully copied bytes.
///
/// # Safety
///
/// The region of memory [`src`..`src` + len] and [`dst`..`dst` + len] must be valid,
/// and should _not_ overlap if one of them represent typed memory.
///
/// Operation on typed memory may be safe only if it is plain-old-data. Otherwise
/// the safety requirements of [`core::ptr::copy`] should also be considered.
unsafe fn copy_valid_ensured(src: &mut *const u8, dst: &mut *mut u8, len: usize) -> usize {
    core::ptr::copy(*src, *dst, len);
    *src = src.add(len);
    *dst = dst.add(len);

    len
}

/// Copies `len` bytes from `src` to `dst`.
/// Moves `src` and `dst` to the end of copied range.
/// This function will early stop copying if encountering an unresolvable page fault.
///
/// Returns the number of successfully copied bytes.
///
/// # Safety
///
/// This method should only be used when one of [`src`..`src` + len] and [`dst`..`dst` + len]
/// is in user space, and the other memory is in kernel space and is ensured to be valid.
/// In addition, users should ensure this function only be invoked when a suitable page table
/// is activated.
///
/// The actual physical address of memory [`src`..`src` + len] and [`dst`..`dst` + len] should
/// _not_ overlap if the kernel space memory represent typed memory.
unsafe fn copy_valid_unchecked(src: &mut *const u8, dst: &mut *mut u8, len: usize) -> usize {
    let failed_bytes = copy_with_recovery(*dst, *src, len);
    let copied_len = len - failed_bytes;
    *src = src.add(copied_len);
    *dst = dst.add(copied_len);

    copied_len
}

/// `VmReader` is a reader for reading data from a contiguous range of memory.
///
/// The memory range read by `VmReader` can be in either kernel space or user space.
/// When the operating range is in kernel space, the memory within that range
/// is guaranteed to be valid.
/// When the operating range is in user space, it is ensured that the page table of
/// the process creating the `VmReader` is active for the duration of `'a`.
///
/// When perform reading with a `VmWriter`, if one of them represents typed memory,
/// it can ensure that the reading range in this reader and writing range in the
/// writer are not overlapped.
///
/// NOTE: The overlap mentioned above is at both the virtual address level
/// and physical address level. There is not guarantee for the operation results
/// of `VmReader` and `VmWriter` in overlapping untyped addresses, and it is
/// the user's responsibility to handle this situation.
pub struct VmReader<'a, Target: 'static = KernelSpace> {
    cursor: *const u8,
    end: *const u8,
    phantom: PhantomData<(&'a [u8], Target)>,
}

impl<'a> VmReader<'a, KernelSpace> {
    /// Constructs a `VmReader` from a pointer and a length, which represents
    /// a memory range in kernel space.
    ///
    /// # Safety
    ///
    /// Users must ensure the memory from `ptr` to `ptr.add(len)` is contiguous.
    /// Users must ensure the memory is valid during the entire period of `'a`.
    /// Users must ensure the memory should _not_ overlap with other `VmWriter`s
    /// with typed memory, and if the memory range in this `VmReader` is typed,
    /// it should _not_ overlap with other `VmWriter`s.
    ///
    /// The user space memory is treated as untyped.
    pub unsafe fn from_kernel_space(ptr: *const u8, len: usize) -> Self {
        debug_assert!(KERNEL_BASE_VADDR <= ptr as usize);
        debug_assert!(ptr.add(len) as usize <= KERNEL_END_VADDR);

        Self {
            cursor: ptr,
            end: ptr.add(len),
            phantom: PhantomData,
        }
    }
}

impl<'a> VmReader<'a, UserSpace> {
    /// Constructs a `VmReader` from a pointer and a length, which represents
    /// a memory range in kernel space.
    ///
    /// # Safety
    ///
    /// Users must ensure the memory from `ptr` to `ptr.add(len)` is contiguous.
    /// Users must ensure that the page table for the process in which this constructor is called
    /// are active during the entire period of `'a`.
    pub unsafe fn from_user_space(ptr: *const u8, len: usize) -> Self {
        debug_assert!((ptr as usize).checked_add(len).unwrap_or(usize::MAX) <= MAX_USERSPACE_VADDR);

        Self {
            cursor: ptr,
            end: ptr.add(len),
            phantom: PhantomData,
        }
    }
}

impl<'a, Target: 'static> VmReader<'a, Target> {
    /// Returns the number of bytes for the remaining data.
    pub const fn remain(&self) -> usize {
        // SAFETY: the end is equal to or greater than the cursor.
        unsafe { self.end.sub_ptr(self.cursor) }
    }

    /// Returns the cursor pointer, which refers to the address of the next byte to read.
    pub const fn cursor(&self) -> *const u8 {
        self.cursor
    }

    /// Returns if it has remaining data to read.
    pub const fn has_remain(&self) -> bool {
        self.remain() > 0
    }

    /// Limits the length of remaining data.
    ///
    /// This method ensures the post condition of `self.remain() <= max_remain`.
    pub const fn limit(mut self, max_remain: usize) -> Self {
        if max_remain < self.remain() {
            // SAFETY: the new end is less than the old end.
            unsafe { self.end = self.cursor.add(max_remain) };
        }
        self
    }

    /// Skips the first `nbytes` bytes of data.
    /// The length of remaining data is decreased accordingly.
    ///
    /// # Panic
    ///
    /// If `nbytes` is greater than `self.remain()`, then the method panics.
    pub fn skip(mut self, nbytes: usize) -> Self {
        assert!(nbytes <= self.remain());

        // SAFETY: the new cursor is less than or equal to the end.
        unsafe { self.cursor = self.cursor.add(nbytes) };
        self
    }

    /// Reads data from this reader into the writer until one of the three conditions is met:
    /// 1. The reader has no remaining data.
    /// 2. The writer has no available space.
    /// 3. The reading encounters a page fault that cannot be handled.
    ///
    /// Returns the number of bytes read.
    ///
    /// It pulls the number of bytes data from the reader and
    /// fills in the writer with the number of bytes.
    ///
    /// The reading instruction is forbidden if the targets of reader and writer
    /// are both user space.
    pub fn read<W: 'static>(&mut self, writer: &mut VmWriter<'_, W>) -> usize {
        let copy_len = self.remain().min(writer.avail());
        if copy_len == 0 {
            return 0;
        }

        if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            && TypeId::of::<W>() == TypeId::of::<UserSpace>()
        {
            // The target of this reader and the input writer are both user space.
            panic!("copy from user to user is forbidden");
        } else if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            || TypeId::of::<W>() == TypeId::of::<UserSpace>()
        {
            // One of the target is user space and the other is kernel space.
            //
            // SAFETY: The the corresponding page table of the user space memory is
            // guaranteed to be activated due to its construction requirement.
            // The kernel space memory range will be valid since `copy_len` is the minimum
            // of the reader's remaining data and the writer's available space, and will
            // not overlap with user space memory range in physical address level if it
            // represents typed memory.
            unsafe { copy_valid_unchecked(&mut self.cursor, &mut writer.cursor, copy_len) }
        } else {
            // The target of this reader and the input writer are both kernel space.
            //
            // SAFETY: the reading memory range and writing memory range will be valid
            // since `copy_len` is the minimum of the reader's remaining data and the
            // writer's available space, and will not overlap if one of them represents
            // typed memory.
            unsafe { copy_valid_ensured(&mut self.cursor, &mut writer.cursor, copy_len) }
        }
    }

    /// Reads all data from this reader into the writer until one of the three conditions is met:
    /// 1. The reader has no remaining data.
    /// 2. The writer has no available space.
    /// 3. The reading encounters a page fault that cannot be handled.
    ///
    /// It pulls the number of bytes data from the reader and
    /// fills in the writer with the number of bytes.
    ///
    /// If the reading stops due to the condition 1 or 2, this method is considered successful
    /// and returns the number of bytes read.
    /// If the reading stops due to the condition 3, which means the reader have remaining data
    /// and the writer also has available space, the reading is considered failed and return `Err`.
    ///
    /// The reading instruction is forbidden if the targets of reader and writer
    /// are both user space.
    pub fn read_all<W: 'static>(&mut self, writer: &mut VmWriter<'_, W>) -> Result<usize> {
        let copy_len = self.remain().min(writer.avail());
        if copy_len == 0 {
            return Ok(0);
        }

        if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            && TypeId::of::<W>() == TypeId::of::<UserSpace>()
        {
            // The target of this reader and the input writer are both user space.
            panic!("copy from user to user is forbidden");
        } else if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            || TypeId::of::<W>() == TypeId::of::<UserSpace>()
        {
            // One of the target is user space and the other is kernel space.
            //
            // SAFETY:
            // Meets the same safety requirements as `VmReader::read`.
            let copied_len =
                unsafe { copy_valid_unchecked(&mut self.cursor, &mut writer.cursor, copy_len) };
            if copied_len < copy_len {
                Err(Error::PageFault)
            } else {
                Ok(copied_len)
            }
        } else {
            // The target of this reader and the input writer are both kernel space.
            //
            // SAFETY:
            // Meets the same safety requirements as `VmReader::read`.
            unsafe {
                Ok(copy_valid_ensured(
                    &mut self.cursor,
                    &mut writer.cursor,
                    copy_len,
                ))
            }
        }
    }

    /// Reads a value of `Pod` type.
    ///
    /// If the length of the `Pod` type exceeds `self.remain()`,
    /// or the value can not be read completely,
    /// this method will return `Err`.
    pub fn read_val<T: Pod>(&mut self) -> Result<T> {
        if self.remain() < core::mem::size_of::<T>() {
            return Err(Error::PageFault);
        }

        let mut val = T::new_uninit();
        let mut writer = VmWriter::from(val.as_bytes_mut());
        self.read_all(&mut writer).map(|_| val)
    }
}

impl<'a> From<&'a [u8]> for VmReader<'a> {
    fn from(slice: &'a [u8]) -> Self {
        // SAFETY: the range of memory is contiguous and is valid during `'a`,
        // and will not overlap with other `VmWriter` since the slice already has
        // an immutable reference. The slice will not be mapped to the user space hence
        // it will also not overlap with `VmWriter` generated from user space.
        unsafe { Self::from_kernel_space(slice.as_ptr(), slice.len()) }
    }
}

/// `VmWriter` is a writer for writing data to a contiguous range of memory.
///
/// The memory range write by `VmWriter` can be in either kernel space or user space.
/// When the operating range is in kernel space, the memory within that range
/// is guaranteed to be valid.
/// When the operating range is in user space, it is ensured that the page table of
/// the process creating the `VmWriter` is active for the duration of `'a`.
///
/// When perform writing with a `VmReader`, if one of them represents typed memory,
/// it can ensure that the writing range in this writer and reading range in the
/// reader are not overlapped.
///
/// NOTE: The overlap mentioned above is at both the virtual address level
/// and physical address level. There is not guarantee for the operation results
/// of `VmReader` and `VmWriter` in overlapping untyped addresses, and it is
/// the user's responsibility to handle this situation.
pub struct VmWriter<'a, Target: 'static = KernelSpace> {
    cursor: *mut u8,
    end: *mut u8,
    phantom: PhantomData<(&'a mut [u8], Target)>,
}

impl<'a> VmWriter<'a, KernelSpace> {
    /// Constructs a `VmWriter` from a pointer and a length, which represents
    /// a memory range in kernel space.
    ///
    /// # Safety
    ///
    /// Users must ensure the memory from `ptr` to `ptr.add(len)` is contiguous.
    /// Users must ensure the memory is valid during the entire period of `'a`.
    /// Users must ensure the memory should _not_ overlap with other `VmReader`s
    /// with typed memory, and if the memory range in this `VmReader` is typed,
    /// it should _not_ overlap with other `VmReader`s.
    ///
    /// The user space memory is treated as untyped.
    pub unsafe fn from_kernel_space(ptr: *mut u8, len: usize) -> Self {
        debug_assert!(KERNEL_BASE_VADDR <= ptr as usize);
        debug_assert!(ptr.add(len) as usize <= KERNEL_END_VADDR);

        Self {
            cursor: ptr,
            end: ptr.add(len),
            phantom: PhantomData,
        }
    }

    /// Fills the available space by repeating `value`.
    ///
    /// Returns the number of values written.
    ///
    /// # Panic
    ///
    /// The size of the available space must be a multiple of the size of `value`.
    /// Otherwise, the method would panic.
    pub fn fill<T: Pod>(&mut self, value: T) -> usize {
        let avail = self.avail();

        assert!((self.cursor as *mut T).is_aligned());
        assert!(avail % core::mem::size_of::<T>() == 0);

        let written_num = avail / core::mem::size_of::<T>();

        for i in 0..written_num {
            // SAFETY: `written_num` is calculated by the avail size and the size of the type `T`,
            // hence the `add` operation and `write` operation are valid and will only manipulate
            // the memory managed by this writer.
            unsafe {
                (self.cursor as *mut T).add(i).write(value);
            }
        }

        // The available space has been filled so this cursor can be moved to the end.
        self.cursor = self.end;
        written_num
    }
}

impl<'a> VmWriter<'a, UserSpace> {
    /// Constructs a `VmWriter` from a pointer and a length, which represents
    /// a memory range in kernel space.
    ///
    /// # Safety
    ///
    /// Users must ensure the memory from `ptr` to `ptr.add(len)` is contiguous.
    /// Users must ensure that the page table for the process in which this constructor is called
    /// are active during the entire period of `'a`.
    pub unsafe fn from_user_space(ptr: *mut u8, len: usize) -> Self {
        debug_assert!((ptr as usize).checked_add(len).unwrap_or(usize::MAX) <= MAX_USERSPACE_VADDR);

        Self {
            cursor: ptr,
            end: ptr.add(len),
            phantom: PhantomData,
        }
    }
}

impl<'a, Target: 'static> VmWriter<'a, Target> {
    /// Returns the number of bytes for the available space.
    pub const fn avail(&self) -> usize {
        // SAFETY: the end is equal to or greater than the cursor.
        unsafe { self.end.sub_ptr(self.cursor) }
    }

    /// Returns the cursor pointer, which refers to the address of the next byte to write.
    pub const fn cursor(&self) -> *mut u8 {
        self.cursor
    }

    /// Returns if it has available space to write.
    pub const fn has_avail(&self) -> bool {
        self.avail() > 0
    }

    /// Limits the length of available space.
    ///
    /// This method ensures the post condition of `self.avail() <= max_avail`.
    pub const fn limit(mut self, max_avail: usize) -> Self {
        if max_avail < self.avail() {
            // SAFETY: the new end is less than the old end.
            unsafe { self.end = self.cursor.add(max_avail) };
        }
        self
    }

    /// Skips the first `nbytes` bytes of data.
    /// The length of available space is decreased accordingly.
    ///
    /// # Panic
    ///
    /// If `nbytes` is greater than `self.avail()`, then the method panics.
    pub fn skip(mut self, nbytes: usize) -> Self {
        assert!(nbytes <= self.avail());

        // SAFETY: the new cursor is less than or equal to the end.
        unsafe { self.cursor = self.cursor.add(nbytes) };
        self
    }

    /// Writes data from the reader into this writer until one of the three conditions is met:
    /// 1. The writer has no available space.
    /// 2. The reader has no remaining data.
    /// 3. The writing encounters a page fault that cannot be handled.
    ///
    /// Returns the number of bytes written.
    ///
    /// It pulls the number of bytes data from the reader and
    /// fills in the writer with the number of bytes.
    ///
    /// The writing instruction is forbidden if the targets of reader and writer
    /// are both user space.
    pub fn write<R: 'static>(&mut self, reader: &mut VmReader<'_, R>) -> usize {
        let copy_len = self.avail().min(reader.remain());
        if copy_len == 0 {
            return 0;
        }

        if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            && TypeId::of::<R>() == TypeId::of::<UserSpace>()
        {
            // The target of this writer and the input reader are both user space.
            panic!("copy from user to user is forbidden");
        } else if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            || TypeId::of::<R>() == TypeId::of::<UserSpace>()
        {
            // One of the target is user space and the other is kernel space.
            //
            // SAFETY: The the corresponding page table of the user space memory is
            // guaranteed to be activated due to its construction requirement.
            // The kernel space memory range will be valid since `copy_len` is the minimum
            // of the reader's remaining data and the writer's available space, and will
            // not overlap with user space memory range in physical address level if it
            // represents typed memory.
            unsafe { copy_valid_unchecked(&mut reader.cursor, &mut self.cursor, copy_len) }
        } else {
            // The target of this writer and the input reader are both kernel space.
            //
            // SAFETY: the reading memory range and writing memory range will be valid
            // since `copy_len` is the minimum of the reader's remaining data and the
            // writer's available space, and will not overlap if one of them represents
            // typed memory.
            unsafe { copy_valid_ensured(&mut reader.cursor, &mut self.cursor, copy_len) }
        }
    }

    /// Writes all data from the reader into this writer until one of the three conditions is met:
    /// 1. The writer has no available space.
    /// 2. The reader has no remaining data.
    /// 3. The writing encounters a page fault that cannot be handled.
    ///
    /// Returns the number of bytes written.
    ///
    /// It pulls the number of bytes data from the reader and
    /// fills in the writer with the number of bytes.
    ///
    /// If the writing stops due to the condition 1 or 2, this method is considered successful
    /// and returns the number of bytes written.
    /// If the writing stops due to the condition 3, which means the reader have remaining data
    /// and the writer also has available space, the writing is considered failed and return `Err`.
    ///
    /// The writing instruction is forbidden if the targets of reader and writer
    /// are both user space.
    pub fn write_all<R: 'static>(&mut self, reader: &mut VmReader<'_, R>) -> Result<usize> {
        let copy_len = self.avail().min(reader.remain());
        if copy_len == 0 {
            return Ok(0);
        }

        if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            && TypeId::of::<R>() == TypeId::of::<UserSpace>()
        {
            // The target of this writer and the input reader are both user space.
            panic!("copy from user to user is forbidden");
        } else if TypeId::of::<Target>() == TypeId::of::<UserSpace>()
            || TypeId::of::<R>() == TypeId::of::<UserSpace>()
        {
            // One of the target is user space and the other is kernel space.
            //
            // SAFETY:
            // Meets the same safety requirements as `VmWriter::write`.
            let copied_len =
                unsafe { copy_valid_unchecked(&mut reader.cursor, &mut self.cursor, copy_len) };
            if copied_len < copy_len {
                Err(Error::PageFault)
            } else {
                Ok(copied_len)
            }
        } else {
            // The target of this writer and the input reader are both kernel space.
            //
            // SAFETY:
            // Meets the same safety requirements as `VmWriter::write`.
            unsafe {
                Ok(copy_valid_ensured(
                    &mut reader.cursor,
                    &mut self.cursor,
                    copy_len,
                ))
            }
        }
    }

    /// Writes a value of `Pod` type.
    ///
    /// If the length of the `Pod` type exceeds `self.avail()`,
    /// or the value can not be write completely, this method will return `Err`.
    pub fn write_val<T: Pod>(&mut self, new_val: &T) -> Result<()> {
        if self.avail() < core::mem::size_of::<T>() {
            return Err(Error::PageFault);
        }

        let mut reader = VmReader::from(new_val.as_bytes());
        self.write_all(&mut reader)?;
        Ok(())
    }
}

impl<'a> From<&'a mut [u8]> for VmWriter<'a> {
    fn from(slice: &'a mut [u8]) -> Self {
        // SAFETY: the range of memory is contiguous and is valid during `'a`, and
        // will not overlap with other `VmWriter` since the slice already has
        // an mutable reference. The slice will not be mapped to the user space hence
        // it will also not overlap with `VmWriter` generated from user space.
        unsafe { Self::from_kernel_space(slice.as_mut_ptr(), slice.len()) }
    }
}
