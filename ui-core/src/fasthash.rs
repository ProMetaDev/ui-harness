//! xxHash64：给「这个文件改没改」用的快速哈希。
//!
//! 为什么不用 SHA-256：这里只需要"同一内容同值、不同内容几乎不同值"，
//! 不需要抗碰撞的密码学强度。我们的 SHA-256 是纯 Rust 标量实现，实测 359 MiB/s；
//! 而隔离副本要给每个文件算哈希，94 MB 的工作区光哈希就 0.27 s。
//!
//! 审计需要 SHA-256 的地方（日志链、实验包产物）仍然用 `sha256` 模块，两者互不替代。
//!
//! 实现按 xxHash 官方规格（seed = 0），用官方向量钉住。

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(acc: u64, input: u64) -> u64 {
    let acc = acc.wrapping_add(input.wrapping_mul(P2));
    acc.rotate_left(31).wrapping_mul(P1)
}

fn merge_round(acc: u64, val: u64) -> u64 {
    let acc = acc ^ round(0, val);
    acc.wrapping_mul(P1).wrapping_add(P4)
}

fn read64(b: &[u8]) -> u64 {
    u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ])
}

fn read32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// 流式 xxHash64。可以分块喂入，结果与一次喂完一致。
pub struct Xxh64 {
    v1: u64,
    v2: u64,
    v3: u64,
    v4: u64,
    buf: [u8; 32],
    buf_len: usize,
    total: u64,
}

impl Default for Xxh64 {
    fn default() -> Xxh64 {
        Xxh64::new()
    }
}

impl Xxh64 {
    pub fn new() -> Xxh64 {
        Xxh64 {
            v1: P1.wrapping_add(P2),
            v2: P2,
            v3: 0,
            v4: 0u64.wrapping_sub(P1),
            buf: [0u8; 32],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;

        // 先把缓冲区填满
        if self.buf_len > 0 {
            let need = 32 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 32 {
                let block = self.buf;
                self.lanes(&block);
                self.buf_len = 0;
            }
        }

        // 整 32 字节块
        while data.len() >= 32 {
            self.lanes(&data[..32]);
            data = &data[32..];
        }

        // 剩下的留在缓冲里
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn lanes(&mut self, block: &[u8]) {
        self.v1 = round(self.v1, read64(&block[0..8]));
        self.v2 = round(self.v2, read64(&block[8..16]));
        self.v3 = round(self.v3, read64(&block[16..24]));
        self.v4 = round(self.v4, read64(&block[24..32]));
    }

    pub fn finish(&self) -> u64 {
        let mut h = if self.total >= 32 {
            let mut h = self.v1.rotate_left(1)
                .wrapping_add(self.v2.rotate_left(7))
                .wrapping_add(self.v3.rotate_left(12))
                .wrapping_add(self.v4.rotate_left(18));
            h = merge_round(h, self.v1);
            h = merge_round(h, self.v2);
            h = merge_round(h, self.v3);
            h = merge_round(h, self.v4);
            h
        } else {
            P5
        };

        h = h.wrapping_add(self.total);

        let tail = &self.buf[..self.buf_len];
        let mut i = 0usize;
        while i + 8 <= tail.len() {
            h ^= round(0, read64(&tail[i..i + 8]));
            h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
            i += 8;
        }
        if i + 4 <= tail.len() {
            h ^= (read32(&tail[i..i + 4]) as u64).wrapping_mul(P1);
            h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
            i += 4;
        }
        while i < tail.len() {
            h ^= (tail[i] as u64).wrapping_mul(P5);
            h = h.rotate_left(11).wrapping_mul(P1);
            i += 1;
        }

        h ^= h >> 33;
        h = h.wrapping_mul(P2);
        h ^= h >> 29;
        h = h.wrapping_mul(P3);
        h ^= h >> 32;
        h
    }
}

/// 一次性算一段数据的 xxHash64。
pub fn hash(data: &[u8]) -> u64 {
    let mut h = Xxh64::new();
    h.update(data);
    h.finish()
}

/// 16 位十六进制（固定长度，便于 manifest 里按长度区分算法）。
pub fn hex(data: &[u8]) -> String {
    format!("{:016x}", hash(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_official_vectors() {
        // xxHash 官方测试向量（seed = 0）
        assert_eq!(hash(b""), 0xef46_db37_51d8_e999);
        assert_eq!(hash(b"a"), 0xd24e_c4f1_a98c_6e5b);
        assert_eq!(hash(b"abc"), 0x44bc_2cf5_ad77_0999);
        assert_eq!(hash(b"hello"), 0x26c7_827d_889f_6da3);
    }

    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        for chunk in [1usize, 3, 7, 31, 32, 33, 64, 999] {
            let mut h = Xxh64::new();
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(h.finish(), hash(&data), "分块大小 {} 结果不一致", chunk);
        }
    }

    #[test]
    fn one_byte_change_changes_hash() {
        let a = hash(b"the quick brown fox");
        let b = hash(b"the quick brown fix");
        assert_ne!(a, b);
        assert_eq!(hex(b"abc").len(), 16);
    }
}
