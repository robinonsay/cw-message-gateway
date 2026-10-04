//! A small, fast slice of `hfnode selftest --sweep`: complete exchanges against the
//! mock IC-7300 at a few points well inside the should-pass region, which must all
//! succeed, and one far outside it (-6 dB), which must fail gracefully: no safety
//! violation, nothing wrong (or anything at all) delivered.
//!
//! The full grid runs with `hfnode selftest --sweep`; see the README. On a slow
//! machine set `HFNODE_E2E_SCALE` lower, as for `mock_radio_e2e`.

use hfnode::selftest::{self, Cell, Keying, TrialResult};

fn scale() -> f32 {
    std::env::var("HFNODE_E2E_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(selftest::DEFAULT_SCALE)
}

fn cell(keying: Keying, wpm: f32, snr_db: Option<f32>) -> Cell {
    Cell {
        keying,
        wpm,
        snr_db,
    }
}

/// Run each cell's first trial, all at once, and print the result rows.
fn run(cells: &[Cell], rx: bool) -> Vec<TrialResult> {
    let runs: Vec<(Cell, u32)> = cells.iter().map(|c| (*c, 0)).collect();
    let results = selftest::sweep_runs(&runs, rx, scale(), runs.len(), &|_, r, out| {
        if !r.success {
            println!("{}", out.render());
        }
    });
    print!("{}", selftest::sweep_csv(&results));
    assert_eq!(results.len(), cells.len());
    results
}

#[test]
fn sweep_slice_passes_inside_the_region_and_fails_gracefully_outside() {
    let inside = [
        cell(Keying::Machine, 10.0, Some(6.0)),
        cell(Keying::Machine, 18.0, None),
        cell(Keying::Machine, 30.0, Some(10.0)),
        cell(Keying::Hand, 15.0, Some(10.0)),
        cell(Keying::Hand, 25.0, Some(20.0)),
    ];
    let outside = cell(Keying::Machine, 18.0, Some(-6.0));
    let mut cells = inside.to_vec();
    cells.push(outside);
    let results = run(&cells, false);

    for r in &results {
        assert!(
            !r.hard_failure(),
            "{}: hard failure: {} {:?}",
            r.cell,
            r.why,
            r.wrong_delivered
        );
        assert!(r.wrong_delivered.is_empty(), "{}", r.cell);
    }
    for r in results.iter().filter(|r| r.cell != outside) {
        assert!(selftest::should_pass(&r.cell), "{}", r.cell);
        assert!(r.success, "{}: {}", r.cell, r.why);
    }
    let r = results.iter().find(|r| r.cell == outside).unwrap();
    assert!(!selftest::should_pass(&r.cell));
    assert!(!r.success, "{} unexpectedly succeeded", r.cell);
    assert!(!r.delivered, "{}: delivered at -6 dB", r.cell);
    assert!(r.hard.is_empty(), "{}: {:?}", r.cell, r.hard);
    // It failed the way an operator sees it: no read-back after every try.
    assert!(r.transmissions >= 3, "{}: {r:?}", r.cell);
    assert!(selftest::verdict(&results).ok());
}

#[test]
fn sweep_slice_with_rx() {
    let results = run(&[cell(Keying::Machine, 18.0, Some(10.0))], true);
    let r = &results[0];
    assert!(r.success && !r.hard_failure(), "{}: {}", r.cell, r.why);
    assert!(r.transmissions >= 4);
}
