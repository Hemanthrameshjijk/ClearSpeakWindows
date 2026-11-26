//! 16-bit PCM WAV files: written whole (voice test) or streamed (call recordings, header kept up to date while
//! writing so a file cut off by a crash or power loss still plays up to that point).

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// The 44-byte header of a 16-bit PCM WAV with `data_len` bytes of samples.
pub fn header(rate: u32, channels: u16, data_len: u32) -> [u8; 44] {
    let mut b = [0u8; 44];
    let block = channels as u32 * 2;
    b[0..4].copy_from_slice(b"RIFF");
    b[4..8].copy_from_slice(&36u32.saturating_add(data_len).to_le_bytes());
    b[8..16].copy_from_slice(b"WAVEfmt ");
    b[16..20].copy_from_slice(&16u32.to_le_bytes());
    b[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    b[22..24].copy_from_slice(&channels.to_le_bytes());
    b[24..28].copy_from_slice(&rate.to_le_bytes());
    b[28..32].copy_from_slice(&(rate * block).to_le_bytes());
    b[32..34].copy_from_slice(&(block as u16).to_le_bytes());
    b[34..36].copy_from_slice(&16u16.to_le_bytes());
    b[36..40].copy_from_slice(b"data");
    b[40..44].copy_from_slice(&data_len.to_le_bytes());
    b
}

pub fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// A whole mono file at once.
pub fn write_mono(path: &Path, rate: u32, x: &[f32]) -> std::io::Result<()> {
    let mut b = Vec::with_capacity(44 + x.len() * 2);
    b.extend_from_slice(&header(rate, 1, (x.len() * 2) as u32));
    for v in x {
        b.extend_from_slice(&to_i16(*v).to_le_bytes());
    }
    std::fs::write(path, b)
}

/// A file written a little at a time.
pub struct WavWriter {
    out: BufWriter<File>,
    rate: u32,
    channels: u16,
    data_len: u32,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32, channels: u16) -> std::io::Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);
        out.write_all(&header(rate, channels, 0))?;
        Ok(Self { out, rate, channels, data_len: 0 })
    }

    /// Interleaved samples (all channels of one moment, then the next).
    pub fn write(&mut self, samples: &[i16]) -> std::io::Result<()> {
        let mut b = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        self.out.write_all(&b)?;
        self.data_len = self.data_len.saturating_add(b.len() as u32);
        Ok(())
    }

    /// Writes what is buffered and the sizes so far into the header: the file plays up to here.
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.out.flush()?;
        let f = self.out.get_mut();
        let end = f.stream_position()?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header(self.rate, self.channels, self.data_len))?;
        f.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    pub fn seconds(&self) -> u32 {
        self.data_len / (self.rate * self.channels as u32 * 2)
    }

    pub fn bytes(&self) -> u64 {
        44 + self.data_len as u64
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streamed_file_plays_before_and_after_finish() {
        let p = std::env::temp_dir().join(format!("clearspeak-wav-{}.wav", std::process::id()));
        let mut w = WavWriter::create(&p, 16_000, 2).unwrap();
        w.write(&[1, -1, 2, -2]).unwrap();
        w.sync().unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u16::from_le_bytes([b[22], b[23]]), 2, "stereo");
        assert_eq!(u32::from_le_bytes([b[24], b[25], b[26], b[27]]), 16_000);
        assert_eq!(u32::from_le_bytes([b[40], b[41], b[42], b[43]]), 8, "header says what is written so far");
        assert_eq!(b.len(), 44 + 8);
        w.write(&vec![0; 16_000 * 2]).unwrap();
        assert_eq!(w.seconds(), 1);
        w.finish().unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(u32::from_le_bytes([b[40], b[41], b[42], b[43]]) as usize, b.len() - 44);
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize, b.len() - 8);
        let _ = std::fs::remove_file(&p);
    }
}
