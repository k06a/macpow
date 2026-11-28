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
