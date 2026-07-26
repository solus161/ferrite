/// Next index + 1, but wrap around to 0 if reach max index
/// size must be power of 2
pub fn next_wrapped(i: usize, size: usize) -> usize {
    let mask = size - 1;
    (i + 1) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrapped() {
        let size: usize = 16;
        assert_eq!(next_wrapped(1, size), 2usize);
        assert_eq!(next_wrapped(15, size), 0);
    }
}
