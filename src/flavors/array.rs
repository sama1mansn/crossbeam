//! Bounded channel based on a preallocated array.
//!
//! This flavor has a fixed, positive capacity.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use crossbeam_utils::CachePadded;

use err::{RecvError, SendError, TryRecvError, TrySendError};
use internal::context::Context;
use internal::select::{Operation, SelectHandle, Selected, Token};
use internal::utils::Backoff;
use internal::waker::SyncWaker;

/// A slot in a channel.
struct Slot<T> {
    /// The current stamp.
    ///
    /// If the stamp equals the tail, this node will be next written to. If it equals the head,
    /// this node will be next read from.
    stamp: AtomicUsize,

    /// The message in this slot.
    ///
    /// If the lap in the stamp is odd, this value contains a message. Otherwise, it is empty.
    msg: UnsafeCell<T>,
}

/// The token type for the array flavor.
pub struct ArrayToken {
    /// Slot to read from or write to.
    slot: *const u8,

    /// Stamp to store into the slot after reading or writing.
    stamp: usize,
}

impl Default for ArrayToken {
    #[inline]
    fn default() -> Self {
        ArrayToken {
            slot: ptr::null(),
            stamp: 0,
        }
    }
}

/// Bounded channel based on a preallocated array.
///
/// The implementation is based on Dmitry Vyukov's bounded MPMC queue:
///
/// - http://www.1024cores.net/home/lock-free-algorithms/queues/bounded-mpmc-queue
/// - https://docs.google.com/document/d/1yIAYmbvL3JxOKOjuCyon7JhW4cSv1wy5hC0ApeGMV9s/pub
pub struct Channel<T> {
    /// The head of the channel.
    ///
    /// This value is a "stamp" consisting of an index into the buffer and a lap, but packed into a
    /// single `usize`. The lower bits represent the index, while the upper bits represent the lap.
    /// The lap in the head is always an odd number.
    ///
    /// Messages are popped from the head of the channel.
    head: CachePadded<AtomicUsize>,

    /// The tail of the channel.
    ///
    /// This value is a "stamp" consisting of an index into the buffer and a lap, but packed into a
    /// single `usize`. The lower bits represent the index, while the upper bits represent the lap.
    /// The lap in the tail is always an even number.
    ///
    /// Messages are pushed into the tail of the channel.
    tail: CachePadded<AtomicUsize>,

    /// The buffer holding slots.
    buffer: *mut Slot<T>,

    /// The channel capacity.
    cap: usize,

    /// A stamp with the value of `{ lap: 1, index: 0 }`.
    one_lap: usize,

    /// Equals `true` when the channel is closed.
    is_closed: AtomicBool,

    /// Senders waiting while the channel is full.
    senders: SyncWaker,

    /// Receivers waiting while the channel is empty and not closed.
    receivers: SyncWaker,

    /// Indicates that dropping a `Channel<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Channel<T> {
    /// Creates a bounded channel of capacity `cap`.
    ///
    /// # Panics
    ///
    /// Panics if the capacity is not in range `1 .. usize::max_value() / 4 + 1`.
    pub fn with_capacity(cap: usize) -> Self {
        assert!(cap > 0, "capacity must be positive");

        // Make sure there are at least two most significant bits to encode laps. If we can't
        // reserve two bits, then panic. In that case, the buffer is likely too large to allocate
        // anyway.
        let cap_limit = usize::max_value() / 4;
        assert!(
            cap <= cap_limit,
            "channel capacity is too large: {} > {}",
            cap,
            cap_limit
        );

        // One lap is the smallest power of two greater than or equal to `cap`.
        let one_lap = cap.next_power_of_two();

        // Head is initialized to `{ lap: 1, index: 0 }`.
        // Tail is initialized to `{ lap: 0, index: 0 }`.
        let head = one_lap;
        let tail = 0;

        // Allocate a buffer of `cap` slots.
        let buffer = {
            let mut v = Vec::<Slot<T>>::with_capacity(cap);
            let ptr = v.as_mut_ptr();
            mem::forget(v);
            ptr
        };

        // Initialize stamps in the slots.
        for i in 0..cap {
            unsafe {
                // Set the stamp to `{ lap: 0, index: i }`.
                let slot = buffer.add(i);
                ptr::write(&mut (*slot).stamp, AtomicUsize::new(i));
            }
        }

        Channel {
            buffer,
            cap,
            one_lap,
            is_closed: AtomicBool::new(false),
            head: CachePadded::new(AtomicUsize::new(head)),
            tail: CachePadded::new(AtomicUsize::new(tail)),
            senders: SyncWaker::new(),
            receivers: SyncWaker::new(),
            _marker: PhantomData,
        }
    }

