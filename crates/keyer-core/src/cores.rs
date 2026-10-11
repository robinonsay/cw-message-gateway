//! What the processor's two cores share: the keyer's loop on core 0
//! ([`crate::control`]) and the decoder's on core 1 (`decoder_core`) meet only
//! here, in one [`Link`] in memory both can reach. There are no locks and nothing
//! waits: each side reads what the other has left and goes on.
//!
//! Each field has one writer, and the reader trusts nothing it reads from the
//! other side: a ring whose other end's count is impossible is treated as empty or
//! full, never indexed past its end, so that a core 1 gone wrong cannot make core
//! 0's loop fault or stall here (the keyer's watch on core 1 trips the box for it
//! instead). Every byte is an atomic, so that reading a byte the other side is
//! writing is a stale value, not undefined behaviour.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

/// A one-way byte ring: one core writes it ([`Ring::write`], [`Ring::write_all`]),
/// the other reads it ([`Ring::read`]). `N` bytes, a power of two.
///
/// The writer owns `tail` (the count of bytes ever written) and the reader `head`
/// (the count ever read), both counting on through wraps of `u32`; a byte's slot
/// is its count modulo `N`. The writer publishes its bytes with a release store of
/// `tail` after writing them, and the reader frees their slots with a release
/// store of `head` after reading them, each read by the other side with an
/// acquire load first.
pub struct Ring<const N: usize> {
    bytes: [AtomicU8; N],
    head: AtomicU32,
    tail: AtomicU32,
}

impl<const N: usize> Ring<N> {
    const MASK: u32 = {
        assert!(N.is_power_of_two() && N <= 1 << 16);
        N as u32 - 1
    };

    pub const fn new() -> Self {
        Self {
            bytes: [const { AtomicU8::new(0) }; N],
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
        }
    }

    /// The writer: bytes free, as the reader last left them; 0 if the reader's
    /// count is impossible (more than `N` behind).
    pub fn room(&self) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        (N as u32).saturating_sub(tail.wrapping_sub(head)) as usize
    }

    /// The writer: as many of `bytes` as fit, in order; how many.
    pub fn write(&self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(self.room());
        self.put(&bytes[..n]);
        n
    }

    /// The writer: all of `bytes`, or none if they do not all fit; whether they
    /// went.
    pub fn write_all(&self, bytes: &[u8]) -> bool {
        if bytes.len() > self.room() {
            return false;
        }
        self.put(bytes);
        true
    }

    /// Write `bytes`, which fit, and publish them.
    fn put(&self, bytes: &[u8]) {
        let tail = self.tail.load(Ordering::Relaxed);
        for (i, &b) in bytes.iter().enumerate() {
            let slot = tail.wrapping_add(i as u32) & Self::MASK;
            self.bytes[slot as usize].store(b, Ordering::Relaxed);
        }
        self.tail
            .store(tail.wrapping_add(bytes.len() as u32), Ordering::Release);
    }

    /// The reader: the bytes written since the last read, oldest first, into the
    /// front of `buf`, as many as fit; how many. If the writer's count is
    /// impossible (more than `N` ahead), what is in the ring is dropped and
    /// nothing is read.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let ready = tail.wrapping_sub(head);
        if ready > N as u32 {
            self.head.store(tail, Ordering::Release);
            return 0;
        }
        let n = (ready as usize).min(buf.len());
        for (i, b) in buf.iter_mut().take(n).enumerate() {
            let slot = head.wrapping_add(i as u32) & Self::MASK;
            *b = self.bytes[slot as usize].load(Ordering::Relaxed);
        }
        self.head
            .store(head.wrapping_add(n as u32), Ordering::Release);
        n
    }
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Bytes in the ring from core 0 to core 1: the host's `RX` commands, a line at a
/// time.
pub const TO_CORE1: usize = 512;
/// Bytes in the ring from core 1 to core 0: the decoder's lines. At most about
/// 2 kB a second in hfnode mode; core 0 takes a line every pass.
pub const FROM_CORE1: usize = 2048;

/// A bring-up test of core 1, as [`Link::test`] carries it.
pub const TEST_NONE: u32 = 0;
pub const TEST_HANG: u32 = 1;
pub const TEST_FAULT: u32 = 2;

/// Everything the two cores share.
pub struct Link {
    /// Core 1's heartbeat: core 1 adds one every pass of its loop.
    pub beat: AtomicU32,
    /// Set by the fault and panic handlers, on either core.
    pub fault: AtomicBool,
    /// Core 0's USB link epoch, for the decoder's `link_epoch`.
    pub epoch: AtomicU32,
    /// Whether core 0 has the key or the PTT down, for the decoder's `keyed`.
    pub keyed: AtomicBool,
    /// A bring-up test for core 1 ([`TEST_HANG`], [`TEST_FAULT`]), set by core 0.
    pub test: AtomicU32,
    /// Lines from core 0 to core 1.
    pub to_core1: Ring<TO_CORE1>,
    /// Bytes from core 1 to core 0.
    pub from_core1: Ring<FROM_CORE1>,
}

impl Link {
    pub const fn new() -> Self {
        Self {
            beat: AtomicU32::new(0),
            fault: AtomicBool::new(false),
            epoch: AtomicU32::new(0),
            keyed: AtomicBool::new(false),
            test: AtomicU32::new(TEST_NONE),
            to_core1: Ring::new(),
            from_core1: Ring::new(),
        }
    }
}

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
