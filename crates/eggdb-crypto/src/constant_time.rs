/// Whether `left` and `right` hold the same bytes, in time that depends only on their lengths,
/// never on where they differ, so comparing a secret leaks nothing about it.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let difference = left
        .iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    std::hint::black_box(difference) == 0
}
