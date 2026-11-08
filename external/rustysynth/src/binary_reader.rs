#![allow(dead_code)]

use std::io;
use std::io::ErrorKind;
use std::io::Read;
use std::str;

use crate::four_cc::FourCC;

#[allow(unused)]
#[non_exhaustive]
pub(crate) struct BinaryReader {}

impl BinaryReader {
    pub(crate) fn read_i8<R: Read>(reader: &mut R) -> Result<i8, io::Error> {
        let mut data: [u8; 1] = [0; 1];
        reader.read_exact(&mut data)?;
        Ok(i8::from_le_bytes(data))
    }

    pub(crate) fn read_u8<R: Read>(reader: &mut R) -> Result<u8, io::Error> {
        let mut data: [u8; 1] = [0; 1];
        reader.read_exact(&mut data)?;
        Ok(u8::from_le_bytes(data))
    }

    pub(crate) fn read_i16<R: Read>(reader: &mut R) -> Result<i16, io::Error> {
        let mut data: [u8; 2] = [0; 2];
        reader.read_exact(&mut data)?;
        Ok(i16::from_le_bytes(data))
    }

    pub(crate) fn read_u16<R: Read>(reader: &mut R) -> Result<u16, io::Error> {
        let mut data: [u8; 2] = [0; 2];
        reader.read_exact(&mut data)?;
        Ok(u16::from_le_bytes(data))
    }

    pub(crate) fn read_i32<R: Read>(reader: &mut R) -> Result<i32, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(i32::from_le_bytes(data))
    }

    pub(crate) fn read_u32<R: Read>(reader: &mut R) -> Result<u32, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(u32::from_le_bytes(data))
    }

    pub(crate) fn read_i16_big_endian<R: Read>(reader: &mut R) -> Result<i16, io::Error> {
        let mut data: [u8; 2] = [0; 2];
        reader.read_exact(&mut data)?;
        Ok(i16::from_be_bytes(data))
    }

    pub(crate) fn read_i32_big_endian<R: Read>(reader: &mut R) -> Result<i32, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(i32::from_be_bytes(data))
    }

    pub(crate) fn read_i32_variable_length<R: Read>(reader: &mut R) -> Result<i32, io::Error> {
        let mut acc: i32 = 0;
        let mut count: i32 = 0;

        loop {
            let value = BinaryReader::read_u8(reader)? as i32;
            acc = (acc << 7) | (value & 127);
            if (value & 128) == 0 {
                break;
            }
            count += 1;
            if count == 4 {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "the length of the value must be equal to or less than 4",
                ));
            }
        }

        Ok(acc)
    }

    pub(crate) fn read_four_cc<R: Read>(reader: &mut R) -> Result<FourCC, io::Error> {
        let mut data: [u8; 4] = [0; 4];
        reader.read_exact(&mut data)?;
        Ok(FourCC::from_bytes(data))
    }

    /// Reads exactly `size` bytes.
    ///
    /// Futureboard: `size` comes straight from a chunk header, so it cannot be
    /// trusted as an allocation size. The buffer grows only as data actually
    /// arrives, and a stream that ends early fails with `UnexpectedEof`
    /// instead of first allocating whatever the header claimed.
    pub(crate) fn read_bytes<R: Read>(reader: &mut R, size: usize) -> Result<Vec<u8>, io::Error> {
        let mut data = Vec::new();
        let read = reader.by_ref().take(size as u64).read_to_end(&mut data)?;
        if read != size {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the chunk is larger than the data that follows it",
            ));
        }
        Ok(data)
    }

    pub(crate) fn read_fixed_length_string<R: Read>(
        reader: &mut R,
        length: usize,
    ) -> Result<String, io::Error> {
        let mut data = BinaryReader::read_bytes(reader, length)?;

        let mut actual_length: usize = 0;
        for value in &mut data {
            if *value == 0 {
                break;
            }
            actual_length += 1;
        }

        // Replace non-ASCII characters with '?'.
        // Tabs and returns are preserved.
        for value in &mut data[0..actual_length] {
            if !(9..=126).contains(value) {
                *value = 63; // '?'
            }
        }

        Ok(str::from_utf8(&data[0..actual_length]).unwrap().to_string())
    }

    pub(crate) fn discard_data<R: Read>(reader: &mut R, size: usize) -> Result<(), io::Error> {
        // Futureboard: skip through a fixed buffer rather than allocating the
        // declared size (see `read_bytes`).
        let skipped = io::copy(&mut reader.by_ref().take(size as u64), &mut io::sink())?;
        if skipped != size as u64 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the chunk is larger than the data that follows it",
            ));
        }
        Ok(())
    }

    pub(crate) fn read_wave_data<R: Read>(
        reader: &mut R,
        size: usize,
    ) -> Result<Vec<i16>, io::Error> {
        // SF2 `smpl` data is little-endian 16-bit PCM. Read as bytes and
        // convert explicitly so big-endian hosts decode the same as x86/ARM LE.
        //
        // Futureboard: `size` is the chunk header's claim, not what the file
        // holds. Read in fixed blocks and grow the sample buffer geometrically,
        // never past the declared size, so a truncated file fails with
        // `UnexpectedEof` after allocating about what it actually contained
        // rather than up to 4 GiB up front.
        const BLOCK_BYTES: usize = 64 * 1024;
        let sample_count = size / 2;
        let mut samples: Vec<i16> = Vec::new();
        let mut block = vec![0_u8; BLOCK_BYTES.min(sample_count * 2)];
        while samples.len() < sample_count {
            let remaining = sample_count - samples.len();
            let take = remaining.min(BLOCK_BYTES / 2);
            let bytes = &mut block[..take * 2];
            reader.read_exact(bytes)?;
            if samples.capacity() - samples.len() < take {
                samples.reserve_exact(samples.len().max(take).min(remaining));
            }
            samples.extend(
                bytes
                    .chunks_exact(2)
                    .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]])),
            );
        }
        // An odd trailing byte is not a sample, but it is part of the chunk.
        if size % 2 == 1 {
            BinaryReader::read_u8(reader)?;
        }
        Ok(samples)
    }
}