    /// Returns a receiver handle to the channel.
    pub fn receiver(&self) -> Receiver<T> {
        Receiver(self)
    }

    /// Returns a sender handle to the channel.
    pub fn sender(&self) -> Sender<T> {
        Sender(self)
    }

    /// Attempts to reserve a slot for sending a message.
    fn start_send(&self, token: &mut Token) -> bool {
        // If the channel is closed, return early.
        if self.is_closed() {
            token.array.slot = ptr::null();
            token.array.stamp = 0;
            return true;
        }

        let mut backoff = Backoff::new();

        loop {
            // Load the tail and deconstruct it.
            let tail = self.tail.load(Ordering::SeqCst);
            let index = tail & (self.one_lap - 1);
            let lap = tail & !(self.one_lap - 1);

            // Inspect the corresponding slot.
            let slot = unsafe { &*self.buffer.add(index) };
            let stamp = slot.stamp.load(Ordering::Acquire);

            // If the tail and the stamp match, we may attempt to push.
            if tail == stamp {
                let new_tail = if index + 1 < self.cap {
                    // Same lap, incremented index.
                    // Set to `{ lap: lap, index: index + 1 }`.
                    tail + 1
                } else {
                    // Two laps forward, index wraps around to zero.
                    // Set to `{ lap: lap.wrapping_add(2), index: 0 }`.
                    lap.wrapping_add(self.one_lap.wrapping_mul(2))
                };

                // Try moving the tail.
                if self
                    .tail
                    .compare_exchange_weak(tail, new_tail, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    // Prepare the token for the follow-up call to `write`.
                    token.array.slot = slot as *const Slot<T> as *const u8;
                    token.array.stamp = stamp.wrapping_add(self.one_lap);
                    return true;
                }
            // But if the slot lags one lap behind the tail...
            } else if stamp.wrapping_add(self.one_lap) == tail {
                let head = self.head.load(Ordering::SeqCst);

                // ...and if the head lags one lap behind the tail as well...
                if head.wrapping_add(self.one_lap) == tail {
                    // ...then the channel is full.
                    return false;
                }
            }

            backoff.spin();
        }
    }

    /// Writes a message into the channel.
    pub unsafe fn write(&self, token: &mut Token, msg: T) -> Result<(), T> {
        // If there is no slot, the channel is closed.
        if token.array.slot.is_null() {
            return Err(msg);
        }

        let slot: &Slot<T> = &*(token.array.slot as *const Slot<T>);

        // Write the message into the slot and update the stamp.
        slot.msg.get().write(msg);
        slot.stamp.store(token.array.stamp, Ordering::Release);

        // Wake a sleeping receiver.
        self.receivers.wake_one();
        Ok(())
    }

    /// Attempts to reserve a slot for receiving a message.
    fn start_recv(&self, token: &mut Token) -> bool {
        let mut backoff = Backoff::new();

        loop {
            // Load the head and deconstruct it.
            let head = self.head.load(Ordering::SeqCst);
            let index = head & (self.one_lap - 1);
            let lap = head & !(self.one_lap - 1);

            // Inspect the corresponding slot.
            let slot = unsafe { &*self.buffer.add(index) };
            let stamp = slot.stamp.load(Ordering::Acquire);

            // If the the head and the stamp match, we may attempt to pop.
            if head == stamp {
                let new = if index + 1 < self.cap {
                    // Same lap, incremented index.
                    // Set to `{ lap: lap, index: index + 1 }`.
                    head + 1
                } else {
                    // Two laps forward, index wraps around to zero.
                    // Set to `{ lap: lap.wrapping_add(2), index: 0 }`.
                    lap.wrapping_add(self.one_lap.wrapping_mul(2))
                };

                // Try moving the head.
                if self
                    .head
                    .compare_exchange_weak(head, new, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    // Prepare the token for the follow-up call to `read`.
                    token.array.slot = slot as *const Slot<T> as *const u8;
                    token.array.stamp = stamp.wrapping_add(self.one_lap);
                    return true;
                }
            // But if the slot lags one lap behind the head...
            } else if stamp.wrapping_add(self.one_lap) == head {
                let tail = self.tail.load(Ordering::SeqCst);

                // ...and if the tail lags one lap behind the head as well, that means the channel
                // is empty.
                if tail.wrapping_add(self.one_lap) == head {
                    // If the channel is closed...
                    if self.is_closed() {
                        // ...and still empty...
                        if self.tail.load(Ordering::SeqCst) == tail {
                            // ...then receive an error.
                            token.array.slot = ptr::null();
                            token.array.stamp = 0;
                            return true;
                        }
                    } else {
                        // Otherwise, the receive operation is not ready.
                        return false;
                    }
                }
            }

            backoff.spin();
        }
    }

