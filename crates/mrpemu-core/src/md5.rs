//! MD5 (RFC 1321) — the platform exposes it through `_strCom(500, data)`.

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Md5 {
    state: [u32; 4],
    #[serde(with = "serde_big_array::BigArray")]
    buffer: [u8; 64],
    buffered: usize,
    length: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

/// Little-endian u32 words over a 64-byte block.
fn words_from(block: &[u8]) -> [u32; 16] {
    let mut words = [0u32; 16];
    for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    words
}

impl Md5 {
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476],
            buffer: [0; 64],
            buffered: 0,
            length: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.buffered > 0 {
            let take = (64 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let words = words_from(&self.buffer);
                self.compress(&words);
                self.buffered = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            self.compress(&words_from(block));
            data = rest;
        }
        // Tail bytes append after any partial fill from the head branch.
        self.buffer[self.buffered..self.buffered + data.len()].copy_from_slice(data);
        self.buffered += data.len();
    }

    pub fn finish(mut self) -> [u8; 16] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffered != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_le_bytes());
        let mut digest = [0u8; 16];
        for (index, word) in self.state.iter().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        digest
    }

    fn compress(&mut self, block: &[u32; 16]) {
        let (mut a, mut b, mut c, mut d) =
            (self.state[0], self.state[1], self.state[2], self.state[3]);
        for index in 0..64 {
            let (f, gate) = match index / 16 {
                0 => ((b & c) | (!b & d), index),
                1 => ((b & d) | (c & !d), (5 * index + 1) % 16),
                2 => (b ^ c ^ d, (3 * index + 5) % 16),
                _ => (c ^ (b | !d), (7 * index) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a
                .wrapping_add(f)
                .wrapping_add(block[gate])
                .wrapping_add(Self::K[index]);
            b = b.wrapping_add(sum.rotate_left(Self::S[index]));
            a = temp;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }

    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];
}

/// One-shot MD5 digest.
pub fn md5(data: &[u8]) -> [u8; 16] {
    // Implements the MRP guest API digest; MD5 is required for compatibility.
    let mut md5 = Md5::new(); // NOSONAR
    md5.update(data);
    md5.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc1321_vectors() {
        assert_eq!(
            md5(b"")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            md5(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            md5(b"The quick brown fox jumps over the lazy dog")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }
}
