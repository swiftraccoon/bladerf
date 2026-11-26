//! Recover International Morse text from a 1 kHz linear-power envelope.
//!
//! Character mappings and dot-unit timing follow [ITU-R M.1677-1], Annex 1
//! Part I. This decoder interprets the supported written characters.
//!
//! [ITU-R M.1677-1]: https://www.itu.int/rec/R-REC-M.1677-1-200910-I/

use serde::Serialize;

const DIT_WPM_PRODUCT: f64 = 1_200.0;
/// Supported character speeds for fixed and automatic timing.
pub(super) const WPM_RANGE: std::ops::RangeInclusive<u32> = 8..=40;
const LEVEL_BLOCK_SAMPLES: usize = 8;
/// Minimum envelope extent for estimating keying levels.
pub(super) const MIN_ENVELOPE_SAMPLES: usize = 12 * LEVEL_BLOCK_SAMPLES;
const MIN_CONTRAST_DB: f64 = 6.0;
const DEBOUNCE_SAMPLES: usize = 6;
const MAX_CODE_ELEMENTS: usize = 8;
const MAX_CHARACTERS: usize = 512;

/// Text and timing evidence from the supplied envelope.
#[derive(Default, Serialize)]
pub(super) struct Decoded {
    text: String,
    /// Estimated speed when automatic timing selection finds a usable fit.
    estimated_wpm: Option<f64>,
    dit_samples: Option<f64>,
    threshold_power: Option<f64>,
    noise_power: Option<f64>,
    contrast_db: Option<f64>,
    /// Fraction of complete mark and gap durations accepted by the fit.
    timing_fit: Option<f64>,
    /// Number of recognized characters, including any omitted observations.
    pub(super) resolved_characters: usize,
    unresolved_characters: usize,
    omitted_characters: usize,
    characters: Vec<Character>,
}

#[derive(Serialize)]
struct Character {
    start_envelope_sample: usize,
    end_envelope_sample_exclusive: usize,
    code: String,
    symbol: Option<char>,
}

#[derive(Clone, Copy)]
struct Run {
    keyed: bool,
    start: usize,
    end: usize,
}

impl Run {
    fn units(self, dit_samples: f64) -> f64 {
        (self.end - self.start) as f64 / dit_samples
    }
}

fn level_estimates(power: &[f64]) -> Option<(f64, f64)> {
    if power.len() < MIN_ENVELOPE_SAMPLES
        || !power
            .iter()
            .all(|&sample| sample.is_finite() && sample >= 0.0)
    {
        return None;
    }
    // Block means keep isolated sample spikes out of the level estimates.
    let mut levels: Vec<_> = power
        .as_chunks::<LEVEL_BLOCK_SAMPLES>()
        .0
        .iter()
        .map(|block| {
            block
                .iter()
                .map(|&sample| sample / LEVEL_BLOCK_SAMPLES as f64)
                .sum::<f64>()
        })
        .collect();
    let last = levels.len() - 1;
    let noise = *levels.select_nth_unstable_by(last / 5, f64::total_cmp).1;
    let signal = *levels
        .select_nth_unstable_by(last * 98 / 100, f64::total_cmp)
        .1;
    Some((noise, signal))
}

fn runs(power: &[f64], threshold: f64) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut keyed = power[0] > threshold;
    let mut start = 0;
    let mut opposite = 0;
    for (index, &sample) in power.iter().enumerate().skip(1) {
        if (sample > threshold) == keyed {
            opposite = 0;
            continue;
        }
        opposite += 1;
        if opposite == DEBOUNCE_SAMPLES {
            // Confirm the transition before keeping its first envelope sample.
            let edge = index + 1 - opposite;
            runs.push(Run {
                keyed,
                start,
                end: edge,
            });
            start = edge;
            keyed = !keyed;
            opposite = 0;
        }
    }
    runs.push(Run {
        keyed,
        start,
        end: power.len(),
    });
    runs
}

