use std::io::{self, Write};

use sha2::{Digest, Sha256};

/// A `Write` adapter that computes a SHA256 digest of all bytes passing through.
pub(crate) struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes_written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W> HashingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes_written: 0,
        }
    }

    pub(crate) fn into_parts(self) -> (W, Sha256, u64) {
        (self.inner, self.hasher, self.bytes_written)
    }
}
