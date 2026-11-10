pub const SIZE: u32 = 32;
pub const STANDBY: [u8; 4] = [135, 157, 185, 255];

pub fn waveform(color: [u8; 4]) -> Vec<u8> {
    let size = SIZE as usize;
    let mut pixels = vec![0; size * size * 4];
    for (bar, height) in [5, 12, 20, 12, 5].into_iter().enumerate() {
        for y in (size - height) / 2..(size + height) / 2 {
            for x in 4 + bar * 5..7 + bar * 5 {
                let at = (y * size + x) * 4;
                pixels[at..at + 4].copy_from_slice(&color);
            }
        }
    }
    pixels
}