fn mark(units: f64) -> Option<(char, f64)> {
    if (0.60..=1.40).contains(&units) {
        Some(('.', 1.0))
    } else if (2.30..=3.70).contains(&units) {
        Some(('-', 3.0))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum Gap {
    Element,
    Character,
    Word,
}

fn gap(units: f64) -> Option<(Gap, f64)> {
    if (0.55..=1.45).contains(&units) {
        Some((Gap::Element, 1.0))
    } else if (2.05..=3.95).contains(&units) {
        Some((Gap::Character, 3.0))
    } else if units >= 5.25 {
        Some((Gap::Word, 7.0))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct Fit {
    dit_samples: f64,
    fraction: f64,
    loss: f64,
}

fn fit_timing(runs: &[Run], dit_samples: f64) -> Option<Fit> {
    let mut examined = [0; 2];
    let mut matched = [0; 2];
    let mut loss = 0.0;
    // Leading and trailing runs can be clipped at the capture boundaries.
    for &run in runs.get(1..runs.len().checked_sub(1)?)? {
        let units = run.units(dit_samples);
        let index = usize::from(run.keyed);
        examined[index] += 1;
        let target = if run.keyed {
            mark(units).map(|(_, units)| units)
        } else {
            gap(units).map(|(_, units)| units)
        };
        if let Some(target) = target {
            matched[index] += 1;
            loss += (units / target - 1.0).abs().min(0.30);
        } else {
            loss += 1.0;
        }
    }
    if matched[0] < 2
        || matched[1] < 3
        || (0..2).any(|index| matched[index] * 5 < examined[index] * 4)
    {
        return None;
    }
    let total = examined.iter().sum::<usize>() as f64;
    Some(Fit {
        dit_samples,
        fraction: matched.iter().sum::<usize>() as f64 / total,
        loss: loss / total,
    })
}

fn choose_timing(runs: &[Run], wpm: Option<u32>) -> Option<Fit> {
    if let Some(wpm) = wpm {
        return WPM_RANGE
            .contains(&wpm)
            .then(|| fit_timing(runs, DIT_WPM_PRODUCT / f64::from(wpm)))
            .flatten();
    }
    let fits: Vec<_> = WPM_RANGE
        .filter_map(|wpm| fit_timing(runs, DIT_WPM_PRODUCT / f64::from(wpm)))
        .collect();
    let best = *fits
        .iter()
        .min_by(|left, right| left.loss.total_cmp(&right.loss))?;
    // Nearby speeds describe one timing hypothesis. A substantially different
    // rate with comparably good evidence makes the interpretation ambiguous.
    let ambiguous = fits.iter().any(|fit| {
        (fit.dit_samples / best.dit_samples - 1.0).abs() > 0.24 && fit.loss - best.loss < 0.05
    });
    (!ambiguous).then_some(best)
}

struct Pending {
    start: usize,
    end: usize,
    code: String,
    uncertain: bool,
}

impl Pending {
    fn add(&mut self, run: Run, units: f64) {
        self.end = run.end;
        let symbol = mark(units).map(|(symbol, _)| symbol);
        self.uncertain |= symbol.is_none() || self.code.len() == MAX_CODE_ELEMENTS;
        if self.code.len() < MAX_CODE_ELEMENTS {
            self.code.push(symbol.unwrap_or('?'));
        }
    }
}

impl Decoded {
    fn record(&mut self, pending: Pending, word_break: bool) {
        let symbol = (!pending.uncertain)
            .then(|| character(&pending.code))
            .flatten();
        self.resolved_characters += usize::from(symbol.is_some());
        self.unresolved_characters += usize::from(symbol.is_none());
        if self.characters.len() == MAX_CHARACTERS {
            self.omitted_characters += 1;
            return;
        }
        if word_break && !self.text.is_empty() {
            self.text.push(' ');
        }
        self.text.push(symbol.unwrap_or('\u{fffd}'));
        self.characters.push(Character {
            start_envelope_sample: pending.start,
            end_envelope_sample_exclusive: pending.end,
            code: pending.code,
            symbol,
        });
    }

    fn read_characters(&mut self, runs: &[Run], dit_samples: f64) {
        let mut pending: Option<Pending> = None;
        let mut confirmed_start = false;
        let mut word_break = false;
        for (index, &run) in runs.iter().enumerate() {
            let units = run.units(dit_samples);
            if run.keyed {
                if confirmed_start {
                    pending
                        .get_or_insert_with(|| Pending {
                            start: run.start,
                            end: run.end,
                            code: String::new(),
                            uncertain: false,
                        })
                        .add(run, units);
                }
                continue;
            }
            // An edge gap is observed only in part. Its minimum elapsed
            // duration can still establish a character boundary.
            let separation = if index == 0 || index + 1 == runs.len() {
                (units >= 2.05).then_some(Gap::Character)
            } else {
                gap(units).map(|(separation, _)| separation)
            };
            match separation {
                Some(Gap::Character | Gap::Word) => {
                    if let Some(character) = pending.take() {
                        self.record(character, word_break);
                    }
                    confirmed_start = true;
                    word_break = matches!(separation, Some(Gap::Word));
                }
                Some(Gap::Element) => {}
                None => {
                    if let Some(character) = &mut pending {
                        character.uncertain = true;
                    }
                }
            }
        }
        if !self
            .characters
            .iter()
            .any(|character| character.symbol.is_some())
        {
            self.text.clear();
        }
    }
}

/// Recover complete characters after sufficient contrast and timing evidence.
/// Character indices are zero-based, half-open intervals in the envelope.
/// Unresolved characters retain their observed code and use U+FFFD in text.
pub(super) fn decode(power: &[f64], wpm: Option<u32>) -> Decoded {
    let mut decoded = Decoded::default();
    let Some((noise, signal)) = level_estimates(power) else {
        return decoded;
    };
    decoded.noise_power = Some(noise);
    let contrast_db = if signal > 0.0 {
        -10.0 * (noise / signal).clamp(1e-12, 1.0).log10()
    } else {
        0.0
    };
    decoded.contrast_db = Some(contrast_db);
    if contrast_db < MIN_CONTRAST_DB {
        return decoded;
    }
    let threshold = noise + (signal - noise) * 0.40;
    decoded.threshold_power = Some(threshold);
    let runs = runs(power, threshold);
    let Some(fit) = choose_timing(&runs, wpm) else {
        return decoded;
    };
    decoded.dit_samples = Some(fit.dit_samples);
    decoded.timing_fit = Some(fit.fraction);
    decoded.estimated_wpm = wpm.is_none().then_some(DIT_WPM_PRODUCT / fit.dit_samples);
    decoded.read_characters(&runs, fit.dit_samples);
    decoded
}

fn character(code: &str) -> Option<char> {
    Some(match code {
        ".-" => 'A',
        "-..." => 'B',
        "-.-." => 'C',
        "-.." => 'D',
        "." => 'E',
        "..-.." => 'É',
        "..-." => 'F',
        "--." => 'G',
        "...." => 'H',
        ".." => 'I',
        ".---" => 'J',
        "-.-" => 'K',
        ".-.." => 'L',
        "--" => 'M',
        "-." => 'N',
        "---" => 'O',
        ".--." => 'P',
        "--.-" => 'Q',
        ".-." => 'R',
        "..." => 'S',
        "-" => 'T',
        "..-" => 'U',
        "...-" => 'V',
        ".--" => 'W',
        "-..-" => 'X',
        "-.--" => 'Y',
        "--.." => 'Z',
        "-----" => '0',
        ".----" => '1',
        "..---" => '2',
        "...--" => '3',
        "....-" => '4',
        "....." => '5',
        "-...." => '6',
        "--..." => '7',
        "---.." => '8',
        "----." => '9',
        ".-.-.-" => '.',
        "--..--" => ',',
        "---..." => ':',
        "..--.." => '?',
        ".----." => '\'',
        "-....-" => '-',
        "-..-." => '/',
        "-.--." => '(',
        "-.--.-" => ')',
        ".-..-." => '"',
        "-...-" => '=',
        ".-.-." => '+',
        ".--.-." => '@',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(words: &[&[&str]], dit: usize, jitter: bool) -> Vec<f64> {
        let mut power = vec![0.08; 4 * dit];
        for (word_index, word) in words.iter().enumerate() {
            if word_index > 0 {
                power.extend(std::iter::repeat_n(0.08, 7 * dit));
            }
            for (character_index, code) in word.iter().enumerate() {
                if character_index > 0 {
                    power.extend(std::iter::repeat_n(0.08, 3 * dit));
                }
                for (element, mark) in code.bytes().enumerate() {
                    if element > 0 {
                        power.extend(std::iter::repeat_n(0.08, dit));
                    }
                    let length = if mark == b'.' { dit } else { 3 * dit };
                    let adjustment = if jitter { [4, -5, 1][element % 3] } else { 0 };
                    power.extend(std::iter::repeat_n(
                        1.6,
                        length.saturating_add_signed(adjustment),
                    ));
                }
            }
        }
        power.extend(std::iter::repeat_n(0.08, 4 * dit));
        power
    }

    #[test]
    fn preserves_dits_dashes_word_spaces_and_exact_envelope_indices() {
        let decoded = decode(
            &envelope(&[&["-.-.", "--.-"], &["....."]], 60, false),
            Some(20),
        );
        assert_eq!(decoded.text, "CQ 5");
        assert_eq!(decoded.dit_samples, Some(60.0));
        assert_eq!(decoded.timing_fit, Some(1.0));
        assert_eq!(decoded.estimated_wpm, None);
        let positions: Vec<_> = decoded
            .characters
            .iter()
            .map(|character| {
                (
                    character.start_envelope_sample,
                    character.end_envelope_sample_exclusive,
                )
            })
            .collect();
        assert_eq!(positions, [(240, 900), (1080, 1860), (2280, 2820)]);
    }

    #[test]
    fn estimates_speed_across_jitter_and_ignores_short_spikes_or_dropouts() {
        let mut power = envelope(&[&["...", "---", "..."], &[".-.", ".-.", ".-."]], 57, true);
        power[20..24].fill(7.0);
        power[255..259].fill(0.0);
        let decoded = decode(&power, None);
        assert_eq!(decoded.text, "SOS RRR");
        assert!((decoded.estimated_wpm.unwrap() - 21.0).abs() <= 1.0);
    }

    #[test]
    fn abstains_on_continuous_carrier_noise_and_unconfirmed_timing() {
        assert!(decode(&[0.5; 4_000], Some(20)).text.is_empty());
        let mut state = 99u32;
        let noise: Vec<_> = (0..4_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                -((f64::from(state) + 1.0) / 4_294_967_296.0).ln()
            })
            .collect();
        assert!(decode(&noise, None).text.is_empty());
        let isolated_mark = envelope(&[&["-"]], 60, false);
        assert!(decode(&isolated_mark, Some(20)).text.is_empty());
    }

    #[test]
    fn preserves_unresolved_codes_and_withholds_a_clipped_character() {
        let power = envelope(&[&["...", "...-.-", ".--.-.", "..-.."]], 60, false);
        let decoded = decode(&power, Some(20));
        assert_eq!(decoded.text, "S\u{fffd}@É");
        assert_eq!(decoded.unresolved_characters, 1);
        assert_eq!(decoded.characters[1].code, "...-.-");
        assert_eq!(decoded.characters[1].symbol, None);
        let clipped = decode(&power[..power.len() - 230], Some(20));
        assert_eq!(clipped.text, "S\u{fffd}@");
    }
}
