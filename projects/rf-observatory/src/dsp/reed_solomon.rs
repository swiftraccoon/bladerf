//! Bounded error correction for shortened Reed–Solomon words over GF(256).

const FIELD_CYCLE: usize = 255;
const MAX_PARITY_BYTES: usize = 24;
const MAX_ERRORS: usize = MAX_PARITY_BYTES / 2;

pub(crate) struct ReedSolomon {
    // Duplicate the exponent cycle to index sums of logarithms without reduction.
    exp: [u8; 2 * FIELD_CYCLE],
    log: [u8; FIELD_CYCLE + 1],
    first_root: usize,
    forney_exponent: usize,
}

impl ReedSolomon {
    /// Build field tables for consecutive roots starting at `alpha^first_root`.
    ///
    /// # Panics
    /// Panics if `polynomial` fails to generate all 255 nonzero elements from
    /// `alpha = 2` or `first_root` is outside `0..255`.
    pub(crate) const fn new(polynomial: u16, first_root: usize) -> Self {
        assert!(polynomial > 0xff && polynomial <= 0x1ff);
        assert!(first_root < FIELD_CYCLE);
        let mut exp = [0; 2 * FIELD_CYCLE];
        let mut log = [0; FIELD_CYCLE + 1];
        let mut value = 1_u16;
        let mut exponent = 0;
        while exponent < FIELD_CYCLE {
            assert!(value != 0 && (exponent == 0 || value != 1));
            exp[exponent] = value as u8;
            exp[exponent + FIELD_CYCLE] = value as u8;
            log[value as usize] = exponent as u8;
            value <<= 1;
            if value > 0xff {
                value ^= polynomial;
            }
            exponent += 1;
        }
        assert!(value == 1);
        Self {
            exp,
            log,
            first_root,
            forney_exponent: (FIELD_CYCLE + 1 - first_root) % FIELD_CYCLE,
        }
    }

    /// Correct a low-degree-first word with its high-degree shortening zeros omitted.
    ///
    /// The supported word has at most 255 bytes and 2–24 parity bytes, with
    /// at least one data byte. Returns the number of corrected symbols after
    /// every syndrome check passes. A rejected correction leaves the word intact.
    pub(crate) fn correct(&self, word: &mut [u8], parity_bytes: usize) -> Option<usize> {
        if !(2..=MAX_PARITY_BYTES).contains(&parity_bytes)
            || !(parity_bytes + 1..=FIELD_CYCLE).contains(&word.len())
        {
            return None;
        }
        let observed = self.syndromes(word, parity_bytes);
        if observed == [0; MAX_PARITY_BYTES] {
            return Some(0);
        }
        let (locator, degree) = self.error_locator(&observed[..parity_bytes])?;
        let evaluator: [u8; MAX_ERRORS] = std::array::from_fn(|power| {
            (0..=power).fold(0, |sum, offset| {
                sum ^ self.multiply(locator[offset], observed[power - offset])
            })
        });
        let derivative: [u8; MAX_ERRORS] = std::array::from_fn(|power| {
            if power.is_multiple_of(2) {
                locator[power + 1]
            } else {
                0
            }
        });

        let mut corrections = [(0, 0); MAX_ERRORS];
        let corrections = &mut corrections[..degree];
        let mut roots = 0;
        for position in 0..word.len() {
            let inverse = self.exp[FIELD_CYCLE - position];
            if self.evaluate(&locator[..=degree], inverse) != 0 {
                continue;
            }
            let slope = self.evaluate(&derivative[..degree], inverse);
            let numerator = self.evaluate(&evaluator[..degree], inverse);
            if slope == 0 || numerator == 0 {
                return None;
            }
            // For consecutive roots starting at b, Forney's magnitude has
            // scale X^(1-b) at the location X = alpha^position.
            let scale = self.exp[(position * self.forney_exponent) % FIELD_CYCLE];
            let magnitude = self.divide(self.multiply(scale, numerator), slope);
            *corrections.get_mut(roots)? = (position, magnitude);
            roots += 1;
        }
        // A root in one of the omitted high-degree zeros cannot be corrected.
        if roots != degree {
            return None;
        }
        for &(position, magnitude) in corrections.iter() {
            word[position] ^= magnitude;
        }
        if self.syndromes(word, parity_bytes) == [0; MAX_PARITY_BYTES] {
            return Some(roots);
        }
        for &(position, magnitude) in corrections.iter() {
            word[position] ^= magnitude;
        }
        None
    }

    fn multiply(&self, first: u8, second: u8) -> u8 {
        if first == 0 || second == 0 {
            return 0;
        }
        let first_power = usize::from(self.log[usize::from(first)]);
        let second_power = usize::from(self.log[usize::from(second)]);
        self.exp[first_power + second_power]
    }

    fn divide(&self, numerator: u8, denominator: u8) -> u8 {
        debug_assert_ne!(denominator, 0);
        if numerator == 0 {
            return 0;
        }
        let numerator_power = usize::from(self.log[usize::from(numerator)]);
        let denominator_power = usize::from(self.log[usize::from(denominator)]);
        self.exp[FIELD_CYCLE + numerator_power - denominator_power]
    }

    fn evaluate(&self, coefficients: &[u8], point: u8) -> u8 {
        coefficients.iter().rev().fold(0, |value, &coefficient| {
            self.multiply(value, point) ^ coefficient
        })
    }

    fn syndromes(&self, word: &[u8], parity_bytes: usize) -> [u8; MAX_PARITY_BYTES] {
        let mut values = [0; MAX_PARITY_BYTES];
        for (root, value) in values[..parity_bytes].iter_mut().enumerate() {
            *value = self.evaluate(word, self.exp[self.first_root + root]);
        }
        values
    }

    fn error_locator(&self, syndromes: &[u8]) -> Option<([u8; MAX_ERRORS + 1], usize)> {
        // Berlekamp–Massey finds the shortest recurrence of the syndromes.
        let mut locator = [0; MAX_ERRORS + 1];
        locator[0] = 1;
        let mut previous = locator;
        let mut degree = 0;
        let mut shift = 1;
        let mut previous_discrepancy = 1;
        for (index, &syndrome) in syndromes.iter().enumerate() {
            let mut discrepancy = syndrome;
            for (&coefficient, &earlier) in locator
                .iter()
                .skip(1)
                .take(degree)
                .zip(syndromes[..index].iter().rev())
            {
                discrepancy ^= self.multiply(coefficient, earlier);
            }
            if discrepancy == 0 {
                shift += 1;
                continue;
            }

            let saved = locator;
            let factor = self.divide(discrepancy, previous_discrepancy);
            for (coefficient, &prior) in locator.iter_mut().skip(shift).zip(&previous) {
                *coefficient ^= self.multiply(factor, prior);
            }
            if 2 * degree <= index {
                degree = index + 1 - degree;
                if degree > syndromes.len() / 2 {
                    return None;
                }
                previous = saved;
                previous_discrepancy = discrepancy;
                shift = 1;
            } else {
                shift += 1;
            }
        }
        Some((locator, degree))
    }
}
