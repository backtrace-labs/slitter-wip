//! This module services individual allocation and deallocation calls,
//! i.e., the majority of public calls into Slitter.
use std::ffi::c_void;
use std::ptr::NonNull;

use crate::cache;
use crate::class::Class;
use crate::class::ClassInfo;
use crate::linear_ref::LinearRef;
use crate::press;

impl Class {
    #[inline(always)]
    pub fn allocate(self) -> Option<NonNull<c_void>> {
        cache::allocate(self).map(|x| x.convert_to_non_null())
    }

    #[inline(always)]
    pub fn release(self, block: NonNull<c_void>) {
        press::check_allocation(self, block.as_ptr() as usize)
            .expect("deallocated address should match allocation class");
        cache::release(self, LinearRef::new(block));
    }
}

impl ClassInfo {
    #[inline(never)]
    pub(crate) fn allocate_slow(&self) -> Option<LinearRef> {
        self.press.allocate_one_object()
    }

    #[inline(never)]
    pub(crate) fn release_slow(&self, block: LinearRef) {
        let mut mag = self.allocate_non_full_magazine();

        // Deallocation must succeed.
        assert_eq!(mag.put(block), None);
        self.release_magazine(mag);
    }
}
