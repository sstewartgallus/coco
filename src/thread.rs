//! Thread synchronization and pinning
//!
//! # The global epoch
//!
//! The global `STATE` number holds two pieces of data: the current global epoch and whether
//! garbage needs to be urgently collected. Every so often the global epoch is incremented - we say
//! it "advances". It can advance only if all currently pinned threads have been pinned in the
//! current epoch.
//!
//! If an object became unreachable in some epoch, we can be sure that no thread will hold a
//! reference to it after two epoch advancements - that is the moment when it will be safe to free
//! it's memory.
//!
//! # Registration
//!
//! In order to track all threads in one place, we need some form of thread registration. Every
//! thread has a thread-local so-called "harness" that registers it the first time it is pinned,
//! and unregisters when it exits.
//!
//! Registered threads are tracked in a global lock-free singly-linked list of thread entries. The
//! head of this list is accessed by calling the `participants` function.
//!
//! Thread entries are implemented as the `Thread` data type. Every entry contains an integer that
//! tells whether the thread is pinned and if so, what was the global epoch at the time it was
//! pinned.
//!
//! # Stashing garbage
//!
//! If a pinned thread wants to stash away an unlinked object to free it's memory at a later safe
//! time, it will store it in it's thread-local bag. If the local bag is full, it must first be
//! replaced with a fresh one. The old bag is then pushed into a global garbage queue and marked
//! with the current epoch.
//!
//! Global garbage queues live in the `garbage` module. All local bags eventually end up there.
//! Threads are good citizens so they sometimes pop a few bags of garbage from the global queues
//! in order to free some memory and thus help reduce the amount of accumulated garbage.

use std::cell::Cell;
use std::mem;
use std::ptr;
use std::sync::atomic::Ordering::{self, AcqRel, Acquire, Relaxed, Release, SeqCst};
use std::sync::atomic::{self, AtomicUsize, ATOMIC_USIZE_INIT};

use super::garbage::{self, Bag, Urgency};
use super::{Atomic, Ptr, TaggedAtomic, TaggedPtr};

/// The global state (epoch and urgency).
///
/// The last bit in this number indicates that garbage must be urgently collected, and the rest of
/// the bits encode the current global epoch, which is always an even number.
///
/// More precisely:
///
/// * Urgency: `state & 1 == 1`
/// * Epoch: `state & !1`
///
/// The global epoch is advanced by increasing the state by 2, and wrapping it around on overflow.
/// A pinned thread may advance the epoch only if all pinned threads have been pinned with the
/// current epoch.
static STATE: AtomicUsize = ATOMIC_USIZE_INIT;

/// Head pointer to the singly-linked list of participating threads.
///
/// This `AtomicUsize` is actually a `TaggedAtomic<Thread>`. Until we get const functions in Rust,
/// this is an easy zero-cost method of initializing it. This head pointer must be accessed using
/// the `participants` function.
///
/// Each thread is registered on it's first call to `pin()` by adding it's own newly allocated entry
/// to the head of this list. Unregistration is triggered by destruction of the thread-local
/// `Harness`, which happens on thread exit.
static PARTICIPANTS: AtomicUsize = ATOMIC_USIZE_INIT;

thread_local! {
    /// The thread registration harness.
    ///
    /// The harness is lazily initialized on it's first use. Initialization performs registration.
    /// If initialized, the harness will get destructed on thread exit, which in turn unregisters
    /// the thread.
    static HARNESS: Harness = Harness {
        thread: Thread::register(),
        pin_count: Cell::new(0),
        bag: Cell::new(Box::into_raw(Box::new(Bag::new()))),
    };
}

/// Holds thread-local data and unregisters the thread when dropped.
struct Harness {
    /// This thread's entry in the participants list.
    thread: *const Thread,

    /// Number of pending uses of `pin()`.
    pin_count: Cell<usize>,

    /// The local bag of unlinked objects.
    bag: Cell<*mut Bag>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Now that the thread is exiting, we must move the local bag into the global garbage
        // queue. Also, let's try advancing the epoch and help free some garbage.
        let thread = unsafe { &*self.thread };

