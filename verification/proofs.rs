#![forbid(unsafe_code)]

#[path = "../src/search/bounds.rs"]
mod bounds;

#[kani::proof]
fn request_reservations_are_exact_and_bounded() {
    let used = bounds::Reservation {
        attempts: kani::any(),
        bytes: kani::any(),
    };
    let encoded_bytes: usize = kani::any();
    let max_attempts: usize = kani::any();
    let max_bytes: usize = kani::any();
    let next = bounds::reserve(used, encoded_bytes, max_attempts, max_bytes);
    let fits = used.attempts < max_attempts
        && used.bytes <= max_bytes
        && encoded_bytes <= max_bytes - used.bytes;
    assert_eq!(next.is_some(), fits);
    if let Some(next) = next {
        assert!(next.attempts <= max_attempts && next.bytes <= max_bytes);
        assert_eq!(next.attempts - used.attempts, 1);
        assert_eq!(next.bytes - used.bytes, encoded_bytes);
    }
}

#[kani::proof]
fn accepted_ranges_stay_inside_the_source() {
    let start: usize = kani::any();
    let end: usize = kani::any();
    let source_len: usize = kani::any();
    let accepted = bounds::valid_range(start, end, source_len);
    assert_eq!(accepted, start < end && end <= source_len);
    if accepted {
        assert!(end - start <= source_len);
        assert!(start < source_len);
    }
}
