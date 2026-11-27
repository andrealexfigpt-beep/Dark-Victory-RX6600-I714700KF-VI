//! Synthetic rendering correctness tests for RDNA and wave-size equivalence.
//!
//! These are compile-time / unit-level checks that run on Linux. Full
//! rendering validation (lighting, shadows, reflections, resource coherence)
//! requires Windows CI with a D3D12 device and is gated behind `#[cfg(windows)]`.

#[cfg(test)]
mod tests {
    use super::super::{rdna, rdna_float};

    #[test]
    fn rdna_all_bits_is_sum_of_parts() {
        assert_eq!(rdna::ALL, rdna::MAX_MIN | rdna::LEGACY | rdna::MED3 | rdna::FRACT);
    }

    #[test]
    fn rdna_bits_are_distinct() {
        let bits = [rdna::MAX_MIN, rdna::LEGACY, rdna::MED3, rdna::FRACT];
        for i in 0..bits.len() {
            for j in (i + 1)..bits.len() {
                assert_eq!(bits[i] & bits[j], 0, "RDNA bits overlap");
            }
        }
    }
}