        // If we called `pin()` here, it would try to access `HARNESS` and then panic.
        // To work around the problem, we manually pin the thread.
        thread.set_pinned();
        let guard = Guard { harness: self };

        // Spare some cycles on garbage collection.
        // Note: this may itself produce garbage and in turn allocate new bags.
        advance(&guard);
        let epoch = STATE.load(SeqCst) & !1;
        garbage::collect(epoch, &guard);

        // Push the local bag into a garbage queue.
        let bag = unsafe { Box::from_raw(self.bag.get()) };
        if garbage::push(bag, epoch, &guard) == Urgency::Urgent {
            STATE.fetch_or(1, SeqCst);
        }

        // Forget the guard and manually unpin the thread.
        mem::forget(guard);
        thread.set_unpinned();

        // Mark the thread entry as deleted.
        thread.unregister();
    }
}

/// An entry in the linked list of participanting threads.
struct Thread {
    /// The global epoch just before the thread got pinned.
    ///
    /// If this number is odd, the thread is not pinned. In other words, the least significant bit
    /// indicates that the thread is unpinned. Epochs are always even numbers so that they fit into
    /// the upper bits.
    state: AtomicUsize,

    /// The next thread in the linked list of participants.
    ///
    /// If the tag is 1, that signifies this entry is deleted and can be freely removed from the
    /// list. Every participating thread sets the tag to 1 when it exits.
    next: TaggedAtomic<Thread>,
}

impl Thread {
    /// Marks the thread as pinned and returns the global epoch just before pinning.
    ///
    /// Must not be called if the thread is already pinned!
    #[inline]
    fn set_pinned(&self) -> Urgency {
        let state = STATE.load(Relaxed);
        let epoch = state & !1;
        self.state.store(epoch, Relaxed);

        // Any further loads must not precede the store. In order words, this thread's epoch must
        // be fully announced before we load anything from the memory shared throught `Atomic`s.
        atomic::fence(SeqCst);

        match state & 1 {
            0 => Urgency::Normal,
            _ => Urgency::Urgent,
        }
    }

    /// Marks the thread as unpinned.
    #[inline]
    fn set_unpinned(&self) {
        // Nothing special about number 1, any odd number marks the thread as unpinned.
        atomic::fence(Release);
        self.state.store(1, Relaxed);
    }

    /// Registers a thread by adding a new entry to the list of participanting threads.
    ///
    /// Returns a pointer to the newly allocated entry.
    fn register() -> *mut Thread {
        let list = participants();

        let mut new = Box::new(Thread {
            // Nothing special about number 1, any odd number marks the thread as unpinned.
            state: AtomicUsize::new(1),
            next: TaggedAtomic::null(0),
        });
        let ptr = &mut *new as *mut _;

        // This code is executing while the thread harness is initializing, so normal pinning would
        // try to access it while it is being initialized. Such accesses fail with a panic. We must
        // therefore cheat by creating a fake guard and then forgetting it.
        let guard = unsafe { mem::zeroed::<Guard>() };
        {
            let mut head = list.load(Acquire, &guard);
            loop {
                new.next.store(head, Relaxed);

                // Try installing this thread's entry as the new head.
                match list.cas_box_weak(head, new, 0, AcqRel) {
                    Ok(n) => break,
                    Err((h, n)) => {
                        head = h;
                        new = n;
                    }
                }
            }
        }
        mem::forget(guard);

        ptr
    }

    /// Unregisters the thread by marking it's entry as deleted.
    ///
    /// This function doesn't physically remove the entry from the linked list, though. That will
    /// do any future call to `advance`.
    fn unregister(&self) {
        // This code is executing while the thread harness is initializing, so normal pinning would
        // try to access it while it is being initialized. Such accesses fail with a panic. We must
        // therefore cheat by creating a fake guard and then forgetting it.
        let guard = unsafe { mem::zeroed::<Guard>() };
        {
            // Simply mark the next-pointer in this thread's entry.
            let mut next = self.next.load(Acquire, &guard);
            while next.tag() == 0 {
                match self.next.cas_weak(next, next.with_tag(1), AcqRel) {
                    Ok(()) => break,
                    Err(n) => next = n,
                }
            }
        }
        mem::forget(guard);
    }
}