    /// Reads a message from the channel.
    pub unsafe fn read(&self, token: &mut Token) -> Result<T, ()> {
        if token.array.slot.is_null() {
            // The channel is closed.
            return Err(());
        }

        let slot: &Slot<T> = &*(token.array.slot as *const Slot<T>);

        // Read the message from the slot and update the stamp.
        let msg = slot.msg.get().read();
        slot.stamp.store(token.array.stamp, Ordering::Release);

        // Wake a sleeping sender.
        self.senders.wake_one();
        Ok(msg)
    }

    /// Attempts to send a message into the channel.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        let token = &mut Token::default();
        if self.start_send(token) {
            unsafe {
                return self.write(token, msg).map_err(|m| TrySendError::Disconnected(m));
            }
        } else {
            Err(TrySendError::Full(msg))
        }
    }

    /// Sends a message into the channel.
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        let token = &mut Token::default();
        loop {
            // Try sending a message several times.
            let mut backoff = Backoff::new();
            loop {
                if self.start_send(token) {
                    unsafe {
                        return self.write(token, msg).map_err(|m| SendError(m));
                    }
                }
                if !backoff.snooze() {
                    break;
                }
            }

            Context::with(|cx| {
                // Prepare for blocking until a receiver wakes us up.
                let oper = Operation::hook(token);
                self.senders.register(oper, cx);

                // Has the channel become ready just now?
                if !self.is_full() {
                    let _ = cx.try_select(Selected::Aborted);
                }

                // Block the current thread.
                let sel = cx.wait_until(None);

                match sel {
                    Selected::Waiting => unreachable!(),
                    Selected::Aborted | Selected::Closed => {
                        self.senders.unregister(oper).unwrap();
                    }
                    Selected::Operation(_) => {}
                }
            })
        }
    }

    /// Attempts to receive a message without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let token = &mut Token::default();

        if self.start_recv(token) {
            unsafe {
                self.read(token).map_err(|_| TryRecvError::Disconnected)
            }
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Receives a message from the channel.
    pub fn recv(&self) -> Result<T, RecvError> {
        let token = &mut Token::default();
        loop {
            // Try receiving a message several times.
            let mut backoff = Backoff::new();
            loop {
                if self.start_recv(token) {
                    unsafe {
                        return self.read(token).map_err(|_| RecvError);
                    }
                }
                if !backoff.snooze() {
                    break;
                }
            }

            Context::with(|cx| {
                // Prepare for blocking until a sender wakes us up.
                let oper = Operation::hook(token);
                self.receivers.register(oper, cx);

                // Has the channel become ready just now?
                if !self.is_empty() || self.is_closed() {
                    let _ = cx.try_select(Selected::Aborted);
                }

                // Block the current thread.
                let sel = cx.wait_until(None);

                match sel {
                    Selected::Waiting => unreachable!(),
                    Selected::Aborted | Selected::Closed => {
                        self.receivers.unregister(oper).unwrap();
                        // If the channel was closed, we still have to check for remaining messages.
                    }
                    Selected::Operation(_) => {}
                }
            })
        }
    }

    /// Returns the current number of messages inside the channel.
    pub fn len(&self) -> usize {
        loop {
            // Load the tail, then load the head.
            let tail = self.tail.load(Ordering::SeqCst);
            let head = self.head.load(Ordering::SeqCst);

            // If the tail didn't change, we've got consistent values to work with.
            if self.tail.load(Ordering::SeqCst) == tail {
                let hix = head & (self.one_lap - 1);
                let tix = tail & (self.one_lap - 1);

                return if hix < tix {
                    tix - hix
                } else if hix > tix {
                    self.cap - hix + tix
                } else if tail.wrapping_add(self.one_lap) == head {
                    0
                } else {
                    self.cap
                };
            }
        }
    }

    /// Returns the capacity of the channel.
    pub fn capacity(&self) -> Option<usize> {
        Some(self.cap)
    }

    /// Closes the channel and wakes up all blocked receivers.
    pub fn close(&self) {
        if !self.is_closed.swap(true, Ordering::SeqCst) {
            self.senders.close();
            self.receivers.close();
        }
    }

    /// Returns `true` if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }

    /// Returns `true` if the channel is empty.
    pub fn is_empty(&self) -> bool {
        let head = self.head.load(Ordering::SeqCst);
        let tail = self.tail.load(Ordering::SeqCst);

        // Is the tail lagging one lap behind head?
        //
        // Note: If the head changes just before we load the tail, that means there was a moment
        // when the channel was not empty, so it is safe to just return `false`.
        tail.wrapping_add(self.one_lap) == head
    }

    /// Returns `true` if the channel is full.
    pub fn is_full(&self) -> bool {
        let tail = self.tail.load(Ordering::SeqCst);
        let head = self.head.load(Ordering::SeqCst);

        // Is the head lagging one lap behind tail?
        //
        // Note: If the tail changes just before we load the head, that means there was a moment
        // when the channel was not full, so it is safe to just return `false`.
        head.wrapping_add(self.one_lap) == tail
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        // Get the index of the head.
        let hix = self.head.load(Ordering::Relaxed) & (self.one_lap - 1);

        // Loop over all slots that hold a message and drop them.
        for i in 0..self.len() {
            // Compute the index of the next slot holding a message.
            let index = if hix + i < self.cap {
                hix + i
            } else {
                hix + i - self.cap
            };

            unsafe {
                self.buffer.add(index).drop_in_place();
            }
        }

        // Finally, deallocate the buffer, but don't run any destructors.
        unsafe {
            Vec::from_raw_parts(self.buffer, 0, self.cap);
        }
    }
}

