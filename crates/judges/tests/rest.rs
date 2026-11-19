//! A failing model's rest through `Fallback`, with formats that do no I/O
//! and a clock the tests advance instead of sleeping.
#![expect(
    clippy::unwrap_used,
    reason = "the helpers fail the test on a broken fixture"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chauffeur_core::Failure;
use chauffeur_judges::{
    AnsweredBy, Evidence, Fallback, Format, Model, Probabilities, Question, REST, Rests,
};

type Replies = Vec<Result<Probabilities, Failure>>;

/// The clock the resting tests advance. Tests share it, so each holds
/// `SERIAL` for its whole run.
static CLOCK: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
static SERIAL: Mutex<()> = Mutex::new(());

/// The guard that keeps the shared clock to one test at a time.
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now() -> Instant {
    *CLOCK.lock().unwrap()
}

fn set(at: Instant) {
    *CLOCK.lock().unwrap() = at;
}

fn advance(by: Duration) {
    let mut clock = CLOCK.lock().unwrap();
    let next = clock.checked_add(by).unwrap();
    *clock = next;
}

/// A clock that fails the test if anything reads it.
#[expect(clippy::panic, reason = "reading this clock is the failure under test")]
fn panicking_now() -> Instant {
    panic!("no failure means no clock read")
}

fn clocked() -> Rests {
    Rests::with_clock(now, 4_096)
}

/// A format that gives `replies` to every call and counts its calls.
struct Fake {
    replies: Replies,
    calls: Arc<AtomicUsize>,
}

impl Format for Fake {
    fn answer_all(&self, _: &[&Question], _: &Evidence) -> Replies {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.replies.clone()
    }
}

/// A model named `name`, and the count of its calls.
fn model(name: &str, replies: Replies) -> (Model, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let format = Fake {
        replies,
        calls: Arc::clone(&calls),
    };
    (Model::new(name, Box::new(format)), calls)
}

fn question() -> Question {
    Question::noul(
        "Does this commit message break Conventional Commits?",
        ["commit"],
    )
    .unwrap()
}

fn evidence() -> Evidence {
    Evidence::new([("commit", "fixed stuff")])
}

fn p(yes: f64) -> Probabilities {
    Probabilities::read(question().outcomes(), [("yes", yes), ("no", 1.0 - yes)]).unwrap()
}

fn by(model: &str) -> AnsweredBy {
    AnsweredBy {
        model: model.into(),
        placeholder: false,
    }
}

#[test]
fn a_failure_rests_the_model_and_the_next_one_answers() {
    let _guard = serial();
    set(Instant::now());
    let (question, evidence) = (question(), evidence());
    let (first, first_calls) = model("first", vec![Err(Failure::Unavailable)]);
    let (second, second_calls) = model("second", vec![Ok(p(0.1))]);
    let rests = clocked();
    let fallback = Fallback::new(vec![first, second]).with_rests(rests.clone());

    let replies = fallback.answer_all(&[&question], &evidence);

    assert_eq!(replies, [Ok((p(0.1), by("second")))]);
    assert_eq!(first_calls.load(Ordering::Relaxed), 1);
    assert_eq!(second_calls.load(Ordering::Relaxed), 1);
    assert!(rests.cooling("first"));
    assert!(!rests.cooling("second"));
}