/// Returns a reference to the head pointer of the list of participating threads.
fn participants() -> &'static TaggedAtomic<Thread> {
    // Simply cast the `&'static AtomicUsize` to a `&'static TaggedAtomic<Thread>`.
    unsafe { &*(&PARTICIPANTS as *const _ as *const _) }
}

/// Attempts to advance the global epoch.
///
/// The global epoch can advance only if all currently pinned threads have been pinned in the
/// current epoch.
#[cold]
fn advance(guard: &Guard) {
    let state = STATE.load(SeqCst);
    let epoch = state & !1;

    // Traverse the linked list of participating threads.
    let mut pred = participants();
    let mut curr = pred.load(Acquire, guard);

    while let Some(c) = curr.as_ref() {
        let succ = c.next.load(Acquire, guard);

        if succ.tag() == 1 {
            // This thread has exited. Try unlinking it from the list.
            let succ = succ.with_tag(0);

            if pred.cas(curr, succ, Release).is_err() {
                // We lost the race to unlink the thread. Usually this means we should traverse the
                // list again from the beginning, but since another thread trying to advance the
                // epoch has won the race, we leave the job to that one.
                return;
            }

            // Free the entry allocated by the unlinked thread.
            unsafe { unlinked(c as *const _ as *mut Thread, 1, guard) }

            // Predecessor doesn't change.
            curr = succ;
        } else {
            let thread_state = c.state.load(SeqCst);
            let thread_is_pinned = thread_state & 1 == 0;
            let thread_epoch = thread_state & !1;

            // If the thread was pinned in a different epoch, we cannot advance the global epoch
            // just yet.
            if thread_is_pinned && thread_epoch != epoch {
                return;
            }

            // Move one step forward.
            pred = &c.next;
            curr = succ;
        }
    }

    // All pinned threads were pinned in the current global epoch.
    // Finally, try advancing the epoch. We increment by 2 and simply wrap around on overflow.
    STATE.compare_and_swap(state, state.wrapping_add(2), SeqCst);
}

/// A witness that the current thread is pinned.
///
/// A reference to `Guard` is proof that the current thread is pinned. Lots of methods that
/// interact with `Atomic`s can safely be called only while the thread is pinned so they often
/// require a reference to `Guard`.
///
/// This data type is inherently bound to the thread that created it, therefore it does not
/// implement `Send` nor `Sync`.
///
/// # Examples
///
/// ```
/// use epoch::{self, Atomic, Guard};
/// use std::sync::atomic::Ordering::SeqCst;
///
/// struct Foo(Atomic<String>);
///
/// impl Foo {
///     fn get<'g>(&self, guard: &'g Guard) -> &'g str {
///         self.0.load(SeqCst, guard).unwrap()
///     }
/// }
///
/// let foo = Foo(Atomic::new("hello".to_string()));
///
/// let guard = epoch::pin();
/// assert_eq!(foo.get(&guard), "hello");
/// ```
#[derive(Debug)]
pub struct Guard {
    /// A pointer to the harness.
    ///
    /// This pointer is kept within `Guard` as a matter of convenience. It could also be reached
    /// through the `HARNESS` thread-local, but that doesn't work if we are in the process of it's
    /// destruction.
    harness: *const Harness,
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        let harness = unsafe { &*self.harness };

        let c = harness.pin_count.get();
        harness.pin_count.set(c - 1);

        if c == 1 {
            let thread = unsafe { &*harness.thread };
            thread.set_unpinned();
        }
    }
}

