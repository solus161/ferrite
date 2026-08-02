use std::ops::Deref;

use crate::exceptions::CustomError;

#[derive(Debug)]
pub struct Buffer<T: Copy + Default, const N: usize>([T; N]);

impl<T: Copy + Default, const N: usize> Buffer<T, N> {
    pub fn new() -> Self {
        Self([T::default(); N])
    }

    pub fn write(&mut self, src: &[T]) -> Result<(), CustomError> {
        if src.len() != N {
            return Err(CustomError::MismatchInputLength);
        };
        self.0.copy_from_slice(src);
        Ok(())
    }

    /// Store a single element. Lets a producer that computes its data one
    /// element at a time fill the buffer in place, instead of accumulating in a
    /// staging array and paying a whole-buffer `copy_from_slice` afterwards.
    pub fn set(&mut self, i: usize, x: T) -> Result<(), CustomError> {
        *self.0.get_mut(i).ok_or(CustomError::InvalidIndex)? = x;
        Ok(())
    }
}

impl<T: Copy + Default, const N: usize> Deref for Buffer<T, N> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_buffer_u8() -> Result<(), CustomError> {
        let mut buffer = Buffer::<u8, 4>::new();
        buffer.write(&[1u8, 2u8, 3u8, 4u8])?;
        println!("{:?}", &buffer);
        Ok(())
        // assert_eq!(&buffer, &[1u8, 2u8, 3u8, 4u8])
    }
}
