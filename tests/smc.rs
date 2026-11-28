//! Hardware tests for SMC decoding. They compare what the sampler-side readers
//! return with the raw key dump of the machine running the tests, so they need
//! a real AppleSMC (bare-metal Apple Silicon).

use macpow::smc::SmcConnection;

/// Temperature keys of type `ioft` (48.16 fixed point) must be listed next to
/// the `flt ` ones, with the value the raw dump shows.
#[test]
fn ioft_temperature_keys_are_listed() {
    let mut smc = SmcConnection::open().expect("SMC should open on Apple Silicon");
    let dumped: Vec<(String, f32)> = smc
        .dump_all()
        .iter()
        .filter(|e| e.key.starts_with('T') && e.data_type == "ioft")
        .filter_map(|e| Some((e.key.clone(), e.decoded().parse::<f32>().ok()?)))
        .filter(|(_, celsius)| *celsius > 0.0 && *celsius < 150.0)
        .collect();
    if dumped.is_empty() {
        eprintln!("no ioft temperature keys on this machine, nothing to check");
        return;
    }

    let handle = smc.start_temp_discovery();
    smc.finish_temp_discovery(handle);
    let temps = smc.read_temperatures();

    for (key, dumped_celsius) in &dumped {
        let listed: Vec<_> = temps.iter().filter(|t| &t.key == key).collect();
        assert_eq!(listed.len(), 1, "{key} should be listed exactly once");
        // The dump and the reading are taken a moment apart.
        let celsius = listed[0].value_celsius;
        assert!(
            (celsius - dumped_celsius).abs() < 5.0,
            "{key} reads {celsius} °C but the dump showed {dumped_celsius} °C"
        );
    }
}

/// Integer keys of the dump must agree with sources outside the SMC: the
/// battery registry entry for a little-endian key, the enumeration itself for
/// the big-endian key count.
#[test]
fn dump_integers_match_independent_sources() {
    let mut smc = SmcConnection::open().expect("SMC should open on Apple Silicon");
    let entries = smc.dump_all();
    let decoded = |key: &str| entries.iter().find(|e| e.key == key).map(|e| e.decoded());

    let key_count: usize = decoded("#KEY")
        .expect("#KEY should be in the dump")
        .parse()
        .expect("#KEY should decode to a number");
    assert_eq!(key_count as u32, smc.key_count());
    // Some keys refuse to be read, so the dump can be a little shorter.
    assert!(
        entries.len() <= key_count && entries.len() > key_count / 2,
        "#KEY decodes to {key_count} but the dump has {} keys",
        entries.len()
    );

    let battery = macpow::battery::read_battery();
    match decoded("B0CT") {
        Some(cycles) if battery.present => {
            let cycles: i64 = cycles.parse().expect("B0CT should decode to a number");
            // The SMC counter was seen one cycle ahead of the registry copy
            // (242 against 241). A big-endian reading of it would be 61952.
            assert!(
                (cycles - battery.cycle_count).abs() <= 1,
                "B0CT decodes to {cycles} but the battery reports {} cycles",
                battery.cycle_count
            );
        }
        _ => eprintln!("no battery or no B0CT key, cycle count not checked"),
    }
}

/// The wireless sensor is spelt `TW0P` (upper-case W) and belongs to the
/// "Wireless" category, not to "Other".
#[test]
fn wireless_sensor_is_categorised() {
    let mut smc = SmcConnection::open().expect("SMC should open on Apple Silicon");
    let handle = smc.start_temp_discovery();
    smc.finish_temp_discovery(handle);
    let temps = smc.read_temperatures();
    match temps.iter().find(|t| t.key == "TW0P") {
        Some(sensor) => assert_eq!(sensor.category, "Wireless"),
        None => eprintln!("no TW0P sensor on this machine, nothing to check"),
    }
}
