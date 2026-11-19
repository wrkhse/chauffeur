use std::time::Instant;

use chauffeur_core::{Failure, Outcome, Panel, Running};
use chauffeur_judges::{Answer, Question};

use super::{Cancel, Ran};

/// Drives the core rounds, checking the caller before each advance or ask.
pub(super) fn run(
    mut running: Running<'_, Question, Answer>,
    panel: &Panel<'_, Question, Answer>,
    deadline: Instant,
    cancel: &Cancel,
) -> Ran {
    loop {
        if cancel.is_cancelled() {
            return Ran::Cancelled;
        }
        if Instant::now() >= deadline {
            return Ran::Deadline;
        }

        let Some(round) = running.questions(Instant::now) else {
            return Ran::Settled(running.finish());
        };
        let reply = round.ask(panel);

        answer(&mut running, reply);
    }
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

    use chauffeur_core::{Event, Failure, Need, Outcome, Panel, Plan, Run, Step};
    use chauffeur_judges::{Answer, AnsweredBy, Confidence, Probabilities, Question};

    use super::{answer, run};
    use crate::{Cancel, Ran, Source};

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
            &panel,
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

        let ran = run(plan.start(Some(deadline)), &panel, deadline, &cancel);

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

        let ran = run(plan.start(Some(deadline)), &panel, deadline, &cancel);

        assert!(matches!(ran, Ran::Cancelled), "{ran:?}");
        assert_eq!(*calls.borrow(), Vec::<&str>::new());
    }

    #[test]
    fn the_caller_deadline_prevents_the_second_round() {
        let plan = plan();
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(200))
            .unwrap();
        let calls = RefCell::new(Vec::new());
        let quick = |question: &Question| {
            calls.borrow_mut().push("quick");
            thread::sleep(deadline.saturating_duration_since(Instant::now()));
            yes(question)
        };
        let careful = |question: &Question| {
            calls.borrow_mut().push("careful");
            yes(question)
        };
        let panel = Panel::new().with("quick", &quick).with("careful", &careful);

        let ran = run(
            plan.start(Some(deadline)),
            &panel,
            deadline,
            &Cancel::new(Source::Harness),
        );

        assert!(matches!(ran, Ran::Deadline), "{ran:?}");
        assert_eq!(*calls.borrow(), ["quick"]);
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
            &panel,
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
