//! Pure-synthetic integration test for `bris_nav::cold_start_fix`.
//!
//! Engine-side wiring (Stage E fallback) is deferred to a follow-up
//! PR; see `docs/design/circle_of_position.md`, section "Engine
//! integration". This test exercises the solver directly with
//! hand-crafted `CircleOfPosition` records and asserts the no-AP
//! cold-start behaviour: two distinct GPs → two-candidate
//! ambiguity; three diverse GPs → single fix near truth.

use bris_nav::{cold_start_fix, CircleOfPosition, ColdStartConfig, ColdStartResult};

fn latlon_to_xyz(lat: f64, lon: f64) -> [f64; 3] {
    let cl = lat.cos();
    [cl * lon.cos(), cl * lon.sin(), lat.sin()]
}

fn unit_angle(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let cx = a[1] * b[2] - a[2] * b[1];
    let cy = a[2] * b[0] - a[0] * b[2];
    let cz = a[0] * b[1] - a[1] * b[0];
    let s = (cx * cx + cy * cy + cz * cz).sqrt();
    s.atan2(dot)
}

fn synth(obs_lat_deg: f64, obs_lon_deg: f64, gp_lat_deg: f64, gp_lon_deg: f64) -> CircleOfPosition {
    let obs = latlon_to_xyz(obs_lat_deg.to_radians(), obs_lon_deg.to_radians());
    let gp = latlon_to_xyz(gp_lat_deg.to_radians(), gp_lon_deg.to_radians());
    CircleOfPosition {
        gp_lat_rad: gp_lat_deg.to_radians(),
        gp_lon_rad: gp_lon_deg.to_radians(),
        co_altitude_rad: unit_angle(obs, gp),
        sigma_rad: (0.5_f64 / 60.0).to_radians(),
    }
}

#[test]
fn three_distinct_bodies_yield_single_fix_near_truth() {
    let obs_lat = 12.5;
    let obs_lon = -78.25;
    let circles = [
        synth(obs_lat, obs_lon, 40.0, -100.0),
        synth(obs_lat, obs_lon, -10.0, -50.0),
        synth(obs_lat, obs_lon, 30.0, -150.0),
    ];
    let res = cold_start_fix(&circles, &ColdStartConfig::default()).expect("cold-start");
    match res {
        ColdStartResult::Fix(f) => {
            let truth = latlon_to_xyz(obs_lat.to_radians(), obs_lon.to_radians());
            let got = latlon_to_xyz(f.lat.radians(), f.lon.radians());
            let nm_per_rad = 180.0 * 60.0 / std::f64::consts::PI;
            let d_nm = unit_angle(truth, got) * nm_per_rad;
            assert!(d_nm < 0.5, "fix {d_nm} nm from truth");
            assert_eq!(f.sight_count, 3);
        }
        other => panic!("expected Fix, got {other:?}"),
    }
}

#[test]
fn two_distinct_bodies_yield_two_candidates() {
    let obs_lat = 12.5;
    let obs_lon = -78.25;
    let circles = [
        synth(obs_lat, obs_lon, 40.0, -100.0),
        synth(obs_lat, obs_lon, -10.0, -50.0),
    ];
    let res = cold_start_fix(&circles, &ColdStartConfig::default()).expect("cold-start");
    match res {
        ColdStartResult::TwoCandidates {
            primary,
            secondary,
            separation_great_circle_nm,
        } => {
            assert!(separation_great_circle_nm > 100.0);
            // One of the two should be near truth.
            let nm_per_rad = 180.0 * 60.0 / std::f64::consts::PI;
            let truth = latlon_to_xyz(obs_lat.to_radians(), obs_lon.to_radians());
            let dp = unit_angle(
                truth,
                latlon_to_xyz(primary.lat.radians(), primary.lon.radians()),
            ) * nm_per_rad;
            let ds = unit_angle(
                truth,
                latlon_to_xyz(secondary.lat.radians(), secondary.lon.radians()),
            ) * nm_per_rad;
            assert!(dp.min(ds) < 0.5, "best candidate {dp}, {ds} nm");
        }
        other => panic!("expected TwoCandidates, got {other:?}"),
    }
}
