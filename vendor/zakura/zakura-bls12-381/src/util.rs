/// Compute a + b + carry, returning the result and the new carry over.
#[inline(always)]
pub const fn adc(a: u64, b: u64, carry: u64) -> (u64, u64) {
    let ret = (a as u128) + (b as u128) + (carry as u128);
    (ret as u64, (ret >> 64) as u64)
}

/// Compute a - (b + borrow), returning the result and the new borrow.
#[inline(always)]
pub const fn sbb(a: u64, b: u64, borrow: u64) -> (u64, u64) {
    let ret = (a as u128).wrapping_sub((b as u128) + ((borrow >> 63) as u128));
    (ret as u64, (ret >> 64) as u64)
}

/// Compute a + (b * c) + carry, returning the result and the new carry over.
#[inline(always)]
pub const fn mac(a: u64, b: u64, c: u64, carry: u64) -> (u64, u64) {
    #[cfg(target_arch = "wasm32")]
    {
        // wasm32 has no 64x64->128 multiply, so LLVM lowers a `u128` product
        // to a `__multi3` libcall that computes a full 128x128 product. Four
        // 32x32->64 products (one `i64.mul` each) are exact here; this one
        // change removes about half of a WASM wallet scan's run time.
        let (lo, hi) = mul_wide_u32_parts(b, c);
        let (lo, c1) = lo.overflowing_add(a);
        let (lo, c2) = lo.overflowing_add(carry);
        // b * c + a + carry < 2^128, so the high word cannot overflow.
        (lo, hi + c1 as u64 + c2 as u64)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let ret = (a as u128) + ((b as u128) * (c as u128)) + (carry as u128);
        (ret as u64, (ret >> 64) as u64)
    }
}

/// Returns the low and high words of `x * y` using only 32x32->64 products.
#[cfg(any(target_arch = "wasm32", test))]
#[inline(always)]
pub const fn mul_wide_u32_parts(x: u64, y: u64) -> (u64, u64) {
    let (x0, x1) = (x & 0xffff_ffff, x >> 32);
    let (y0, y1) = (y & 0xffff_ffff, y >> 32);
    let p00 = x0 * y0;
    let p01 = x0 * y1;
    let p10 = x1 * y0;
    let p11 = x1 * y1;
    // At most 3 * (2^32 - 1), so this sum cannot overflow.
    let mid = (p00 >> 32) + (p01 & 0xffff_ffff) + (p10 & 0xffff_ffff);
    (
        (p00 & 0xffff_ffff) | (mid << 32),
        p11 + (p01 >> 32) + (p10 >> 32) + (mid >> 32),
    )
}

macro_rules! impl_add_binop_specify_output {
    ($lhs:ident, $rhs:ident, $output:ident) => {
        impl<'b> Add<&'b $rhs> for $lhs {
            type Output = $output;

            #[inline]
            fn add(self, rhs: &'b $rhs) -> $output {
                &self + rhs
            }
        }

        impl<'a> Add<$rhs> for &'a $lhs {
            type Output = $output;

            #[inline]
            fn add(self, rhs: $rhs) -> $output {
                self + &rhs
            }
        }

        impl Add<$rhs> for $lhs {
            type Output = $output;

            #[inline]
            fn add(self, rhs: $rhs) -> $output {
                &self + &rhs
            }
        }
    };
}

macro_rules! impl_sub_binop_specify_output {
    ($lhs:ident, $rhs:ident, $output:ident) => {
        impl<'b> Sub<&'b $rhs> for $lhs {
            type Output = $output;

            #[inline]
            fn sub(self, rhs: &'b $rhs) -> $output {
                &self - rhs
            }
        }

        impl<'a> Sub<$rhs> for &'a $lhs {
            type Output = $output;

            #[inline]
            fn sub(self, rhs: $rhs) -> $output {
                self - &rhs
            }
        }

        impl Sub<$rhs> for $lhs {
            type Output = $output;

            #[inline]
            fn sub(self, rhs: $rhs) -> $output {
                &self - &rhs
            }
        }
    };
}

macro_rules! impl_binops_additive_specify_output {
    ($lhs:ident, $rhs:ident, $output:ident) => {
        impl_add_binop_specify_output!($lhs, $rhs, $output);
        impl_sub_binop_specify_output!($lhs, $rhs, $output);
    };
}

macro_rules! impl_binops_multiplicative_mixed {
    ($lhs:ident, $rhs:ident, $output:ident) => {
        impl<'b> Mul<&'b $rhs> for $lhs {
            type Output = $output;

            #[inline]
            fn mul(self, rhs: &'b $rhs) -> $output {
                &self * rhs
            }
        }

        impl<'a> Mul<$rhs> for &'a $lhs {
            type Output = $output;

            #[inline]
            fn mul(self, rhs: $rhs) -> $output {
                self * &rhs
            }
        }

        impl Mul<$rhs> for $lhs {
            type Output = $output;

            #[inline]
            fn mul(self, rhs: $rhs) -> $output {
                &self * &rhs
            }
        }
    };
}

macro_rules! impl_binops_additive {
    ($lhs:ident, $rhs:ident) => {
        impl_binops_additive_specify_output!($lhs, $rhs, $lhs);

        impl SubAssign<$rhs> for $lhs {
            #[inline]
            fn sub_assign(&mut self, rhs: $rhs) {
                *self = &*self - &rhs;
            }
        }

        impl AddAssign<$rhs> for $lhs {
            #[inline]
            fn add_assign(&mut self, rhs: $rhs) {
                *self = &*self + &rhs;
            }
        }

        impl<'b> SubAssign<&'b $rhs> for $lhs {
            #[inline]
            fn sub_assign(&mut self, rhs: &'b $rhs) {
                *self = &*self - rhs;
            }
        }

        impl<'b> AddAssign<&'b $rhs> for $lhs {
            #[inline]
            fn add_assign(&mut self, rhs: &'b $rhs) {
                *self = &*self + rhs;
            }
        }
    };
}

macro_rules! impl_binops_multiplicative {
    ($lhs:ident, $rhs:ident) => {
        impl_binops_multiplicative_mixed!($lhs, $rhs, $lhs);

        impl MulAssign<$rhs> for $lhs {
            #[inline]
            fn mul_assign(&mut self, rhs: $rhs) {
                *self = &*self * &rhs;
            }
        }

        impl<'b> MulAssign<&'b $rhs> for $lhs {
            #[inline]
            fn mul_assign(&mut self, rhs: &'b $rhs) {
                *self = &*self * rhs;
            }
        }
    };
}

#[cfg(test)]
mod wasm32_mac_tests {
    use super::mul_wide_u32_parts;

    #[test]
    fn split_product_matches_u128() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let edges = [0, 1, 2, 0xffff_ffff, 0x1_0000_0000, u64::MAX - 1, u64::MAX];
        for &a in &edges {
            for &b in &edges {
                let p = (a as u128) * (b as u128);
                assert_eq!(mul_wide_u32_parts(a, b), (p as u64, (p >> 64) as u64));
            }
        }
        for _ in 0..100_000 {
            // xorshift64*
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let a = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
            let b = a.rotate_left(29) ^ x;
            let p = (a as u128) * (b as u128);
            assert_eq!(mul_wide_u32_parts(a, b), (p as u64, (p >> 64) as u64));
        }
    }
}
