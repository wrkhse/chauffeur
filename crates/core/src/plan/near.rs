//! A step read by k near needs settles to one machine code per value and set
//! of matched needs: `settled * 2^k + matched`, bit b of `matched` set when
//! need b's bars match the answer. The machine then decides each need from
//! the code alone.

use rungs::{MAX_OUTCOMES, Outcome};

use super::need::{Near, When};
use super::reply::Reply;

/// The most near needs on one step: 2^7 codes already exceed `MAX_OUTCOMES`.
pub const MAX_NEAR: u8 = 6;

/// Whether `reply`'s probability for some bar's outcome lies within the margin.
pub fn matches(near: &Near, reply: &impl Reply) -> bool {
    near.bars().iter().any(|bar| {
        reply
            .p(bar.outcome)
            .is_some_and(|p| p.abs_diff(bar.at) <= near.margin())
    })
}

/// 2^k, the sets of matched needs, for k up to `MAX_NEAR`.
fn span(k: u8) -> Option<u8> {
    if k > MAX_NEAR {
        return None;
    }

    1_u8.checked_shl(u32::from(k))
}

/// How many codes a step of `outcomes` values and `k` near needs settles to.
pub fn codes(outcomes: u8, k: u8) -> Option<u8> {
    if outcomes == 0 {
        return None;
    }

    outcomes
        .checked_mul(span(k)?)
        .filter(|codes| *codes <= MAX_OUTCOMES)
}

/// The code for value `settled` with the needs `matched` matched.
pub fn encode(settled: u8, matched: u8, k: u8) -> Option<Outcome> {
    let span = span(k)?;
    if matched >= span {
        return None;
    }

    let code = settled.checked_mul(span)?.checked_add(matched)?;
    (code < MAX_OUTCOMES).then_some(Outcome(code))
}

/// The value and matched needs of `code`.
pub fn decode(code: Outcome, k: u8) -> (u8, u8) {
    let settled = code.0.checked_shr(u32::from(k)).unwrap_or(0);
    let mask = span(k).map_or(u8::MAX, |span| span.saturating_sub(1));

    (settled, code.0 & mask)
}

/// The codes an object need accepts of a step with `outcomes` values and
/// `k` near needs: those where its own bit `near` is set, or whose value
/// is in `when`, and the failure when `when` names it.
pub fn accepts(outcomes: u8, k: u8, near: Option<u8>, when: &[When]) -> Vec<Outcome> {
    let total = codes(outcomes, k).unwrap_or(0);
    let mut set: Vec<Outcome> = (0..total)
        .map(Outcome)
        .filter(|code| {
            let (settled, matched) = decode(*code, k);
            let near = near.is_some_and(|bit| {
                matched
                    .checked_shr(u32::from(bit))
                    .is_some_and(|rest| rest & 1 == 1)
            });

            near || when.contains(&When::Outcome(settled))
        })
        .collect();

    if when.contains(&When::Failed) {
        set.push(Outcome::FAILED);
    }

    set
}

#[cfg(test)]
mod tests {
    use rungs::Outcome;

    use super::{accepts, codes, decode, encode, matches};
    use crate::plan::need::{Bar, Near, When};
    use crate::plan::reply::Reply;

    /// An answer whose probability for outcome 0 is `yes`.
    struct Yes(u16);

    impl Reply for Yes {
        fn settled(&self) -> u8 {
            0
        }

        fn p(&self, outcome: u8) -> Option<u16> {
            (outcome == 0).then_some(self.0)
        }
    }

    #[test]
    fn a_margin_holds_both_of_its_ends() {
        let near = Near::new(
            150,
            [Bar {
                outcome: 0,
                at: 700,
            }],
        );

        assert!(matches(&near, &Yes(850)));
        assert!(matches(&near, &Yes(550)));
        assert!((551..=849).all(|p| matches(&near, &Yes(p))));
        assert!(!matches(&near, &Yes(549)));
        assert!(!matches(&near, &Yes(851)));
    }

    #[test]
    fn a_bar_on_an_outcome_the_answer_lacks_never_matches() {
        let near = Near::new(
            1000,
            [Bar {
                outcome: 3,
                at: 500,
            }],
        );

        assert!(!matches(&near, &Yes(500)));
    }

    #[test]
    fn codes_fit_the_machine() {
        assert_eq!(codes(3, 0), Some(3));
        assert_eq!(codes(3, 5), Some(96));
        assert_eq!(codes(3, 6), None);
        assert_eq!(codes(65, 1), None);
        assert_eq!(codes(0, 0), None);
        assert_eq!(codes(1, 7), None);
    }

    #[test]
    fn a_code_decodes_to_what_it_encodes() {
        for (settled, matched, k) in [(0, 0, 0), (2, 0, 0), (1, 1, 1), (2, 5, 3), (1, 61, 6)] {
            let code = encode(settled, matched, k).unwrap();
            assert_eq!(decode(code, k), (settled, matched));
        }
        assert_eq!(encode(2, 5, 3), Some(Outcome(21)));
        assert_eq!(encode(0, 2, 1), None);
        assert_eq!(encode(1, 62, 6), None);
        assert_eq!(encode(2, 0, 6), None);
        assert_eq!(encode(0, 0, 7), None);
    }

    #[test]
    fn a_need_accepts_its_bit_its_values_and_failure() {
        let near = accepts(3, 1, Some(0), &[]);
        let when = accepts(3, 1, None, &[When::Outcome(2), When::Failed]);

        assert_eq!(near, [Outcome(1), Outcome(3), Outcome(5)]);
        assert_eq!(when, [Outcome(4), Outcome(5), Outcome::FAILED]);
    }
}
