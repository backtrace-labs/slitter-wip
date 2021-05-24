//! A Slitter allocation class represent a set of type-stable objects
//! that all have the same size (Slitter never overwrites an
//! allocation with internal metadata, even once freed).  Allocation
//! and deallocation calls must have matching `Class` structs, even if
//! objects from different classes have the same size: the Slitter
//! code may check this invariant to help detect bugs, and callers may
//! rely on type stability.
#[cfg(any(
    all(test, feature = "check_contracts_in_tests"),
    feature = "check_contracts"
))]
use contracts::*;
#[cfg(not(any(
    all(test, feature = "check_contracts_in_tests"),
    feature = "check_contracts"
)))]
use disabled_contracts::*;

use std::alloc::Layout;
use std::ffi::CStr;
use std::num::NonZeroU32;
use std::os::raw::c_char;

use crate::magazine::MagazineStack;
use crate::press::Press;

/// External callers interact with slitter allocation classes via this
/// opaque Class struct.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Class {
    id: NonZeroU32,
}

/// When created, a class is configured with an object size, and an
/// optional name.
pub struct ClassConfig {
    pub name: Option<String>,
    pub layout: Layout,
}

/// The extern "C" interface uses this version of `ClassConfig`.
#[repr(C)]
pub struct ForeignClassConfig {
    name: *const c_char,
    size: usize,
}

/// Slitter stores internal information about configured classes with
/// this Info struct.
pub(crate) struct ClassInfo {
    pub name: Option<String>,
    pub layout: Layout,

    // The Class will allocate and release magazines via this Rack.
    pub rack: &'static crate::magazine::Rack,

    // Fully populated magazines go in in `full_mags`.
    pub full_mags: MagazineStack,

    // Partially populated, but non-empty, magazines go in `partial_mags`.
    // Empty magazines go back to the `Rack`.
    pub partial_mags: MagazineStack,

    // Use this `Press` to allocate new objects.
    pub press: Press,

    pub id: Class,
}

impl ClassConfig {
    /// Attempts to convert a `ForeignClassConfig` pointer to a native
    /// `ClassConfig`.
    ///
    /// # Safety
    ///
    /// This function assumes `config_ptr` is NULL or valid.
    pub unsafe fn from_c(config_ptr: *const ForeignClassConfig) -> Option<ClassConfig> {
        if config_ptr.is_null() {
            return None;
        }

        let config: &ForeignClassConfig = &*config_ptr;
        let name = if config.name.is_null() {
            None
        } else {
            Some(CStr::from_ptr(config.name).to_str().ok()?.to_owned())
        };

        let layout = Layout::from_size_align(config.size.max(1), /*align=*/ 8).ok()?;
        Some(ClassConfig { name, layout })
    }
}

lazy_static::lazy_static! {
    // TODO(lock): this lock is never taken on a fast path.
    static ref CLASSES: std::sync::Mutex<Vec<&'static ClassInfo>> = Default::default();
}

pub fn max_id() -> usize {
    let guard = CLASSES.lock().unwrap();
    guard.len()
}

