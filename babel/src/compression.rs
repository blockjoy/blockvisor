use eyre::Result;
use std::io::Write;
use std::mem;

pub trait Coder {
    fn feed(&mut self, data: Vec<u8>) -> Result<()>;
    fn consume(&mut self) -> Result<Vec<u8>>;
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
    pub fn new() -> Result<Self> {
        Ok(Self {
            zstd: zstd::stream::write::Encoder::new(Vec::new(), 3)?,
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
        let mut encoder = ZstdEncoder::new()?;
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
        let mut encoder = ZstdEncoder::new()?;
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
