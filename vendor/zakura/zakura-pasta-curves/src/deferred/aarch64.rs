//! Unreduced 256-by-256 multiplication accumulated into eight limbs.
//!
//! Write B = 2^64. Each row adds low product words, then high words.
//! Row i consumes the previous row's pending carry at word i + 4 and
//! leaves its own final carry pending at word i + 5. Thus no row needs
//! to propagate through the untouched upper accumulator words.
//! The highest high-product word plus the previous pending carry and the
//! low-chain carry can require 65 bits. Preserve its overflow separately,
//! then add the high-chain carry-out at the same next-word position.
//! After row i, the initial low i + 5 words plus the processed partial
//! product are below 2 * B^(i + 5), so their combined carry is 0 or 1.
//! The returned overflow is at most one because acc + lhs * rhs < 2^513.
//! All input limbs may be arbitrary u64 values; no field bound is assumed.

use core::arch::asm;

#[inline(always)]
pub(super) fn mul_accumulate(
    accumulator: [u64; 8],
    lhs: &[u64; 4],
    rhs: &[u64; 4],
) -> ([u64; 8], u64) {
    let [
        mut d0,
        mut d1,
        mut d2,
        mut d3,
        mut d4,
        mut d5,
        mut d6,
        mut d7,
    ] = accumulator;
    let overflow;
    // SAFETY: only register arithmetic, all inputs and clobbers declared.
    // There are no memory accesses or data-dependent control flow.
    unsafe {
        asm!(
            // Schoolbook row 0, starting at accumulator limb 0.
            "mul {t0}, {a0}, {b0}",
            "mul {t1}, {a0}, {b1}",
            "mul {t2}, {a0}, {b2}",
            "mul {t3}, {a0}, {b3}",
            "adds {d0}, {d0}, {t0}",
            "adcs {d1}, {d1}, {t1}",
            "adcs {d2}, {d2}, {t2}",
            "adcs {d3}, {d3}, {t3}",
            // high(a0 * b3) <= B - 2, so this carry addition fits.
            "umulh {t4}, {a0}, {b3}",
            "adc {t4}, {t4}, xzr",
            "umulh {t0}, {a0}, {b0}",
            "adds {d1}, {d1}, {t0}",
            "umulh {t0}, {a0}, {b1}",
            "adcs {d2}, {d2}, {t0}",
            "umulh {t0}, {a0}, {b2}",
            "adcs {d3}, {d3}, {t0}",
            "adcs {d4}, {d4}, {t4}",
            "adc {overflow}, xzr, xzr",
            // Schoolbook row 1, starting at accumulator limb 1.
            "mul {t0}, {a1}, {b0}",
            "mul {t1}, {a1}, {b1}",
            "mul {t2}, {a1}, {b2}",
            "mul {t3}, {a1}, {b3}",
            "adds {d1}, {d1}, {t0}",
            "adcs {d2}, {d2}, {t1}",
            "adcs {d3}, {d3}, {t2}",
            "adcs {d4}, {d4}, {t3}",
            // Both incoming carries belong at word 5. Keep the 65th
            // bit of high(a1 * b3) + pending + low_chain_carry.
            "umulh {t4}, {a1}, {b3}",
            "adcs {t4}, {t4}, {overflow}",
            "adc {overflow}, xzr, xzr",
            "umulh {t0}, {a1}, {b0}",
            "adds {d2}, {d2}, {t0}",
            "umulh {t0}, {a1}, {b1}",
            "adcs {d3}, {d3}, {t0}",
            "umulh {t0}, {a1}, {b2}",
            "adcs {d4}, {d4}, {t0}",
            "adcs {d5}, {d5}, {t4}",
            "adc {overflow}, {overflow}, xzr",
            // Schoolbook row 2, starting at accumulator limb 2.
            "mul {t0}, {a2}, {b0}",
            "mul {t1}, {a2}, {b1}",
            "mul {t2}, {a2}, {b2}",
            "mul {t3}, {a2}, {b3}",
            "adds {d2}, {d2}, {t0}",
            "adcs {d3}, {d3}, {t1}",
            "adcs {d4}, {d4}, {t2}",
            "adcs {d5}, {d5}, {t3}",
            // Both incoming carries belong at word 6. Keep the 65th
            // bit of high(a2 * b3) + pending + low_chain_carry.
            "umulh {t4}, {a2}, {b3}",
            "adcs {t4}, {t4}, {overflow}",
            "adc {overflow}, xzr, xzr",
            "umulh {t0}, {a2}, {b0}",
            "adds {d3}, {d3}, {t0}",
            "umulh {t0}, {a2}, {b1}",
            "adcs {d4}, {d4}, {t0}",
            "umulh {t0}, {a2}, {b2}",
            "adcs {d5}, {d5}, {t0}",
            "adcs {d6}, {d6}, {t4}",
            "adc {overflow}, {overflow}, xzr",
            // Schoolbook row 3, starting at accumulator limb 3.
            "mul {t0}, {a3}, {b0}",
            "mul {t1}, {a3}, {b1}",
            "mul {t2}, {a3}, {b2}",
            "mul {t3}, {a3}, {b3}",
            "adds {d3}, {d3}, {t0}",
            "adcs {d4}, {d4}, {t1}",
            "adcs {d5}, {d5}, {t2}",
            "adcs {d6}, {d6}, {t3}",
            // Both incoming carries belong at word 7. Keep the 65th
            // bit of high(a3 * b3) + pending + low_chain_carry.
            "umulh {t4}, {a3}, {b3}",
            "adcs {t4}, {t4}, {overflow}",
            "adc {overflow}, xzr, xzr",
            "umulh {t0}, {a3}, {b0}",
            "adds {d4}, {d4}, {t0}",
            "umulh {t0}, {a3}, {b1}",
            "adcs {d5}, {d5}, {t0}",
            "umulh {t0}, {a3}, {b2}",
            "adcs {d6}, {d6}, {t0}",
            "adcs {d7}, {d7}, {t4}",
            "adc {overflow}, {overflow}, xzr",
            d0 = inout(reg) d0,
            d1 = inout(reg) d1,
            d2 = inout(reg) d2,
            d3 = inout(reg) d3,
            d4 = inout(reg) d4,
            d5 = inout(reg) d5,
            d6 = inout(reg) d6,
            d7 = inout(reg) d7,
            a0 = in(reg) lhs[0],
            a1 = in(reg) lhs[1],
            a2 = in(reg) lhs[2],
            a3 = in(reg) lhs[3],
            b0 = in(reg) rhs[0],
            b1 = in(reg) rhs[1],
            b2 = in(reg) rhs[2],
            b3 = in(reg) rhs[3],
            overflow = out(reg) overflow,
            t0 = out(reg) _,
            t1 = out(reg) _,
            t2 = out(reg) _,
            t3 = out(reg) _,
            t4 = out(reg) _,
            options(pure, nomem, nostack),
        );
    }
    ([d0, d1, d2, d3, d4, d5, d6, d7], overflow)
}

