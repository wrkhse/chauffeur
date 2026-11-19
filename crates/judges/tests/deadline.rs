//! A round's deadline through `Format`, `Fallback` and `NamedJudge`, with
//! formats that do no I/O.
#![expect(
    clippy::unwrap_used,
    reason = "the helpers fail the test on a broken fixture"
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chauffeur_core::{Failure, Judge, Outcome};
use chauffeur_judges::{
    Answer, AnsweredBy, Confidence, Evidence, Fallback, Format, Model, NamedJudge, Placeholder,
    PlaceholderEntry, Probabilities, Question,
};

type Replies = Vec<Result<Probabilities, Failure>>;

/// A format with only the unbounded call: it gives `replies` and counts its
/// calls.
struct Unbounded {
    replies: Replies,
    calls: Arc<AtomicUsize>,
}

impl Format for Unbounded {
    fn answer_all(&self, _: &[&Question], _: &Evidence) -> Replies {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.replies.clone()
    }
}

/// A format that records the deadline of each call, waits for it to pass
/// when it `overruns`, then gives `replies`.
struct Recording {
    replies: Replies,
    overruns: bool,
    deadlines: Arc<Mutex<Vec<Option<Instant>>>>,
}

impl Format for Recording {
    fn answer_all(&self, questions: &[&Question], evidence: &Evidence) -> Replies {
        self.answer_all_until(questions, evidence, None)
    }

    fn answer_all_until(
        &self,
        _: &[&Question],
        _: &Evidence,
        deadline: Option<Instant>,
    ) -> Replies {
        self.deadlines.lock().unwrap().push(deadline);

        while self.overruns && deadline.is_some_and(|end| Instant::now() < end) {
            thread::sleep(Duration::from_millis(1));
        }

        self.replies.clone()
    }
}

/// An unbounded model named `name`, and the count of its calls.
fn unbounded(name: &str, replies: Replies) -> (Model, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let format = Unbounded {
        replies,
        calls: Arc::clone(&calls),
    };
    (Model::new(name, Box::new(format)), calls)
}

/// A recording model named `name`, and the deadlines its calls were given.
fn recording(
    name: &str,
    replies: Replies,
    overruns: bool,
) -> (Model, Arc<Mutex<Vec<Option<Instant>>>>) {
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let format = Recording {
        replies,
        overruns,
        deadlines: Arc::clone(&deadlines),
    };
    (Model::new(name, Box::new(format)), deadlines)
}

fn seen(deadlines: &Mutex<Vec<Option<Instant>>>) -> Vec<Option<Instant>> {
    deadlines.lock().unwrap().clone()
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

/// A deadline `milliseconds` from now.
fn within(milliseconds: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(milliseconds))
        .unwrap()
}

/// A deadline that has passed.
fn passed() -> Instant {
    Instant::now()
}

