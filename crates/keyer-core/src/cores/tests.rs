use super::*;
use std::sync::Arc;
use std::thread;

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[test]
fn bytes_come_out_in_order_and_room_is_what_fits() {
    let r: Ring<16> = Ring::new();
    assert_eq!(r.room(), 16);
    assert_eq!(r.write(b"hello"), 5);
    assert_eq!(r.room(), 11);
    assert!(!r.write_all(&[b'x'; 12]), "12 do not fit");
    assert_eq!(r.room(), 11, "and nothing went");
    assert!(r.write_all(b" world"));
    let mut buf = [0u8; 4];
    assert_eq!(r.read(&mut buf), 4);
    assert_eq!(&buf, b"hell");
    // Around the end of the ring.
    assert_eq!(r.write(b"0123456789"), 9);
    let mut all = [0u8; 32];
    let n = r.read(&mut all);
    assert_eq!(&all[..n], b"o world012345678");
    assert_eq!(r.read(&mut all), 0);
    assert_eq!(r.room(), 16);
}

#[test]
fn the_counts_wrap_through_u32() {
    let r: Ring<8> = Ring::new();
    r.head.store(u32::MAX - 2, Ordering::Relaxed);
    r.tail.store(u32::MAX - 2, Ordering::Relaxed);
    let mut out = Vec::new();
    for i in 0..100u8 {
        assert!(r.write_all(&[i, i]));
        let mut buf = [0u8; 2];
        assert_eq!(r.read(&mut buf), 2);
        out.extend_from_slice(&buf);
    }
    let want: Vec<u8> = (0..100u8).flat_map(|i| [i, i]).collect();
    assert_eq!(out, want);
}

/// One thread writes, another reads, as the two cores do: every byte arrives,
/// once, in order; and what `write_all` writes arrives whole and together.
#[test]
fn two_threads_lose_and_reorder_nothing() {
    let link = Arc::new(Link::new());
    const TOTAL: usize = 400_000;
    let w = {
        let link = link.clone();
        thread::spawn(move || {
            let mut rng = Rng(0x1234_5678_9abc_def1);
            let mut sent = 0usize;
            while sent < TOTAL {
                let len = (1 + rng.next(40) as usize).min(TOTAL - sent);
                let chunk: Vec<u8> = (sent..sent + len).map(|i| (i % 251) as u8).collect();
                let n = if rng.next(2) == 0 {
                    link.from_core1.write(&chunk)
                } else if link.from_core1.write_all(&chunk) {
                    len
                } else {
                    0
                };
                sent += n;
                if n == 0 {
                    thread::yield_now();
                }
            }
        })
    };
    let mut rng = Rng(0x0fed_cba9_8765_4321);
    let mut got = 0usize;
    while got < TOTAL {
        let mut buf = [0u8; 64];
        let want = (1 + rng.next(64) as usize).min(TOTAL - got);
        let n = link.from_core1.read(&mut buf[..want]);
        for (i, &b) in buf[..n].iter().enumerate() {
            assert_eq!(b, ((got + i) % 251) as u8, "byte {}", got + i);
        }
        got += n;
        if n == 0 {
            thread::yield_now();
        }
    }
    w.join().unwrap();
    assert_eq!(link.from_core1.read(&mut [0u8; 8]), 0);
}

/// Whatever the other side's count says, a ring is never read or written past its
/// end and never says more than it holds: an impossible count reads as empty (and
/// is dropped) or as full.
#[test]
fn an_impossible_count_from_the_other_side_is_harmless() {
    let mut rng = Rng(42);
    for _ in 0..10_000 {
        let r: Ring<32> = Ring::new();
        let head = rng.next(1 << 32) as u32;
        let tail = rng.next(1 << 32) as u32;
        r.head.store(head, Ordering::Relaxed);
        r.tail.store(tail, Ordering::Relaxed);
        let room = r.room();
        assert!(room <= 32);
        let mut buf = [0u8; 100];
        let n = r.read(&mut buf);
        assert!(n <= 32);
        if tail.wrapping_sub(head) > 32 {
            assert_eq!(n, 0);
            assert_eq!(r.head.load(Ordering::Relaxed), tail, "dropped");
        }
        assert!(r.write(&[1u8; 100]) <= 32);
    }
}
