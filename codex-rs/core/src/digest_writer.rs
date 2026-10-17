//! Streams serialized bytes into a hash; RustCrypto digest 0.11 hashers do not implement `Write`.

use sha2::Digest;

/// Feeds every written byte to the wrapped hasher.
pub(crate) struct DigestWriter<'a, D>(pub(crate) &'a mut D);

impl<D: Digest> std::io::Write for DigestWriter<'_, D> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
