use macpow::smc::SmcConnection;

fn main() {
    let mut smc = SmcConnection::open().expect("SMC open");
    let keys = [
        "PSTR", "PBwo", "PDBR", "PDTR", "wiPm", "PUSB", "PUS0", "PUS1", "PUS2", "PU0R", "PU1R",
        "PU2R", "PUBC", "PUBD", "PUB0", "PUB1", "PPRT", "PBUS",
    ];
    for key in &keys {
        // A key this Mac does not have is skipped; `macpow --dump-smc` lists every key.
        if let Ok(v) = smc.read_f32(key) {
            println!("{}: {:>8.4} W", key, v);
        }
    }
}
