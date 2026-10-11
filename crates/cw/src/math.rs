//! The few floating-point functions the decoder needs. With the `std` feature they
//! are the standard library's, so hfnode decodes exactly as it always has. Without
//! it they come from `libm`, which needs neither the standard library nor a heap;
//! `RUSTFLAGS="--cfg cw_force_libm"` uses `libm` with `std` too, so that the tests
//! run through the arithmetic a microcontroller build does.

#[cfg(all(feature = "std", not(cw_force_libm)))]
mod imp {
    pub fn sin_cos(x: f32) -> (f32, f32) {
        x.sin_cos()
    }

    pub fn cos(x: f32) -> f32 {
        x.cos()
    }

    pub fn log10(x: f32) -> f32 {
        x.log10()
    }

    pub fn exp(x: f32) -> f32 {
        x.exp()
    }

    pub fn ln(x: f32) -> f32 {
        x.ln()
    }

    pub fn sqrt(x: f32) -> f32 {
        x.sqrt()
    }

    pub fn sqrt_f64(x: f64) -> f64 {
        x.sqrt()
    }

    pub fn rem_euclid(x: f32, m: f32) -> f32 {
        x.rem_euclid(m)
    }
}

#[cfg(any(not(feature = "std"), cw_force_libm))]
mod imp {
    pub fn sin_cos(x: f32) -> (f32, f32) {
        libm::sincosf(x)
    }

    pub fn cos(x: f32) -> f32 {
        libm::cosf(x)
    }

    pub fn log10(x: f32) -> f32 {
        libm::log10f(x)
    }

    pub fn exp(x: f32) -> f32 {
        libm::expf(x)
    }

    pub fn ln(x: f32) -> f32 {
        libm::logf(x)
    }

    pub fn sqrt(x: f32) -> f32 {
        libm::sqrtf(x)
    }

    pub fn sqrt_f64(x: f64) -> f64 {
        libm::sqrt(x)
    }

    /// As the standard library's `f32::rem_euclid`: the remainder, moved into
    /// `0..m.abs()`.
    pub fn rem_euclid(x: f32, m: f32) -> f32 {
        let r = libm::fmodf(x, m);
        if r < 0.0 {
            r + m.abs()
        } else {
            r
        }
    }
}

pub use imp::*;