/// Folds the top two accumulator words into the lower seven words.
///
/// Requires `b448 < 2^253` and `r2 < 2^252`, as for both Pasta fields.
/// Thus the folding term is less than 2^318 and fits in five words;
/// adding the original low 448 bits gives a result less than 2^449.
#[inline(always)]
pub(super) fn partial_reduce(
    limbs: [u64; 8],
    carry: u64,
    b448: &[u64; 4],
    r2: &[u64; 4],
) -> [u64; 8] {
    debug_assert!(b448[3] < (1 << 61));
    debug_assert!(r2[3] < (1 << 60));
    let [mut d0, mut d1, mut d2, mut d3, mut d4, mut d5, mut d6, b7] = limbs;
    let d7;
    // SAFETY: register-only arithmetic with all inputs and clobbers declared.
    // No memory accesses or data-dependent control flow. The bounds above
    // ensure the five-word folding term cannot overflow.
    unsafe {
        asm!(
            // Form b7 * b448. Add the high product words one position left.
            "mul {t0}, {b7}, {p0}",
            "mul {t1}, {b7}, {p1}",
            "mul {t2}, {b7}, {p2}",
            "mul {t3}, {b7}, {p3}",
            "umulh {t4}, {b7}, {p3}",
            "umulh {tmp}, {b7}, {p0}",
            "adds {t1}, {t1}, {tmp}",
            "umulh {tmp}, {b7}, {p1}",
            "adcs {t2}, {t2}, {tmp}",
            "umulh {tmp}, {b7}, {p2}",
            "adcs {t3}, {t3}, {tmp}",
            "adc {t4}, {t4}, xzr",
            // Add the low words of carry * r2, then the high words.
            "mul {tmp}, {b8}, {q0}",
            "adds {t0}, {t0}, {tmp}",
            "mul {tmp}, {b8}, {q1}",
            "adcs {t1}, {t1}, {tmp}",
            "mul {tmp}, {b8}, {q2}",
            "adcs {t2}, {t2}, {tmp}",
            "mul {tmp}, {b8}, {q3}",
            "adcs {t3}, {t3}, {tmp}",
            "adc {t4}, {t4}, xzr",
            "umulh {tmp}, {b8}, {q0}",
            "adds {t1}, {t1}, {tmp}",
            "umulh {tmp}, {b8}, {q1}",
            "adcs {t2}, {t2}, {tmp}",
            "umulh {tmp}, {b8}, {q2}",
            "adcs {t3}, {t3}, {tmp}",
            "umulh {tmp}, {b8}, {q3}",
            "adc {t4}, {t4}, {tmp}",
            // Combine the folding term with the original low seven words.
            "adds {d0}, {d0}, {t0}",
            "adcs {d1}, {d1}, {t1}",
            "adcs {d2}, {d2}, {t2}",
            "adcs {d3}, {d3}, {t3}",
            "adcs {d4}, {d4}, {t4}",
            "adcs {d5}, {d5}, xzr",
            "adcs {d6}, {d6}, xzr",
            "adc {d7}, xzr, xzr",
            d0 = inout(reg) d0,
            d1 = inout(reg) d1,
            d2 = inout(reg) d2,
            d3 = inout(reg) d3,
            d4 = inout(reg) d4,
            d5 = inout(reg) d5,
            d6 = inout(reg) d6,
            d7 = lateout(reg) d7,
            b7 = in(reg) b7,
            b8 = in(reg) carry,
            p0 = in(reg) b448[0],
            p1 = in(reg) b448[1],
            p2 = in(reg) b448[2],
            p3 = in(reg) b448[3],
            q0 = in(reg) r2[0],
            q1 = in(reg) r2[1],
            q2 = in(reg) r2[2],
            q3 = in(reg) r2[3],
            t0 = out(reg) _,
            t1 = out(reg) _,
            t2 = out(reg) _,
            t3 = out(reg) _,
            t4 = out(reg) _,
            tmp = out(reg) _,
            options(pure, nomem, nostack),
        );
    }
    [d0, d1, d2, d3, d4, d5, d6, d7]
}
