//! Kani harnesses: admission's ledger stays within its capacity, a permit's
//! last copy gives back exactly one slot, and the event
//! log never holds more than its bound.

use crate::admission::{Admission, Capacity, Ledger, Source};
use crate::run::EventLog;

fn any_source() -> Source {
    if kani::any() {
        Source::Harness
    } else {
        Source::Integration
    }
}

fn any_capacity() -> Capacity {
    let global: usize = kani::any();
    let per_source: usize = kani::any();
    kani::assume(global <= 3 && per_source <= 3);
    Capacity { global, per_source }
}

#[kani::proof]
#[kani::unwind(7)]
fn the_ledger_stays_within_its_capacity() {
    let capacity = any_capacity();
    let mut ledger = Ledger::new(capacity);

    for _ in 0..6 {
        let source = any_source();
        if kani::any() {
            let before = ledger.held(source);
            if ledger.take(source).is_ok() {
                assert!(ledger.held(source) == before + 1);
            } else {
                assert!(ledger.held(source) == before);
            }
        } else {
            ledger.give(source);
        }
        let (harness, integration) = (
            ledger.held(Source::Harness),
            ledger.held(Source::Integration),
        );
        assert!(harness <= capacity.per_source && integration <= capacity.per_source);
        assert!(ledger.total() <= capacity.global);
        assert!(ledger.total() == harness + integration);
    }
}

// The ledger's Mutex lock loop is unbounded without this; 2 is a complete bound.
#[kani::proof]
#[kani::unwind(2)]
fn the_last_permit_copy_gives_back_exactly_one_slot() {
    let admission = Admission::new(Capacity {
        global: 2,
        per_source: 2,
    });
    let source = any_source();
    let permit = admission.admit(source).ok();
    let ending = admission.admit(source).ok();
    let copy = ending.clone();
    assert!(permit.is_some() && ending.is_some() && copy.is_some());
    assert!(admission.in_use().held(source) == 2);

    // A clone shares its permit's slot, so either copy may drop first and
    // the slot comes back only with the last one.
    if kani::any::<bool>() {
        drop(ending);
        assert!(admission.in_use().held(source) == 2);
        drop(copy);
    } else {
        drop(copy);
        assert!(admission.in_use().held(source) == 2);
        drop(ending);
    }
    assert!(admission.in_use().held(source) == 1);
    drop(permit);
    assert!(admission.in_use().held(source) == 0);
    assert!(admission.in_use().total() == 0);
}

#[kani::proof]
#[kani::unwind(6)]
fn the_event_log_never_exceeds_its_bound() {
    let max: usize = kani::any();
    kani::assume(max <= 3);
    let mut log = EventLog::new(max);

    for event in 0_u8..5 {
        let kept = log.push(event);
        assert!(log.events().len() <= max);
        assert!(kept || log.events().len() == max);
    }
    assert!(log.events().len() + log.dropped() == 5);
}
