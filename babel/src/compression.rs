use eyre::Result;
use std::io::Write;
use std::mem;
use std::ops::DerefMut;
use std::sync::Mutex;

/// Common interface for encoders/decoders that may be used by upload/download jobs.
/// Coder is feed with data and processed output can be consumed.
/// Not consumed data are stored in internal buffer, so remember to consume regularly, to avoid
/// buffer overflow. Once coder wes fed with all data, call finalize to get trailing data.
pub trait Coder {
    /// Feed coder with new data. Processed data are stored in internal buffer until `consume`
    /// or `finalize` is called.
    fn feed(&mut self, data: Vec<u8>) -> Result<()>;
    /// Consume next frame of processed data from buffer. May return empty vector if not fed with
    /// enough data to produce next frame.
    fn consume(&mut self) -> Result<Vec<u8>>;
    /// Flush remaining data from buffer. It should be always called after coder is fed, to make sure
    /// noting remain in internal buffer.
    fn finalize(self) -> Result<Vec<u8>>;
}

#[derive(Default)]
pub struct NoCoder {
    data: Option<Vec<u8>>,
}

impl Coder for NoCoder {
    fn feed(&mut self, data: Vec<u8>) -> Result<()> {
        self.data = Some(data);
        Ok(())
    }

    fn consume(&mut self) -> Result<Vec<u8>> {
        Ok(self.data.take().unwrap_or_default())
    }

    fn finalize(mut self) -> Result<Vec<u8>> {
        Ok(self.data.take().unwrap_or_default())
    }
}

pub struct ZstdDecoder<'a> {
    zstd: zstd::stream::write::Decoder<'a, Vec<u8>>,
}

impl ZstdDecoder<'_> {
    pub fn new() -> Result<Self> {
        Ok(Self {
            zstd: zstd::stream::write::Decoder::new(Vec::new())?,
        })
    }
}

impl Coder for ZstdDecoder<'_> {
    fn feed(&mut self, data: Vec<u8>) -> Result<()> {
        self.zstd.write_all(&data)?;
        Ok(())
    }

    fn consume(&mut self) -> Result<Vec<u8>> {
        Ok(mem::take(self.zstd.get_mut()))
    }

    fn finalize(mut self) -> Result<Vec<u8>> {
        self.zstd.flush()?;
        self.consume()
    }
}

pub struct ZstdEncoder<'a> {
    zstd: zstd::stream::write::Encoder<'a, Vec<u8>>,
}

impl ZstdEncoder<'_> {
    pub fn new(level: i32) -> Result<Self> {
        Ok(Self {
            zstd: zstd::stream::write::Encoder::new(Vec::new(), level)?,
        })
    }
}

impl Coder for ZstdEncoder<'_> {
    fn feed(&mut self, data: Vec<u8>) -> Result<()> {
        self.zstd.write_all(&data)?;
        Ok(())
    }

    fn consume(&mut self) -> Result<Vec<u8>> {
        Ok(mem::take(self.zstd.get_mut()))
    }

    fn finalize(self) -> Result<Vec<u8>> {
        Ok(self.zstd.finish()?)
    }
}

pub struct LockedZstdEncoder<'a>(Mutex<Option<ZstdEncoder<'a>>>);

impl LockedZstdEncoder<'_> {
    pub fn new(level: i32) -> Result<Self> {
        Ok(Self(Mutex::new(Some(ZstdEncoder::new(level)?))))
    }
}

impl Coder for LockedZstdEncoder<'_> {
    fn feed(&mut self, data: Vec<u8>) -> Result<()> {
        if let Some(encoder) = self.0.lock().unwrap().deref_mut() {
            encoder.feed(data)
        } else {
            Ok(())
        }
    }

    fn consume(&mut self) -> Result<Vec<u8>> {
        if let Some(encoder) = self.0.lock().unwrap().deref_mut() {
            encoder.consume()
        } else {
            Ok(Default::default())
        }
    }

    fn finalize(self) -> Result<Vec<u8>> {
        if let Some(encoder) = self.0.lock().unwrap().take() {
            encoder.finalize()
        } else {
            Ok(Default::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_coder() -> Result<()> {
        let mut no_coder = NoCoder::default();
        no_coder.feed(vec![1, 2, 3, 4, 5, 6, 7, 8])?;
        assert_eq!(vec![1, 2, 3, 4, 5, 6, 7, 8], no_coder.consume()?);
        Ok(())
    }

    #[test]
    fn test_zstd_coder_small() -> Result<()> {
        let mut encoder = ZstdEncoder::new(3)?;
        let mut decoder = ZstdDecoder::new()?;
        encoder.feed(vec![7; 1017])?;
        assert!(encoder.consume()?.is_empty());
        let compressed = encoder.finalize()?;
        assert_eq!(
            vec![40, 181, 47, 253, 0, 88, 77, 0, 0, 16, 7, 7, 1, 0, 244, 43, 128, 5],
            compressed
        );
        decoder.feed(compressed)?;
        assert!(decoder.consume()?.is_empty());
        assert_eq!(vec![7; 1017], decoder.finalize()?);
        Ok(())
    }

    #[test]
    fn test_zstd_coder_big() -> Result<()> {
        let mut encoder = ZstdEncoder::new(3)?;
        let mut decoder = ZstdDecoder::new()?;

        let mut input = vec![0u8];
        for v in 1..15 {
            input.append(&mut vec![v as u8; v * 34567]);
        }
        let mut expected = input.clone();
        let mut steps = 0;
        while input.len() > 512 {
            let mut part = input.split_off(512);
            mem::swap(&mut part, &mut input);
            encoder.feed(part)?;
            let compressed = encoder.consume()?;
            if !compressed.is_empty() {
                decoder.feed(compressed)?;
                let output = decoder.consume()?;
                if !output.is_empty() {
                    steps += 1;
                    let mut part = expected.split_off(output.len());
                    mem::swap(&mut part, &mut expected);
                    assert_eq!(part, output);
                }
            }
        }
        assert_eq!(26, steps);
        encoder.feed(input)?;
        decoder.feed(encoder.finalize()?)?;
        assert_eq!(expected, decoder.finalize()?);
        Ok(())
    }
}