impl Class {
    /// Attempts to create a new allocation class for `config`.
    ///
    /// On success, there is a corresponding `ClassInfo` struct at
    /// `ret - 1` in the global `CLASSES` array.
    #[ensures(ret.is_ok() ->
	      CLASSES.lock().unwrap().get(ret.unwrap().id.get() as usize - 1).map(|info| info.id) == Some(ret.unwrap()),
	      "On success, the class is at id - 1 in the global array of ClassInfo")]
    #[ensures(ret.is_ok() ->
	      Class::from_id(ret.unwrap().id) == Some(ret.unwrap()),
	      "On success, we can instantiate `Class` from the NonZeroU32 id.")]
    #[ensures(ret.is_ok() ->
	      std::ptr::eq(ret.unwrap().info(), CLASSES.lock().unwrap()[ret.unwrap().id.get() as usize - 1]),
	      "On success, the class's info matches the entry in the array.")]
    pub fn new(config: ClassConfig) -> Result<Class, &'static str> {
        let mut classes = CLASSES.lock().unwrap();

        let next_id = classes.len() + 1;
        if next_id > u32::MAX as usize {
            return Err("too many slitter allocation classes");
        }

        let id = Class {
            id: NonZeroU32::new(next_id as u32).expect("next_id is positive"),
        };

        let info = Box::leak(Box::new(ClassInfo {
            name: config.name,
            layout: config.layout,
            rack: crate::magazine::get_default_rack(),
            full_mags: MagazineStack::new(),
            partial_mags: MagazineStack::new(),
            press: Press::new(id, config.layout)?,
            id,
        }));
        classes.push(info);
        Ok(id)
    }

    /// Returns a `Class` struct for `id` if such a class exists.
    ///
    /// On success, this operation can be inverted by calling `id()`.
    #[ensures(ret.is_none() -> CLASSES.lock().unwrap().iter().all(|info| info.id.id != id),
	      "`from_id` only fails if there is no registered `ClassInfo` with that id.")]
    #[ensures(ret.is_some() -> CLASSES.lock().unwrap()[id.get() as usize - 1].id == ret.unwrap(),
	      "On success, the class's info is at id - 1 in the global array of info.")]
    #[ensures(ret.is_some() -> ret.unwrap().id == id,
	      "On success, the return value's id matches the argument.")]
    pub(crate) fn from_id(id: NonZeroU32) -> Option<Class> {
        let guard = CLASSES.lock().unwrap();
        if id.get() as usize <= guard.len() {
            Some(Class { id })
        } else {
            None
        }
    }

    /// Returns the `Class`'s underlying `NonZeroU32` id.
    ///
    /// This operation is the inverse of `Class::from_id`.
    #[ensures(Class::from_id(ret) == Some(self),
	      "We can recover the same `Class` with `Class::from_id`.")]
    #[inline]
    pub(crate) fn id(self) -> NonZeroU32 {
        self.id
    }

    /// Returns the global `ClassInfo` for this `Class`.
    #[ensures(ret.id == self)]
    pub(crate) fn info(self) -> &'static ClassInfo {
        let guard = CLASSES.lock().unwrap();

        (*guard)
            .get(self.id.get() as usize - 1)
            .expect("Class structs are only build for valid ids")
    }
}

#[cfg(test)]
mod test {
    use proptest::bool;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use proptest::sample;
    use std::alloc::Layout;
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use crate::Class;
    use crate::ClassConfig;

    #[test]
    fn smoke_test() {
        let class = Class::new(ClassConfig {
            name: Some("alloc_smoke".into()),
            layout: Layout::from_size_align(8, 8).expect("layout should build"),
        })
        .expect("Class should build");

        let p0 = class.allocate().expect("Should allocate");
        let p1 = class.allocate().expect("Should allocate");

        class.release(p0);

        let p2 = class.allocate().expect("Should allocate");

        class.release(p2);
        class.release(p1);
    }

    // Returns true iff that `new` isn't in `current`.
    fn check_new_allocation(current: &[NonNull<c_void>], new: NonNull<c_void>) -> bool {
        current.iter().all(|x| x.as_ptr() != new.as_ptr())
    }