#[test]
fn the_default_refuses_a_round_past_its_deadline_and_otherwise_asks_as_before() {
    let (question, evidence) = (question(), evidence());
    let calls = Arc::new(AtomicUsize::new(0));
    let format: Box<dyn Format> = Box::new(Unbounded {
        replies: vec![Ok(p(0.9))],
        calls: Arc::clone(&calls),
    });

    let late = format.answer_all_until(&[&question], &evidence, Some(passed()));

    assert_eq!(late, [Err(Failure::Deadline)]);
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    let in_time = format.answer_all_until(&[&question], &evidence, Some(within(60_000)));
    let no_deadline = format.answer_all_until(&[&question], &evidence, None);
    let unbounded = format.answer_all(&[&question], &evidence);

    assert_eq!(in_time, [Ok(p(0.9))]);
    assert_eq!(no_deadline, [Ok(p(0.9))]);
    assert_eq!(unbounded, [Ok(p(0.9))]);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn a_placeholder_fails_a_round_past_its_deadline_and_answers_one_within_it() {
    let (question, evidence) = (question(), evidence());
    let entry = PlaceholderEntry {
        answers: None,
        otherwise: Some(BTreeMap::from([("yes".into(), 0.2), ("no".into(), 0.8)])),
    };
    let placeholder = Placeholder::load(&entry).unwrap();

    let late = placeholder.answer_all_until(&[&question], &evidence, Some(passed()));
    let in_time = placeholder.answer_all_until(&[&question], &evidence, Some(within(60_000)));
    let unbounded = placeholder.answer_all(&[&question], &evidence);

    assert_eq!(late, [Err(Failure::Deadline)]);
    assert_eq!(in_time, [Ok(p(0.2))]);
    assert_eq!(unbounded, [Ok(p(0.2))]);
}

#[test]
fn a_deadline_already_passed_fails_every_question_and_asks_no_model() {
    let (question, evidence) = (question(), evidence());
    let (first, deadlines) = recording("first", vec![Ok(p(0.9)), Ok(p(0.9))], false);
    let (second, calls) = unbounded("second", vec![Ok(p(0.1)), Ok(p(0.1))]);
    let fallback = Fallback::new(vec![first, second]);

    let replies = fallback.answer_all_until(&[&question, &question], &evidence, Some(passed()));

    assert_eq!(replies, [Err(Failure::Deadline), Err(Failure::Deadline)]);
    assert_eq!(seen(&deadlines), []);
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    let no_model =
        Fallback::new(Vec::new()).answer_all_until(&[&question], &evidence, Some(passed()));

    assert_eq!(
        no_model,
        [Err(Failure::Unavailable)],
        "with no model to ask, the deadline is not what failed the question"
    );
}

#[test]
fn the_deadline_passing_fails_only_the_questions_still_open() {
    let (question, evidence) = (question(), evidence());
    let overrun = vec![
        Ok(p(0.9)),
        Err(Failure::Invalid),
        Err(Failure::RateLimited),
        Err(Failure::Timeout),
    ];
    let (first, deadlines) = recording("first", overrun, true);
    let (second, calls) = unbounded("second", vec![Ok(p(0.1)), Ok(p(0.1))]);
    let fallback = Fallback::new(vec![first, second]);
    let deadline = Some(within(500));

    let replies = fallback.answer_all_until(&[&question; 4], &evidence, deadline);

    assert_eq!(
        replies,
        [
            Ok((p(0.9), by("first"))),
            Err(Failure::Invalid),
            Err(Failure::Deadline),
            Err(Failure::Deadline),
        ]
    );
    assert_eq!(seen(&deadlines), [deadline]);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn every_model_is_given_the_same_deadline_and_the_unbounded_call_gives_none() {
    let (question, evidence) = (question(), evidence());
    let (first, first_deadlines) = recording("first", vec![Err(Failure::RateLimited)], false);
    let (second, second_deadlines) = recording("second", vec![Ok(p(0.9))], false);
    let fallback = Fallback::new(vec![first, second]);
    let deadline = Some(within(60_000));

    let bounded = fallback.answer_all_until(&[&question], &evidence, deadline);
    let unbounded = fallback.answer_all(&[&question], &evidence);

    assert_eq!(bounded, [Ok((p(0.9), by("second")))]);
    assert_eq!(unbounded, [Ok((p(0.9), by("second")))]);
    assert_eq!(seen(&first_deadlines), [deadline, None]);
    assert_eq!(seen(&second_deadlines), [deadline, None]);
}

#[test]
fn a_model_that_reports_the_deadline_is_not_followed_by_the_next() {
    let (question, evidence) = (question(), evidence());
    let (first, _) = recording("first", vec![Err(Failure::Deadline)], false);
    let (second, calls) = unbounded("second", vec![Ok(p(0.1))]);
    let fallback = Fallback::new(vec![first, second]);

    let replies = fallback.answer_all_until(&[&question], &evidence, Some(within(60_000)));

    assert_eq!(replies, [Err(Failure::Deadline)]);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn a_reply_that_is_too_short_leaves_the_rest_to_the_next_model() {
    let (question, evidence) = (question(), evidence());
    let (first, _) = unbounded("first", vec![Ok(p(0.9))]);
    let (second, _) = unbounded("second", vec![Ok(p(0.1))]);
    let (alone, _) = unbounded("alone", vec![Ok(p(0.9))]);
    let deadline = Some(within(60_000));

    let both = Fallback::new(vec![first, second]).answer_all_until(
        &[&question, &question],
        &evidence,
        deadline,
    );
    let one =
        Fallback::new(vec![alone]).answer_all_until(&[&question, &question], &evidence, deadline);

    assert_eq!(
        both,
        [Ok((p(0.9), by("first"))), Ok((p(0.1), by("second")))]
    );
    assert_eq!(one, [Ok((p(0.9), by("alone"))), Err(Failure::Unavailable)]);
}

#[test]
fn a_named_judge_gives_its_models_its_deadline_and_none_by_default() {
    let (question, evidence) = (question(), evidence());
    let (only, deadlines) = recording("only", vec![Ok(p(0.9))], false);
    let fallback = Fallback::new(vec![only]);
    let confidence = Confidence::Likeliest;
    let judge = || NamedJudge::new(&fallback, &confidence, &evidence);
    let answered = Outcome::Value(Answer::settle(&question, p(0.9), &confidence, by("only")));
    let deadline = Some(within(60_000));

    let unbounded = judge().judge_all(&[&question]);
    let in_time = judge().with_deadline(deadline).judge_all(&[&question]);
    let late = judge().with_deadline(Some(passed())).judge(&question);

    assert_eq!(unbounded, in_time);
    assert_eq!(in_time, [answered]);
    assert_eq!(late, Outcome::Failed(Failure::Deadline));
    assert_eq!(seen(&deadlines), [None, deadline]);
}
