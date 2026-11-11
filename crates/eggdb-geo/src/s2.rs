const MAX_LEVEL: u32 = 30;
const LOOKUP_BITS: u32 = 4;
const SWAP_MASK: usize = 1;
const INVERT_MASK: usize = 2;
const POS_TO_IJ: [[usize; 4]; 4] = [[0, 1, 3, 2], [0, 2, 3, 1], [3, 2, 0, 1], [3, 1, 0, 2]];
const POS_TO_ORIENTATION: [usize; 4] = [SWAP_MASK, 0, 0, INVERT_MASK | SWAP_MASK];
const LOOKUP_SIZE: usize = 1 << (2 * LOOKUP_BITS + 2);

fn init_lookup_cell(
    table: &mut [u16; LOOKUP_SIZE],
    level: u32,
    i: usize,
    j: usize,
    original_orientation: usize,
    pos: usize,
    orientation: usize,
) {
    if level == LOOKUP_BITS {
        let ij = (i << LOOKUP_BITS) + j;
        table[(ij << 2) + original_orientation] = ((pos << 2) + orientation) as u16;
        return;
    }
    let order = POS_TO_IJ[orientation];
    for (quadrant, &corner) in order.iter().enumerate() {
        init_lookup_cell(
            table,
            level + 1,
            (i << 1) + (corner >> 1),
            (j << 1) + (corner & 1),
            original_orientation,
            (pos << 2) + quadrant,
            orientation ^ POS_TO_ORIENTATION[quadrant],
        );
    }
}

fn lookup_positions() -> [u16; LOOKUP_SIZE] {
    let mut table = [0u16; LOOKUP_SIZE];
    for orientation in [0, SWAP_MASK, INVERT_MASK, SWAP_MASK | INVERT_MASK] {
        init_lookup_cell(&mut table, 0, 0, 0, orientation, 0, orientation);
    }
    table
}

fn uv_to_st(u: f64) -> f64 {
    if u >= 0.0 {
        0.5 * (1.0 + 3.0 * u).sqrt()
    } else {
        1.0 - 0.5 * (1.0 - 3.0 * u).sqrt()
    }
}

fn st_to_ij(s: f64) -> usize {
    let cells = f64::from(1u32 << MAX_LEVEL);
    ((s * cells).floor().max(0.0) as usize).min((1usize << MAX_LEVEL) - 1)
}

fn face_uv(x: f64, y: f64, z: f64) -> (u64, f64, f64) {
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let mut face = if ax >= ay && ax >= az {
        0
    } else if ay >= az {
        1
    } else {
        2
    };
    let component = [x, y, z][face as usize];
    if component < 0.0 {
        face += 3;
    }
    let (u, v) = match face {
        0 => (y / x, z / x),
        1 => (-x / y, z / y),
        2 => (-x / z, -y / z),
        3 => (z / x, y / x),
        4 => (z / y, -x / y),
        _ => (-y / z, -x / z),
    };
    (face, u, v)
}

/// The S2 cell id of the cell at `level` (0 to 30) holding the point at `latitude` and
/// `longitude` in degrees, as the S2 geometry library numbers cells: the cube face in the top
/// three bits, then the position along the face's Hilbert curve, then a trailing 1 bit.
pub fn s2_cell_id(latitude: f64, longitude: f64, level: u32) -> u64 {
    let (lat, lng) = (latitude.to_radians(), longitude.to_radians());
    let (x, y, z) = (lat.cos() * lng.cos(), lat.cos() * lng.sin(), lat.sin());
    let (face, u, v) = face_uv(x, y, z);
    let i = st_to_ij(uv_to_st(u));
    let j = st_to_ij(uv_to_st(v));
    let table = lookup_positions();
    let mut n: u64 = face << 60;
    let mut bits = (face as usize) & SWAP_MASK;
    let mask = (1usize << LOOKUP_BITS) - 1;
    for k in (0..8).rev() {
        bits += ((i >> (k * LOOKUP_BITS)) & mask) << (LOOKUP_BITS + 2);
        bits += ((j >> (k * LOOKUP_BITS)) & mask) << 2;
        bits = usize::from(table[bits]);
        n |= ((bits >> 2) as u64) << (k * 2 * LOOKUP_BITS);
        bits &= SWAP_MASK | INVERT_MASK;
    }
    let leaf = n * 2 + 1;
    let level = level.min(MAX_LEVEL);
    let lowest = 1u64 << (2 * (MAX_LEVEL - level));
    (leaf & lowest.wrapping_neg()) | lowest
}

/// A cell id's token: its hexadecimal digits without the trailing zeros, or `X` for 0.
pub fn s2_token(id: u64) -> String {
    if id == 0 {
        return "X".to_owned();
    }
    format!("{id:016x}").trim_end_matches('0').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_follow_the_faces_and_the_hilbert_curve() {
        assert_eq!(s2_cell_id(0.0, 0.0, 30), 0x1000_0000_0000_0001);
        assert_eq!(s2_cell_id(0.0, 0.0, 0), 0x1000_0000_0000_0000);
        assert_eq!(s2_token(0x1000_0000_0000_0000), "1");
        assert_eq!(s2_token(0), "X");
        for (latitude, longitude, face) in [(0.0, 90.0, 1), (89.0, 0.0, 2), (0.0, 180.0, 3), (0.0, -90.0, 4), (-89.0, 0.0, 5)] {
            assert_eq!(s2_cell_id(latitude, longitude, 30) >> 61, face, "{latitude} {longitude}");
        }
        let leaf = s2_cell_id(48.8566, 2.3522, 30);
        for level in 0..30 {
            let parent = s2_cell_id(48.8566, 2.3522, level);
            let lowest = parent & parent.wrapping_neg();
            assert_eq!(lowest, 1u64 << (2 * (30 - level)));
            assert!(parent - (lowest - 1) <= leaf && leaf <= parent + (lowest - 1), "{level}");
        }
    }
}