    proptest! {
        // Bulk allocate, then deallocate and re-allocate in random-ish order.
        #[test]
        fn random_order(indices in vec(0..20usize, 1..50)) {
            use std::collections::HashSet;

            let class = Class::new(ClassConfig {
                name: Some("random".into()),
                layout: Layout::from_size_align(8, 8).expect("layout should build"),
            })
            .expect("Class should build");

            // If a slot is None, we will allocate in there the next
            // time we hit it.  If it holds a `NonNull`, we will
            // instead consume and free its contents.
            let mut slots: Vec<Option<NonNull<c_void>>> = Vec::new();

            // Initialise with 20 allocations.
            slots.resize_with(20, || class.allocate());

            // Make sure all the allocations are unique.
            prop_assert!(slots.len() ==
                         slots
                         .iter()
                         .map(|x| x.expect("alloc should succeed").as_ptr())
                         .collect::<HashSet<_>>()
                         .len());
            for index in indices.iter().cloned() {
                if let Some(alloc) = slots[index].take() {
                    class.release(alloc);
                } else {
                    let new_alloc = class.allocate();

                    prop_assert!(new_alloc.is_some());

                    let fresh = slots.iter().all(|x| {
                        match x {
                            Some(p) => p.as_ptr() != new_alloc.unwrap().as_ptr(),
                            None => true,
                        }
                    });
                    prop_assert!(fresh);
                    slots[index] = new_alloc;
                }
            }

            for slot in slots.iter_mut() {
                if let Some(freed) = slot.take() {
                    class.release(freed);
                }
            }
        }

        // Allocate and deallocate in random-ish order from two classes.
        #[test]
        fn random_order_two_classes(indices in vec((0..10usize, 0..2usize), 1..50)) {
            let classes = vec![
                Class::new(ClassConfig {
                    name: Some("random_class_1".into()),
                    layout: Layout::from_size_align(8, 8).expect("layout should build")
                }).expect("Class should build"),
                Class::new(ClassConfig {
                    name: Some("random_class_2".into()),
                    layout: Layout::from_size_align(16, 8).expect("layout should build")
                }).expect("Class should build"),
            ];

            // If a slot is None, we will allocate in there the next
            // time we hit it.  If it holds a `NonNull`, we will
            // instead consume and free its contents.
            let mut slots: Vec<Option<(NonNull<c_void>, Class)>> = Vec::new();

            slots.resize(20, None);
            for (index, class_id) in indices.iter().cloned() {
                if let Some((alloc, class)) = slots[index].take() {
                    class.release(alloc);
                } else {
                    let class = classes[class_id];
                    let new_alloc = class.allocate();

                    prop_assert!(new_alloc.is_some());

                    let fresh = slots.iter().all(|x| {
                        match x {
                            Some((p, _)) => p.as_ptr() != new_alloc.unwrap().as_ptr(),
                            None => true,
                        }
                    });
                    prop_assert!(fresh);

                    slots[index] = Some((new_alloc.unwrap(), class));
                }
            }

            for slot in slots.iter_mut() {
                if let Some((freed, class)) = slot.take() {
                    class.release(freed);
                }
            }
        }

        // Check that we can correctly allocate and deallocate in stack order.
        #[test]
        fn lifo(push_pop in vec(bool::ANY, 2..50)) {
            let class = Class::new(ClassConfig {
                name: Some("lifo".into()),
                layout: Layout::from_size_align(8, 8).expect("layout should build"),
            })
            .expect("Class should build");

            let mut stack: Vec<NonNull<c_void>> = Vec::new();

            for alloc in push_pop.iter().cloned() {
                if alloc {
                    let new_alloc = class.allocate();

                    prop_assert_ne!(new_alloc, None);
                    let block = new_alloc.unwrap();

                    prop_assert!(check_new_allocation(&stack, block));
                    stack.push(block);
                } else if let Some(freed) = stack.pop() {
                    class.release(freed);
                }
            }

            while let Some(freed) = stack.pop() {
                class.release(freed);
            }
        }

        // Check that we can correctly allocate and deallocate in queue order.
        #[test]
        fn fifo(push_pop in vec(bool::ANY, 2..50)) {
            let class = Class::new(ClassConfig {
                name: Some("lifo".into()),
                layout: Layout::from_size_align(8, 8).expect("layout should build"),
            })
            .expect("Class should build");

            let mut queue: VecDeque<NonNull<c_void>> = VecDeque::new();

            for alloc in push_pop.iter().cloned() {
                if alloc {
                    let new_alloc = class.allocate();

                    prop_assert_ne!(new_alloc, None);
                    let block = new_alloc.unwrap();

                    prop_assert!(check_new_allocation(queue.make_contiguous(), block));
                    queue.push_back(block);
                } else if let Some(freed) = queue.pop_front() {
                    class.release(freed);
                }
            }

            while let Some(freed) = queue.pop_back() {
                class.release(freed);
            }
        }

        // Check that we can correctly allocate and deallocate in FIFO or LIFO order.
        //
        // 0 means allocate, -1 frees from the front, and 1 freeds from back.
        #[test]
        fn biendian(actions in vec(sample::select(vec![-1, 0, 1]), 2..50)) {
            let class = Class::new(ClassConfig {
                name: Some("lifo".into()),
                layout: Layout::from_size_align(8, 8).expect("layout should build"),
            })
            .expect("Class should build");

            let mut queue: VecDeque<NonNull<c_void>> = VecDeque::new();

            for action in actions.iter().cloned() {
                if action == 0 {
                    let new_alloc = class.allocate();

                    prop_assert_ne!(new_alloc, None);
                    let block = new_alloc.unwrap();

                    prop_assert!(check_new_allocation(queue.make_contiguous(), block));
                    queue.push_back(block);
                } else if action == -1 {
                    if let Some(freed) = queue.pop_front() {
                        class.release(freed);
                    }
                } else if let Some(freed) = queue.pop_back() {
                    class.release(freed);
                }
            }

            while let Some(freed) = queue.pop_back() {
                class.release(freed);
            }
        }
    }
}
