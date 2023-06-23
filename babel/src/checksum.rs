use sha2::Digest;
use std::fmt::Debug;

// TODO use FixedSize of return type instead of Vec
pub trait Checksum {
    type Bytes: Debug + PartialEq;

    fn update(&mut self, data: &[u8]);
    fn into_bytes(self) -> Self::Bytes;
}

impl Checksum for sha1_smol::Sha1 {
    type Bytes = [u8; 20];

    fn update(&mut self, data: &[u8]) {
        self.update(data);
    }

    fn into_bytes(self) -> Self::Bytes {
        self.digest().bytes().into()
    }
}

impl Checksum for sha2::Sha256 {
    type Bytes = [u8; 32];

    fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(self, data);
    }

    fn into_bytes(self) -> Self::Bytes {
        self.finalize().into()
    }
}

impl Checksum for blake3::Hasher {
    type Bytes = [u8; 32];

    fn update(&mut self, data: &[u8]) {
        self.update(data);
    }

    fn into_bytes(self) -> Self::Bytes {
        self.finalize().into()
    }
}
