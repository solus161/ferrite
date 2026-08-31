use core::ops::{Add, Mul};

/// A struct for complex number, using f32 by default
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ComplexF32 {
    pub real: f32,
    pub img: f32,
}

impl ComplexF32 {
    pub const fn new(real: f32, img: f32) -> Self {
        Self { real, img }
    }

    pub fn set(&mut self, real: f32, img: f32) {
        self.real = real;
        self.img = img;
    }

    pub fn cis(theta: f32) -> Self {
        let (img, real) = theta.sin_cos();
        Self { real, img }
    }

    pub fn norm(self) -> f32 {
        self.real.hypot(self.img)
    }

    pub fn scale(&mut self, n: f32) {
        self.real *= n;
        self.img *= n;
    }
}

impl Add for ComplexF32 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.real + rhs.real, self.img + rhs.img)
    }
}

impl Mul for ComplexF32 {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        Self::new(
            self.real * rhs.real - self.img * rhs.img,
            self.real * rhs.img + self.img * rhs.real,
        )
    }
}

/// Scaling by a real. Two multiplies where the complex product would take four
/// and two adds — worth having its own impl on a path that runs N times per
/// transform.
impl Mul<f32> for ComplexF32 {
    type Output = Self;

    fn mul(self, rhs: f32) -> Self::Output {
        Self::new(self.real * rhs, self.img * rhs)
    }
}