/// Pins the current thread.
///
/// A guard is returned, which unpins the thread as soon as it gets dropped. The guard serves as
/// proof that whatever data you load from an `Atomic` will not be concurrently deleted by another
/// thread while the pin is alive.
///
/// Note that keeping a thread pinned for a long time prevents memory reclamation of any newly
/// deleted objects protected by `Atomic`s. The returned guard should be short-lived: generally
/// speaking, it shouldn't live for more than 100 ms.
///
/// Pinning itself comes with a price: it begins with a `SeqCst` fence and performs a few other
/// atomic operations. However, this mechanism is designed to be as performant as possible, so it
/// can be used pretty liberally. On a modern machine a single pinning takes around 20 nanoseconds.
///
/// Pinning is reentrant. There is no harm in pinning a thread while it's already pinned (repinning
/// is essentially a noop).
///
/// # Examples
///
/// ```
/// use epoch::Atomic;
/// use std::sync::Arc;
/// use std::sync::atomic::Ordering::Relaxed;
/// use std::thread;
///
/// // Create a shared heap-allocated integer.
/// let a = Atomic::new(10);
///
/// {
///     // Pin the current thread.
///     let guard = epoch::pin();
///
///     // Load the atomic.
///     let old = a.load(Relaxed, &guard);
///     assert_eq!(*old.unwrap(), 10);
///
///     // Store a new heap-allocated integer in it's place.
///     a.store_box(Box::new(20), Relaxed, &guard);
///
///     // The old value is not reachable anymore.
///     // The piece of memory it owns will be reclaimed at a later time.
///     unsafe { old.unlinked(&guard) }
///
///     // Load the atomic again.
///     let new = a.load(Relaxed, &guard);
///     assert_eq!(*new.unwrap(), 20);
/// }
///
/// // When `Atomic` gets destructed, it doesn't do anything with the object it references.
/// // We must announce that it got unlinked, otherwise memory gets leaked.
/// let guard = epoch::pin();
/// unsafe { a.load(Relaxed, &guard).unlinked(&guard) }
/// ```
#[inline]
pub fn pin() -> Guard {
    HARNESS.with(|harness| {
        let thread = unsafe { &*harness.thread };
        let guard = Guard { harness: harness };

        let c = harness.pin_count.get();
        harness.pin_count.set(c + 1);

        if c == 0 && thread.set_pinned() == Urgency::Urgent {
            // Try advancing the epoch and collecting some garbage.
            advance(&guard);

            let state = STATE.load(SeqCst);
            let epoch = state & !1;

            if garbage::collect(epoch, &guard) == Urgency::Normal {
                // Clear the urgency flag.
                STATE.compare_and_swap(state, epoch & !1, SeqCst);
            }

            // Unpin and pin again, in order to help advance the epoch as quickly as possible.
            thread.set_unpinned();
            thread.set_pinned();
        }

        guard
    })
}

/// Announces that an object just became unreachable and can be freed as soon as the global epoch
/// sufficiently advances.
///
/// The object was allocated at address `value`, and consists of `count` elements of type `T`.
///
/// Other currently pinned threads might still be holding a reference to the object. When they get
/// unpinned, it will be safe to free it's memory. No particular guarantees are made when exactly
/// that will happen.
///
/// This function should only be used when building primitives like [Atomic] or fiddling with raw
/// pointers.
///
/// [Atomic]: struct.Atomic.html
pub unsafe fn unlinked<T>(value: *mut T, count: usize, guard: &Guard) {
    let size = ::std::mem::size_of::<T>();

    let cell = &(*guard.harness).bag;
    let bag = cell.get();

    assert!((*bag).try_insert(value, count));

    if (*bag).is_full() {
        // Replace the bag with a fresh one.
        cell.set(Box::into_raw(Box::new(Bag::new())));

        // Spare some cycles on garbage collection.
        advance(guard);
        let epoch = STATE.load(SeqCst) & !1;
        garbage::collect(epoch, guard);

        // Finally, push the old bag into the garbage queue.
        let bag = unsafe { Box::from_raw(bag) };
        if garbage::push(bag, epoch, guard) == Urgency::Urgent {
            STATE.fetch_or(1, SeqCst);
        }
    }
}