#[test]
fn a_rested_model_gets_no_calls_on_the_next_round() {
    let _guard = serial();
    set(Instant::now());
    let (question, evidence) = (question(), evidence());
    let (first, first_calls) = model("first", vec![Err(Failure::Unavailable)]);
    let (second, second_calls) = model("second", vec![Ok(p(0.1))]);
    let rests = clocked();
    let fallback = Fallback::new(vec![first, second]).with_rests(rests);

    let first_round = fallback.answer_all(&[&question], &evidence);
    let second_round = fallback.answer_all(&[&question], &evidence);

    assert_eq!(first_round, [Ok((p(0.1), by("second")))]);
    assert_eq!(second_round, [Ok((p(0.1), by("second")))]);
    assert_eq!(first_calls.load(Ordering::Relaxed), 1);
    assert_eq!(second_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn a_rest_ends_at_sixty_seconds_and_not_a_nanosecond_earlier() {
    let _guard = serial();
    set(Instant::now());
    let (question, evidence) = (question(), evidence());
    let (first, first_calls) = model("first", vec![Ok(p(0.9))]);
    let (second, second_calls) = model("second", vec![Ok(p(0.1))]);
    let rests = clocked();
    let fallback = Fallback::new(vec![first, second]).with_rests(rests.clone());
    rests.rest("first");

    let skipped = fallback.answer_all(&[&question], &evidence);
    assert_eq!(skipped, [Ok((p(0.1), by("second")))]);
    assert_eq!(first_calls.load(Ordering::Relaxed), 0);

    advance(REST.checked_sub(Duration::from_nanos(1)).unwrap());
    let still_skipped = fallback.answer_all(&[&question], &evidence);
    assert_eq!(still_skipped, [Ok((p(0.1), by("second")))]);
    assert_eq!(first_calls.load(Ordering::Relaxed), 0);
    assert_eq!(second_calls.load(Ordering::Relaxed), 2);

    advance(Duration::from_nanos(1));
    let asked = fallback.answer_all(&[&question], &evidence);
    assert_eq!(asked, [Ok((p(0.9), by("first")))]);
    assert_eq!(first_calls.load(Ordering::Relaxed), 1);
    assert_eq!(second_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn an_answer_ends_the_rest_even_beside_a_failure() {
    let _guard = serial();
    set(Instant::now());
    let (question, evidence) = (question(), evidence());
    let (first, first_calls) = model("first", vec![Ok(p(0.9)), Err(Failure::Unavailable)]);
    let (second, second_calls) = model("second", vec![Ok(p(0.1))]);
    let rests = clocked();
    let fallback = Fallback::new(vec![first, second]).with_rests(rests.clone());

    let first_round = fallback.answer_all(&[&question, &question], &evidence);
    assert_eq!(
        first_round,
        [Ok((p(0.9), by("first"))), Ok((p(0.1), by("second")))]
    );
    assert!(!rests.cooling("first"));

    let second_round = fallback.answer_all(&[&question, &question], &evidence);
    assert_eq!(
        second_round,
        [Ok((p(0.9), by("first"))), Ok((p(0.1), by("second")))]
    );
    assert_eq!(first_calls.load(Ordering::Relaxed), 2);
    assert_eq!(second_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn a_falls_back_failure_rests_even_beside_an_invalid_reply() {
    let _guard = serial();
    set(Instant::now());
    let (question, evidence) = (question(), evidence());
    let (first, first_calls) = model(
        "first",
        vec![Err(Failure::Unavailable), Err(Failure::Invalid)],
    );
    let rests = clocked();
    let fallback = Fallback::new(vec![first]).with_rests(rests.clone());

    let replies = fallback.answer_all(&[&question, &question], &evidence);

    assert_eq!(replies, [Err(Failure::Unavailable), Err(Failure::Invalid)]);
    assert_eq!(first_calls.load(Ordering::Relaxed), 1);
    assert!(rests.cooling("first"));
}

#[test]
fn invalid_and_deadline_failures_leave_the_model_unrested() {
    let _guard = serial();
    set(Instant::now());
    let (question, evidence) = (question(), evidence());

    let (invalid, invalid_calls) = model("invalid", vec![Err(Failure::Invalid)]);
    let invalid_rests = clocked();
    let invalid_fallback = Fallback::new(vec![invalid]).with_rests(invalid_rests.clone());

    let first = invalid_fallback.answer_all(&[&question], &evidence);
    let again = invalid_fallback.answer_all(&[&question], &evidence);

    assert_eq!(first, [Err(Failure::Invalid)]);
    assert_eq!(again, [Err(Failure::Invalid)]);
    assert_eq!(invalid_calls.load(Ordering::Relaxed), 2);
    assert!(!invalid_rests.cooling("invalid"));

    let (late, late_calls) = model("late", vec![Err(Failure::Deadline)]);
    let late_rests = clocked();
    let late_fallback = Fallback::new(vec![late]).with_rests(late_rests.clone());

    let first = late_fallback.answer_all(&[&question], &evidence);
    let again = late_fallback.answer_all(&[&question], &evidence);

    assert_eq!(first, [Err(Failure::Deadline)]);
    assert_eq!(again, [Err(Failure::Deadline)]);
    assert_eq!(late_calls.load(Ordering::Relaxed), 2);
    assert!(!late_rests.cooling("late"));
}

#[test]
fn reset_clears_every_rest_and_the_bound_forgets_the_oldest() {
    let _guard = serial();
    set(Instant::now());
    let rests = clocked();
    rests.rest("a");
    rests.rest("b");

    assert!(rests.cooling("a"));
    assert!(rests.cooling("b"));
    assert_eq!(rests.len(), 2);
    assert!(!rests.is_empty());

    rests.reset();

    assert!(!rests.cooling("a"));
    assert!(!rests.cooling("b"));
    assert_eq!(rests.len(), 0);
    assert!(rests.is_empty());

    let bounded = Rests::with_clock(now, 2);
    bounded.rest("a");
    advance(Duration::from_nanos(1));
    bounded.rest("b");
    advance(Duration::from_nanos(1));
    bounded.rest("c");

    assert!(!bounded.cooling("a"));
    assert!(bounded.cooling("b"));
    assert!(bounded.cooling("c"));
    assert_eq!(bounded.len(), 2);
}

#[test]
fn an_empty_registry_and_no_deadline_read_no_clock() {
    let rests = Rests::with_clock(panicking_now, 4_096);
    assert!(!rests.cooling("any"));

    let (question, evidence) = (question(), evidence());
    let (only, calls) = model("only", vec![Ok(p(0.9))]);
    let fallback = Fallback::new(vec![only]).with_rests(rests.clone());

    let replies = fallback.answer_all(&[&question], &evidence);

    assert_eq!(replies, [Ok((p(0.9), by("only")))]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(rests.is_empty());
}
