//! Reading the trainer IDs out of a cartridge save (`.sav`, 128 KiB flash).
//!
//! The bot never reads the game's memory; the save file is what a real
//! cartridge (or a Switch save backup) keeps, and the secret ID is in it and
//! nowhere on screen. Layout (`include/save.h`): two slots of 14 sectors of
//! 4 KiB, rotated within the slot; each sector ends in a footer with its id
//! (0xFF4), checksum (0xFF6), signature 0x08012025 (0xFF8) and the save
//! counter (0xFFC). Sector id 0 holds `SaveBlock2`, whose
//! `playerTrainerId` (0x0A) is the TID then the SID, little-endian.

use crate::rng::TrainerIds;

const SECTOR_SIZE: usize = 0x1000;
const SECTORS_PER_SLOT: usize = 14;
const DATA_SIZE: usize = 3968;
const SIGNATURE: u32 = 0x0801_2025;
/// `sizeof(struct SaveBlock2)` in FireRed/LeafGreen: what the checksum covers.
const SAVEBLOCK2_SIZE: usize = 0xF24;

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// The game's sector checksum: the u32 sum of the data, folded to 16 bits.
fn checksum(data: &[u8]) -> u16 {
    let sum = data
        .chunks_exact(4)
        .fold(0u32, |acc, w| acc.wrapping_add(u32_at(w, 0)));
    ((sum >> 16) as u16).wrapping_add(sum as u16)
}

/// The TID and SID of the newest valid save, or why there's none.
pub fn trainer_ids(sav: &[u8]) -> Result<TrainerIds, String> {
    if sav.len() < 2 * SECTORS_PER_SLOT * SECTOR_SIZE {
        return Err(format!("save is {} bytes, too small", sav.len()));
    }
    let mut best: Option<(u32, TrainerIds)> = None;
    for slot in 0..2 {
        for i in 0..SECTORS_PER_SLOT {
            let sector = &sav[(slot * SECTORS_PER_SLOT + i) * SECTOR_SIZE..][..SECTOR_SIZE];
            if u32_at(sector, 0xFF8) != SIGNATURE || u16_at(sector, 0xFF4) != 0 {
                continue;
            }
            if checksum(&sector[..SAVEBLOCK2_SIZE.min(DATA_SIZE)]) != u16_at(sector, 0xFF6) {
                continue;
            }
            let counter = u32_at(sector, 0xFFC);
            let ids = TrainerIds {
                tid: u16_at(sector, 0x0A),
                sid: u16_at(sector, 0x0C),
            };
            if best.is_none_or(|(c, _)| counter > c) {
                best = Some((counter, ids));
            }
        }
    }
    best.map(|(_, ids)| ids)
        .ok_or_else(|| "no valid trainer sector (has the game been saved?)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sector(sav: &mut [u8], index: usize, counter: u32, tid: u16, sid: u16) {
        let s = &mut sav[index * SECTOR_SIZE..][..SECTOR_SIZE];
        s[0x0A..0x0C].copy_from_slice(&tid.to_le_bytes());
        s[0x0C..0x0E].copy_from_slice(&sid.to_le_bytes());
        let sum = checksum(&s[..SAVEBLOCK2_SIZE]);
        s[0xFF4..0xFF6].copy_from_slice(&0u16.to_le_bytes());
        s[0xFF6..0xFF8].copy_from_slice(&sum.to_le_bytes());
        s[0xFF8..0xFFC].copy_from_slice(&SIGNATURE.to_le_bytes());
        s[0xFFC..0x1000].copy_from_slice(&counter.to_le_bytes());
    }

    #[test]
    fn reads_the_newest_slot() {
        let mut sav = vec![0xFFu8; 0x20000];
        sector(&mut sav, 3, 7, 111, 222);
        sector(&mut sav, SECTORS_PER_SLOT + 9, 8, 12345, 54321);
        assert_eq!(
            trainer_ids(&sav),
            Ok(TrainerIds {
                tid: 12345,
                sid: 54321
            })
        );
    }

    #[test]
    fn a_bad_checksum_is_skipped() {
        let mut sav = vec![0xFFu8; 0x20000];
        sector(&mut sav, 0, 1, 1, 2);
        sector(&mut sav, SECTORS_PER_SLOT, 2, 3, 4);
        sav[SECTORS_PER_SLOT * SECTOR_SIZE + 0x0A] ^= 1;
        assert_eq!(trainer_ids(&sav), Ok(TrainerIds { tid: 1, sid: 2 }));
        assert!(trainer_ids(&vec![0xFFu8; 0x20000]).is_err());
    }
}
