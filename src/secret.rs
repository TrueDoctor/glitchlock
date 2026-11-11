use std::sync::atomic::{Ordering, compiler_fence};

const CAPACITY: usize = 1024;

/// A password buffer that never reallocates and is zeroed when cleared or dropped.
pub struct Secret {
    bytes: Vec<u8>,
}

impl Secret {
    pub fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(CAPACITY),
        }
    }

    /// Appends a character, returning `false` if the buffer is full.
    pub fn push(&mut self, character: char) -> bool {
        let mut encoded = [0; 4];
        let length = character.encode_utf8(&mut encoded).len();
        let fits = self.bytes.len() + length <= CAPACITY;
        if fits && let Some(encoded) = encoded.get(..length) {
            self.bytes.extend_from_slice(encoded);
        }
        zero(&mut encoded);
        fits
    }

    /// Removes the last character, returning `false` if the buffer was empty.
    pub fn pop(&mut self) -> bool {
        let is_char_start = |byte: &u8| byte & 0b1100_0000 != 0b1000_0000;
        let Some(start) = self.bytes.iter().rposition(is_char_start) else {
            return false;
        };
        if let Some(removed) = self.bytes.get_mut(start..) {
            zero(removed);
        }
        self.bytes.truncate(start);
        true
    }

    pub fn clear(&mut self) {
        zero(&mut self.bytes);
        self.bytes.clear();
    }

    /// Moves the contents out, leaving this buffer empty.
    pub fn take(&mut self) -> Secret {
        std::mem::replace(self, Secret::new())
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Overwrites bytes in a way the compiler cannot optimize away.
pub fn zero(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is a valid, exclusive reference.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_removes_whole_multibyte_characters() {
        let mut secret = Secret::new();
        for character in "aä€🐿".chars() {
            assert!(secret.push(character));
        }

        assert!(secret.pop());
        assert_eq!(secret.as_bytes(), "aä€".as_bytes());
        assert!(secret.pop());
        assert!(secret.pop());
        assert!(secret.pop());
        assert!(secret.is_empty());
        assert!(!secret.pop());
    }

    #[test]
    fn push_stops_at_capacity() {
        let mut secret = Secret::new();
        for _ in 0..CAPACITY {
            assert!(secret.push('x'));
        }

        assert!(!secret.push('x'));
        assert_eq!(secret.as_bytes().len(), CAPACITY);
    }

    #[test]
    fn take_leaves_an_empty_buffer() {
        let mut secret = Secret::new();
        secret.push('x');

        let taken = secret.take();

        assert!(secret.is_empty());
        assert_eq!(taken.as_bytes(), b"x");
    }
}
