//! CRC-32C (Castagnoli), reflected, as used by PABotBase2: the running
//! register is seeded by the caller and there is no final XOR.

const POLY: u32 = 0x82F6_3B78;

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    data.iter().fold(seed, |crc, byte| {
        TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_standard_check_value() {
        // Standard CRC-32C("123456789") = 0xE3069283 with init and final XOR
        // of 0xFFFFFFFF; PABotBase2 omits the final XOR.
        assert_eq!(!crc32c(0xFFFF_FFFF, b"123456789"), 0xE306_9283);
    }
}
