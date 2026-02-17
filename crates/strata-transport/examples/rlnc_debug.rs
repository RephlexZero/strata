use bytes::Bytes;
use strata_transport::rlnc::{RlncDecoder, RlncEncoder};

mod gf256 {
    pub fn mul(a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            return 0;
        }
        let log_a = LOG_TABLE[a as usize] as u16;
        let log_b = LOG_TABLE[b as usize] as u16;
        let log_sum = (log_a + log_b) % 255;
        EXP_TABLE[log_sum as usize]
    }
    pub fn div(a: u8, b: u8) -> u8 {
        assert_ne!(b, 0);
        if a == 0 {
            return 0;
        }
        let log_a = LOG_TABLE[a as usize] as u16;
        let log_b = LOG_TABLE[b as usize] as u16;
        let log_diff = (log_a + 255 - log_b) % 255;
        EXP_TABLE[log_diff as usize]
    }
    const fn gen_tables() -> ([u8; 256], [u8; 512]) {
        let mut log = [0u8; 256];
        let mut exp = [0u8; 512];
        let mut x: u16 = 1;
        let mut i = 0usize;
        while i < 255 {
            exp[i] = x as u8;
            exp[i + 255] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
            i += 1;
        }
        log[0] = 0;
        (log, exp)
    }
    const TABLES: ([u8; 256], [u8; 512]) = gen_tables();
    const LOG_TABLE: [u8; 256] = TABLES.0;
    const EXP_TABLE: [u8; 512] = TABLES.1;
}

fn main() {
    // Replicate the failing proptest case: n=5, miss_a=0, miss_b=1, symbol_len=4, seed=11465160323165077091
    let n = 5usize;
    let symbol_len = 4usize;
    let seed: u64 = 11465160323165077091;

    let symbols: Vec<Bytes> = (0..n)
        .map(|i| {
            Bytes::from(
                (0..symbol_len)
                    .map(|j| ((i * symbol_len + j) % 256 + 99) as u8)
                    .collect::<Vec<u8>>(),
            )
        })
        .collect();

    println!("Symbols:");
    for (i, s) in symbols.iter().enumerate() {
        println!("  sym[{}] = {:?}", i, s.as_ref());
    }

    let mut enc = RlncEncoder::new(n + 2, seed);
    for (i, sym) in symbols.iter().enumerate() {
        enc.add_source(i as u64, sym.clone());
    }

    let r1 = enc.generate_repair().unwrap();
    let r2 = enc.generate_repair().unwrap();
    println!("\nr1 coeffs: {:?}", r1.coefficients);
    println!("r2 coeffs: {:?}", r2.coefficients);

    // Check if the two coefficient vectors are linearly dependent
    // when restricted to columns 0 and 1 (the unknown columns)
    let a00 = r1.coefficients[0]; // 39
    let a01 = r1.coefficients[1]; // 155
    let a10 = r2.coefficients[0]; // 247
    let a11 = r2.coefficients[1]; // 209
    println!("\n2x2 submatrix for unknowns (cols 0,1):");
    println!("  [{}, {}]", a00, a01);
    println!("  [{}, {}]", a10, a11);
    // det = a00*a11 - a01*a10 in GF(256) = a00*a11 XOR a01*a10
    let det = gf256::mul(a00, a11) ^ gf256::mul(a01, a10);
    println!("  det = {} (0 means singular!)", det);

    // Check: 247/39 vs 209/155
    let ratio0 = gf256::div(a10, a00);
    let ratio1 = gf256::div(a11, a01);
    println!(
        "  247/39 = {}, 209/155 = {} (equal means dependent)",
        ratio0, ratio1
    );

    // Now manually reduce both repair symbols by known data (syms 2,3,4)
    println!("\nManual reduction:");
    for r_idx in 0..2 {
        let r = if r_idx == 0 { &r1 } else { &r2 };
        let mut data = r.data.clone();
        #[allow(clippy::needless_range_loop)]
        for i in 2..n {
            let c = r.coefficients[i];
            for (j, &byte) in symbols[i].iter().enumerate() {
                data[j] ^= gf256::mul(c, byte);
            }
        }
        println!("  r{} reduced data: {:?}", r_idx + 1, data);
    }

    // Try actual decoder
    let mut dec = RlncDecoder::new();
    // Add known sources
    #[allow(clippy::needless_range_loop)]
    for i in 2..n {
        dec.add_source(i as u64, symbols[i].clone());
    }
    dec.add_coded(&r1);
    dec.add_coded(&r2);

    let recovered = dec.try_recover();
    println!("\nRecovered: {} symbols", recovered.len());
    for (seq, data) in &recovered {
        println!(
            "  seq={} data={:?} expected={:?}",
            seq,
            data,
            symbols[*seq as usize].as_ref()
        );
    }
}
