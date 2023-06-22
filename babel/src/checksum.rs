use sha2::Digest;

pub trait Checksum {
    fn update(&mut self, data: &[u8]);
    fn into_bytes(self) -> Vec<u8>;
}

impl Checksum for sha1_smol::Sha1 {
    fn update(&mut self, data: &[u8]) {
        self.update(data);
    }

    fn into_bytes(self) -> Vec<u8> {
        self.digest().bytes().to_vec()
    }
}

impl Checksum for sha2::Sha256 {
    fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(self, data);
    }

    fn into_bytes(self) -> Vec<u8> {
        self.finalize().to_vec()
    }
}

impl Checksum for blake3::Hasher {
    fn update(&mut self, data: &[u8]) {
        self.update(data);
    }

    fn into_bytes(self) -> Vec<u8> {
        self.finalize().as_bytes().to_vec()
    }
}
