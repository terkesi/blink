#![no_main]
#![forbid(unsafe_code)]

#[path = "../../src/search/bounds.rs"]
mod bounds;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (usize, usize, usize, usize, usize)| {
    let (attempts, bytes, size, max_attempts, max_bytes) = input;
    let next = bounds::reserve(
        bounds::Reservation { attempts, bytes },
        size,
        max_attempts,
        max_bytes,
    );
    let fits = attempts < max_attempts && bytes <= max_bytes && size <= max_bytes - bytes;
    assert_eq!(next.is_some(), fits);
    if let Some(next) = next {
        assert!(next.attempts <= max_attempts && next.bytes <= max_bytes);
        assert_eq!(next.attempts - attempts, 1);
        assert_eq!(next.bytes - bytes, size);
    }
    if bounds::valid_range(attempts, bytes, size) {
        assert!(attempts < bytes && bytes <= size);
        assert!(bytes - attempts <= size);
    }
});
