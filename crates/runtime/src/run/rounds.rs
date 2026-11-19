use std::time::{Duration, Instant};

use chauffeur_core::{Failure, Outcome, Round, Running};
use chauffeur_judges::{Answer, Question};

use super::{Cancel, Ran};

/// How long the first round's judges keep working past the caller's
/// deadline. The caller's wait answers `Deadline` at the deadline itself,
/// so a judge slower than the deadline is cut by that wait first; its model
/// call then stops just after and frees the worker.
pub(super) const GRACE: Duration = Duration::from_millis(100);

/// One round's answers from judges whose model work ends at the given
/// moment, or never for `None`.
pub(super) type Ask<'a> =
    dyn Fn(&Round<'_, Question>, Option<Instant>) -> Vec<(String, Outcome<Answer>)> + 'a;

/// Drives the core rounds. Each turn checks cancellation, then asks core for
/// the next round; the caller's deadline is checked only when there is a
/// round left to ask. Answers that arrive after the deadline still settle
/// their steps, so a run whose last round answered late is `Settled`. Each
/// round is asked through `ask` with the end `judges_end` gives it.
pub(super) fn run(
    mut running: Running<'_, Question, Answer>,
    ask: &Ask<'_>,
    deadline: Instant,
    cancel: &Cancel,
) -> Ran {
    loop {
        if cancel.is_cancelled() {
            return Ran::Cancelled;
        }

        let Some(round) = running.questions(Instant::now) else {
            return Ran::Settled(running.finish());
        };

        if Instant::now() >= deadline {
            return Ran::Deadline;
        }

        let end = judges_end(&round, &running, deadline);
        let reply = ask(&round, end);

        answer(&mut running, reply);
    }
}

/// When a round's judges end. The first round ends `GRACE` after the
/// caller's deadline, so a slow judge is cut by the caller's wait. A later
/// round ends at core's end, before the deadline, so the answers already in
/// hand return in time.
fn judges_end(
    round: &Round<'_, Question>,
    running: &Running<'_, Question, Answer>,
    deadline: Instant,
) -> Option<Instant> {
    if round.number() == 1 {
        return Some(deadline.checked_add(GRACE).unwrap_or(deadline));
    }

    running.end()
}

/// A refused batch leaves the round open; settle it as invalid, not timed out.
fn answer(running: &mut Running<'_, Question, Answer>, reply: Vec<(String, Outcome<Answer>)>) {
    if running.answer(reply).is_err() {
        running.fail_round(Failure::Invalid);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::thread;
    use std::time::{Duration, Instant};

    use chauffeur_core::{Event, Failure, Need, Outcome, Panel, Plan, Round, Run, Step};
    use chauffeur_judges::{Answer, AnsweredBy, Confidence, Probabilities, Question};

    use super::{answer, run};
    use crate::{Cancel, Ran, Source};

    /// Asks each round of `panel`, ignoring the end the round is given.
    fn asking<'a>(
        panel: &'a Panel<'_, Question, Answer>,
    ) -> impl Fn(&Round<'_, Question>, Option<Instant>) -> Vec<(String, Outcome<Answer>)> + 'a {
        move |round, _| round.ask(panel)
    }

    fn plan() -> Plan<Question> {
        let question = Question::noul("Ship it?", ["diff"]).unwrap();

        Plan::new([
            Step::new("first", "quick", question.clone()),
            Step::new("second", "careful", question).after([Need::settled("first")]),
        ])
        .unwrap()
    }

    fn yes(question: &Question) -> Outcome<Answer> {
        let p = Probabilities::read(question.outcomes(), [("yes", 0.9), ("no", 0.1)]).unwrap();
        let by = AnsweredBy {
            model: "counting".into(),
            placeholder: false,
        };

        Outcome::Value(Answer::settle(question, p, &Confidence::Likeliest, by))
    }

    fn deadline() -> Instant {
        Instant::now().checked_add(Duration::from_secs(5)).unwrap()
    }

    fn settled(ran: Ran) -> Run<Answer> {
        let Ran::Settled(run) = ran else {
            panic!("expected a settled run, got {ran:?}");
        };

        run
    }

    #[test]
    fn two_rounds_keep_the_exact_events_and_verdict() {
        let plan = plan();
        let calls = RefCell::new(Vec::new());
        let quick = |question: &Question| {
            calls.borrow_mut().push("quick");
            yes(question)
        };
        let careful = |question: &Question| {
            calls.borrow_mut().push("careful");
            yes(question)
        };
        let panel = Panel::new().with("quick", &quick).with("careful", &careful);
        let deadline = deadline();

        let ran = settled(run(
            plan.start(Some(deadline)),
            &asking(&panel),
            deadline,
            &Cancel::new(Source::Harness),
        ));
        let outcome = yes(plan.steps()[0].request());

        assert_eq!(*calls.borrow(), ["quick", "careful"]);
        assert_eq!(
            ran.events,
            [
                Event::Started {
                    steps: vec!["first".into(), "second".into()]
                },
                Event::RoundReady {
                    round: 1,
                    steps: vec!["first".into()]
                },
                Event::StepSettled {
                    step: "first".into(),
                    outcome: outcome.clone()
                },
                Event::RoundReady {
                    round: 2,
                    steps: vec!["second".into()]
                },
                Event::StepSettled {
                    step: "second".into(),
                    outcome: outcome.clone()
                },
                Event::Finished,
            ]
        );
        assert_eq!(
            ran.verdict.iter().collect::<Vec<_>>(),
            [("first", &outcome), ("second", &outcome),]
        );
        assert_eq!(
            ran.verdict.skipped().collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn the_first_round_ends_after_the_deadline_and_a_later_one_before_it() {
        let plan = plan();
        let deadline = deadline();
        let judge = |question: &Question| yes(question);
        let panel = Panel::new().with("quick", &judge).with("careful", &judge);
        let ends = RefCell::new(Vec::new());
        let ask = |round: &Round<'_, Question>, end: Option<Instant>| {
            ends.borrow_mut().push((round.number(), end));
            round.ask(&panel)
        };

        let ran = run(
            plan.start(Some(deadline)),
            &ask,
            deadline,
            &Cancel::new(Source::Harness),
        );

        assert!(matches!(ran, Ran::Settled(_)), "{ran:?}");
        assert_eq!(
            *ends.borrow(),
            [
                (1, deadline.checked_add(Duration::from_millis(100))),
                (2, deadline.checked_sub(Duration::from_secs(1))),
            ]
        );
    }

    #[test]
    fn cancellation_from_the_first_judge_prevents_the_second_round() {
        let plan = plan();
        let cancel = Cancel::new(Source::Harness);
        let calls = RefCell::new(Vec::new());
        let quick = |question: &Question| {
            calls.borrow_mut().push("quick");
            cancel.cancel();
            yes(question)
        };
        let careful = |question: &Question| {
            calls.borrow_mut().push("careful");
            yes(question)
        };
        let panel = Panel::new().with("quick", &quick).with("careful", &careful);
        let deadline = deadline();

        let ran = run(
            plan.start(Some(deadline)),
            &asking(&panel),
            deadline,
            &cancel,
        );

        assert!(matches!(ran, Ran::Cancelled), "{ran:?}");
        assert_eq!(*calls.borrow(), ["quick"]);
    }

    #[test]
    fn pre_cancelled_runs_ask_nothing_even_past_the_deadline() {
        let plan = plan();
        let cancel = Cancel::new(Source::Harness);
        cancel.cancel();
        let calls = RefCell::new(Vec::new());
        let judge = |question: &Question| {
            calls.borrow_mut().push("asked");
            yes(question)
        };
        let panel = Panel::new().with("quick", &judge).with("careful", &judge);
        let deadline = Instant::now();

        let ran = run(
            plan.start(Some(deadline)),
            &asking(&panel),
            deadline,
            &cancel,
        );

        assert!(matches!(ran, Ran::Cancelled), "{ran:?}");
        assert_eq!(*calls.borrow(), Vec::<&str>::new());
    }

    /// A caller deadline this far away is not reached before the first round
    /// opens, however slowly the test thread is scheduled: reaching that check
    /// takes microseconds, so a loaded runner would need a whole second's stall.
    fn caller_deadline() -> Instant {
        Instant::now().checked_add(Duration::from_secs(1)).unwrap()
    }

    /// Sleeps until `deadline` has certainly passed: a sleep never ends early.
    fn sleep_past(deadline: Instant) {
        let left = deadline.saturating_duration_since(Instant::now());

        thread::sleep(left.saturating_add(Duration::from_millis(10)));
    }

    #[test]
    fn a_last_round_answered_after_the_caller_deadline_still_settles() {
        let question = Question::noul("Ship it?", ["diff"]).unwrap();
        let plan = Plan::new([Step::new("only", "quick", question)]).unwrap();
        let deadline = caller_deadline();
        let quick = |question: &Question| {
            sleep_past(deadline);
            yes(question)
        };
        let panel = Panel::new().with("quick", &quick);

        let ran = settled(run(
            plan.start(Some(deadline)),
            &asking(&panel),
            deadline,
            &Cancel::new(Source::Harness),
        ));
        let outcome = yes(plan.steps()[0].request());

        assert!(Instant::now() > deadline);
        assert_eq!(
            ran.events,
            [
                Event::Started {
                    steps: vec!["only".into()]
                },
                Event::RoundReady {
                    round: 1,
                    steps: vec!["only".into()]
                },
                Event::StepSettled {
                    step: "only".into(),
                    outcome: outcome.clone()
                },
                Event::Finished,
            ]
        );
        assert_eq!(ran.verdict.iter().collect::<Vec<_>>(), [("only", &outcome)]);
        assert_eq!(
            ran.verdict.skipped().collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn the_caller_deadline_prevents_the_second_round() {
        let plan = plan();
        let deadline = caller_deadline();
        let calls = RefCell::new(Vec::new());
        let quick = |question: &Question| {
            calls.borrow_mut().push("quick");
            sleep_past(deadline);
            yes(question)
        };
        let careful = |question: &Question| {
            calls.borrow_mut().push("careful");
            yes(question)
        };
        let panel = Panel::new().with("quick", &quick).with("careful", &careful);

        let ran = settled(run(
            plan.start(Some(deadline)),
            &asking(&panel),
            deadline,
            &Cancel::new(Source::Harness),
        ));
        let outcome = yes(plan.steps()[0].request());

        assert_eq!(*calls.borrow(), ["quick"]);
        assert_eq!(
            ran.events,
            [
                Event::Started {
                    steps: vec!["first".into(), "second".into()]
                },
                Event::RoundReady {
                    round: 1,
                    steps: vec!["first".into()]
                },
                Event::StepSettled {
                    step: "first".into(),
                    outcome: outcome.clone()
                },
                Event::StepSkipped {
                    step: "second".into()
                },
                Event::Finished,
            ]
        );
        assert_eq!(
            ran.verdict.iter().collect::<Vec<_>>(),
            [("first", &outcome)]
        );
        assert_eq!(ran.verdict.skipped().collect::<Vec<_>>(), ["second"]);
    }

    #[test]
    fn a_caller_deadline_passed_before_the_first_round_asks_nothing() {
        let plan = plan();
        let calls = RefCell::new(Vec::new());
        let judge = |question: &Question| {
            calls.borrow_mut().push("asked");
            yes(question)
        };
        let panel = Panel::new().with("quick", &judge).with("careful", &judge);
        let deadline = Instant::now();

        let ran = run(
            plan.start(Some(deadline)),
            &asking(&panel),
            deadline,
            &Cancel::new(Source::Harness),
        );

        assert!(matches!(ran, Ran::Deadline), "{ran:?}");
        assert_eq!(*calls.borrow(), Vec::<&str>::new());
    }

    #[test]
    fn refused_answers_fail_the_open_round_as_invalid() {
        let plan = plan();
        let deadline = deadline();
        let mut running = plan.start(Some(deadline));
        assert_eq!(running.questions(Instant::now).unwrap().number(), 1);

        answer(&mut running, Vec::new());

        let calls = RefCell::new(Vec::new());
        let judge = |question: &Question| {
            calls.borrow_mut().push("careful");
            yes(question)
        };
        let panel = Panel::new().with("careful", &judge);
        let ran = settled(run(
            running,
            &asking(&panel),
            deadline,
            &Cancel::new(Source::Harness),
        ));
        let invalid = Outcome::Failed(Failure::Invalid);
        let outcome = yes(plan.steps()[1].request());

        assert_eq!(*calls.borrow(), ["careful"]);
        assert_eq!(
            ran.events,
            [
                Event::Started {
                    steps: vec!["first".into(), "second".into()]
                },
                Event::RoundReady {
                    round: 1,
                    steps: vec!["first".into()]
                },
                Event::StepSettled {
                    step: "first".into(),
                    outcome: invalid.clone()
                },
                Event::RoundReady {
                    round: 2,
                    steps: vec!["second".into()]
                },
                Event::StepSettled {
                    step: "second".into(),
                    outcome: outcome.clone()
                },
                Event::Finished,
            ]
        );
        assert_eq!(
            ran.verdict.iter().collect::<Vec<_>>(),
            [("first", &invalid), ("second", &outcome),]
        );
        assert_eq!(
            ran.verdict.skipped().collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
    }
}