/// Receiver handle to a channel.
pub struct Receiver<'a, T: 'a>(&'a Channel<T>);

/// Sender handle to a channel.
pub struct Sender<'a, T: 'a>(&'a Channel<T>);

impl<'a, T> SelectHandle for Receiver<'a, T> {
    fn try(&self, token: &mut Token) -> bool {
        self.0.start_recv(token)
    }

    fn retry(&self, token: &mut Token) -> bool {
        self.0.start_recv(token)
    }

    fn deadline(&self) -> Option<Instant> {
        None
    }

    fn register(&self, _token: &mut Token, oper: Operation, cx: &Context) -> bool {
        self.0.receivers.register(oper, cx);
        self.0.is_empty() && !self.0.is_closed()
    }

    fn unregister(&self, oper: Operation) {
        self.0.receivers.unregister(oper);
    }

    fn accept(&self, token: &mut Token, _cx: &Context) -> bool {
        self.0.start_recv(token)
    }

    fn state(&self) -> usize {
        self.0.tail.load(Ordering::SeqCst)
    }
}

impl<'a, T> SelectHandle for Sender<'a, T> {
    fn try(&self, token: &mut Token) -> bool {
        self.0.start_send(token)
    }

    fn retry(&self, token: &mut Token) -> bool {
        self.0.start_send(token)
    }

    fn deadline(&self) -> Option<Instant> {
        None
    }

    fn register(&self, _token: &mut Token, oper: Operation, cx: &Context) -> bool {
        self.0.senders.register(oper, cx);
        self.0.is_full() && !self.0.is_closed()
    }

    fn unregister(&self, oper: Operation) {
        self.0.senders.unregister(oper);
    }

    fn accept(&self, token: &mut Token, _cx: &Context) -> bool {
        self.0.start_send(token)
    }

    fn state(&self) -> usize {
        self.0.head.load(Ordering::SeqCst)
    }
}
