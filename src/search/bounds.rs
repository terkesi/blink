#[derive(Clone, Copy, Debug, Default)]
pub struct Reservation {
    pub attempts: usize,
    pub bytes: usize,
}

pub fn reserve(
    used: Reservation,
    encoded_bytes: usize,
    max_attempts: usize,
    max_bytes: usize,
) -> Option<Reservation> {
    let attempts = used.attempts.checked_add(1)?;
    let bytes = used.bytes.checked_add(encoded_bytes)?;
    (attempts <= max_attempts && bytes <= max_bytes).then_some(Reservation { attempts, bytes })
}

/// The stop reason for a reservation that failed: the attempt ceiling when no attempt remains,
/// otherwise the byte ceiling.
pub fn limit_stop(used: Reservation, max_attempts: usize) -> &'static str {
    if used.attempts >= max_attempts {
        "attempt_limit"
    } else {
        "request_byte_limit"
    }
}

pub fn valid_range(start: usize, end: usize, source_len: usize) -> bool {
    start < end && end <= source_len
}
