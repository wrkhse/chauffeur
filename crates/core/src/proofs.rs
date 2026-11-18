//! Kani harnesses for what core adds to the step machine: a near need
//! matches only within its margin, an answer and its near matches survive
//! their one code, and a reply settles all of its steps or none.

use rungs::{MAX_OUTCOMES, Outcome as Code, Status};

use crate::plan::near::{self, MAX_NEAR};
use crate::plan::run::{Settle, settle_batch};
use crate::{Bar, Near, Reply, When};

/// The values an answer settles to here.
const OUTCOMES: u8 = 3;

/// An answer: the value it settled to, and its probability for each value.
struct Said {
    settled: u8,
    p: [u16; 3],
}

impl Reply for Said {
    fn settled(&self) -> u8 {
        self.settled
    }

    fn p(&self, outcome: u8) -> Option<u16> {
        self.p.get(usize::from(outcome)).copied()
    }
}

fn any_thousandths() -> u16 {
    let thousandths: u16 = kani::any();
    kani::assume(thousandths <= 1000);
    thousandths
}

fn any_below(limit: u8) -> u8 {
    let value: u8 = kani::any();
    kani::assume(value < limit);
    value
}

/// The `when` list of the values, and the failure, whose bits `set` holds.
fn when_of(set: u8) -> Vec<When> {
    let mut when: Vec<When> = (0..OUTCOMES)
        .filter(|outcome| set >> outcome & 1 == 1)
        .map(When::Outcome)
        .collect();
    if set >> OUTCOMES & 1 == 1 {
        when.push(When::Failed);
    }

    when
}

/// Whether `codes` holds `code`. It reads every code: `contains` stops at the
/// first match, and at an unknown code Kani follows both ways on from each.
fn holds(codes: &[Code], code: Code) -> bool {
    codes
        .iter()
        .fold(false, |found, other| found | (*other == code))
}

/// For every answer over three values, every bar and margin in thousandths,
/// and every `when` list: a need accepts the code the answer settles to
/// exactly when the bar's probability lies within the margin, edges included,
/// or `when` lists the value. It accepts the failure exactly when `when`
/// lists it.
#[kani::proof]
#[kani::unwind(17)]
fn a_near_need_matches_only_within_its_margin() {
    let answer = Said {
        settled: any_below(OUTCOMES),
        p: [any_thousandths(), any_thousandths(), any_thousandths()],
    };
    let bar = Bar {
        outcome: any_below(OUTCOMES),
        at: any_thousandths(),
    };
    let margin = any_thousandths();

    let p = answer.p[usize::from(bar.outcome)];
    let within = p <= bar.at + margin && bar.at <= p + margin;
    let matched = near::matches(&Near::new(margin, [bar]), &answer);
    assert!(matched == within);

    // One near need reads the step, so a code is the value, then the match.
    let code = near::encode(answer.settled, u8::from(matched), 1).unwrap();

    // Every set of the three values and the failure, one at a time: with
    // the list known, only the answer is left to the solver.
    for set in 0..1 << (OUTCOMES + 1) {
        let accepted = near::accepts(OUTCOMES, 1, Some(0), &when_of(set));
        let listed = set >> answer.settled & 1 == 1;
        let failure = set >> OUTCOMES & 1 == 1;

        assert!(holds(&accepted, code) == (within || listed));
        assert!(holds(&accepted, Code::FAILED) == failure);
    }
}

/// For every value, set of matched near needs and count of them: they have a
/// code exactly when the count is within `MAX_NEAR`, the set within the
/// count and the code within the machine's, and the code decodes to both.
/// So each value of a step whose codes fit has, with every set of its near
/// needs, a code among the step's.
#[kani::proof]
fn an_outcome_index_decodes_to_its_answer_and_near_matches() {
    let (outcomes, k, settled, matched): (u8, u8, u8, u8) = kani::any();
    let span = 1_u32.checked_shl(u32::from(k)).unwrap_or(u32::MAX);
    let wide = u64::from(settled) * u64::from(span) + u64::from(matched);
    let fits = k <= MAX_NEAR && u32::from(matched) < span && wide < u64::from(MAX_OUTCOMES);

    let code = near::encode(settled, matched, k);
    assert_eq!(code.is_some(), fits);
    if let Some(code) = code {
        assert_eq!(near::decode(code, k), (settled, matched));
    }

    if let Some(total) = near::codes(outcomes, k) {
        if settled < outcomes {
            assert_eq!(code.is_some(), u32::from(matched) < span);
            assert!(code.is_none_or(|code| code.0 < total));
        }
    }
}

/// One item of a reply: a step open in the round, one still waiting or one
/// past the plan, settled with a code it has or lacks, or failed.
fn any_item() -> (usize, Settle) {
    let settle = if kani::any() {
        Settle::Code(Code(any_below(3)))
    } else {
        Settle::Fail
    };

    (usize::from(any_below(4)), settle)
}

/// Steps 0 and 1 are open and step 2 waits on step 0. For every reply of up
/// to three items: the run it is settled on stays as it was, and it is taken
/// exactly when each item names a different open step, with a code the step
/// has or a failure. A reply that is taken settles every step it names and
/// no other.
#[kani::proof]
#[kani::unwind(5)]
fn a_reply_settles_all_of_its_steps_or_none() {
    let plan = rungs::Plan::new([
        rungs::Step::new(2),
        rungs::Step::new(2),
        rungs::Step::new(2).after([rungs::Need::settled(0)]),
    ])
    .unwrap();
    let mut run = plan.start();
    assert!(run.advance() == [0, 1]);
    let before = run.progress();

    let items = [any_item(), any_item(), any_item()];
    let batch = &items[..usize::from(any_below(4))];
    let names = |step: usize| batch.iter().filter(|(index, _)| *index == step).count();
    let fits = |(index, settle): &(usize, Settle)| {
        *index < 2 && names(*index) == 1 && !matches!(settle, Settle::Code(Code(2..)))
    };
    let whole = batch.iter().filter(|item| fits(item)).count() == batch.len();

    let settled = settle_batch(&run, batch);
    assert!(run.progress() == before);
    assert!(settled.is_ok() == whole);

    if let Ok(next) = settled {
        for step in 0..3 {
            let was = before.status[step];
            let now = next.progress().status[step];
            assert!(matches!(now, Status::Settled(_)) == (names(step) == 1));
            assert!(now == was || names(step) == 1);
        }
    }
}
