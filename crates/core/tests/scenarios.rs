use std::cell::Cell;
use std::error::Error;
use std::time::{Duration, Instant};

use chauffeur_core::{
    Bar, Event, Failure, Judge, Near, Need, Outcome, Panel, Plan, PlanError, Reply, ReplyError,
    Request, Run, Running, Step, When,
};

/// A yes/no request, named by its text.
#[derive(Debug, PartialEq, Eq)]
struct Ask(&'static str);

impl Request for Ask {
    fn outcomes(&self) -> u8 {
        2
    }
}

/// An answer carrying a text; it settles to the first value.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Said(&'static str);

impl Reply for Said {
    fn settled(&self) -> u8 {
        0
    }

    fn p(&self, outcome: u8) -> Option<u16> {
        (outcome < 2).then_some(500)
    }
}

/// Counts its `judge_all` calls and answers each request with itself.
struct Counting {
    calls: Cell<u32>,
}

impl Judge<Ask, Said> for Counting {
    fn judge(&self, request: &Ask) -> Outcome<Said> {
        Outcome::Value(Said(request.0))
    }

    fn judge_all(&self, requests: &[&Ask]) -> Vec<Outcome<Said>> {
        self.calls.set(self.calls.get().saturating_add(1));
        requests.iter().map(|request| self.judge(request)).collect()
    }
}

/// Always returns one answer, whatever it is asked.
struct Short;

impl Judge<Ask, Said> for Short {
    fn judge(&self, _: &Ask) -> Outcome<Said> {
        Outcome::Value(Said("only"))
    }

    fn judge_all(&self, _: &[&Ask]) -> Vec<Outcome<Said>> {
        vec![Outcome::Value(Said("only"))]
    }
}

#[test]
fn an_empty_plan_or_a_repeated_id_is_refused() {
    assert_eq!(Plan::<Ask>::new([]), Err(PlanError::Empty));
    assert_eq!(
        Plan::new([Step::new("a", "j", Ask("x")), Step::new("a", "j", Ask("x"))]),
        Err(PlanError::DuplicateStep("a".into()))
    );
}

#[test]
fn a_judge_the_panel_lacks_is_refused_before_anyone_is_asked() {
    let counting = Counting {
        calls: Cell::new(0),
    };
    let plan = Plan::new([
        Step::new("a", "known", Ask("x")),
        Step::new("b", "missing", Ask("y")),
    ])
    .unwrap();
    let panel = Panel::new().with("known", &counting);

    let error = plan.run(&panel).unwrap_err();

    assert_eq!(
        error,
        PlanError::UnknownJudge {
            step: "b".into(),
            judge: "missing".into()
        }
    );
    assert_eq!(
        error.to_string(),
        "step `b` names judge `missing`, which the panel lacks"
    );
    assert_eq!(counting.calls.get(), 0);
}

#[test]
fn each_judge_answers_its_share_in_one_call() {
    let one = Counting {
        calls: Cell::new(0),
    };
    let two = Counting {
        calls: Cell::new(0),
    };
    let plan = Plan::new([
        Step::new("a", "one", Ask("first")),
        Step::new("b", "two", Ask("second")),
        Step::new("c", "one", Ask("third")),
    ])
    .unwrap();
    let panel = Panel::new().with("one", &one).with("two", &two);

    let run = plan.run(&panel).unwrap();

    assert_eq!((one.calls.get(), two.calls.get()), (1, 1));
    let verdict: Vec<_> = run
        .verdict
        .iter()
        .map(|(id, outcome)| (id, outcome.clone()))
        .collect();
    assert_eq!(
        verdict,
        [
            ("a", Outcome::Value(Said("first"))),
            ("b", Outcome::Value(Said("second"))),
            ("c", Outcome::Value(Said("third")))
        ]
    );
}

#[test]
fn a_judge_added_again_under_its_name_replaces_the_first() {
    let first = |_: &Ask| Outcome::Value(Said("first"));
    let second = |_: &Ask| Outcome::Value(Said("second"));
    let plan = Plan::new([Step::new("a", "j", Ask("x"))]).unwrap();
    let panel = Panel::new().with("j", &first).with("j", &second);

    let run = plan.run(&panel).unwrap();

    assert_eq!(run.verdict.get("a"), Some(&Outcome::Value(Said("second"))));
}

#[test]
fn a_batch_of_the_wrong_length_fails_every_step_in_it() {
    let plan = Plan::new([
        Step::new("a", "short", Ask("x")),
        Step::new("b", "short", Ask("y")),
    ])
    .unwrap();
    let panel = Panel::new().with("short", &Short);

    let run = plan.run(&panel).unwrap();

    assert_eq!(
        run.verdict.get("a"),
        Some(&Outcome::Failed(Failure::Invalid))
    );
    assert_eq!(
        run.verdict.get("b"),
        Some(&Outcome::Failed(Failure::Invalid))
    );
}

#[test]
fn a_failure_stays_a_failure_and_events_come_in_order() {
    let timeout = |_: &Ask| Outcome::<Said>::Failed(Failure::Timeout);
    let plan = Plan::new([Step::new("a", "slow", Ask("x"))]).unwrap();
    let panel = Panel::new().with("slow", &timeout);

    let run = plan.run(&panel).unwrap();

    assert_eq!(
        run.events,
        [
            Event::Started {
                steps: vec!["a".into()]
            },
            Event::RoundReady {
                round: 1,
                steps: vec!["a".into()]
            },
            Event::StepSettled {
                step: "a".into(),
                outcome: Outcome::Failed(Failure::Timeout)
            },
            Event::Finished,
        ]
    );
    assert_eq!(Failure::RateLimited.to_string(), "rate_limited");
}

/// A request with this many values; three are yes (0), no (1) and unsure (2).
#[derive(Debug, PartialEq, Eq)]
struct Question(u8);

impl Request for Question {
    fn outcomes(&self) -> u8 {
        self.0
    }
}

/// The yes, no or unsure question every escalation step asks.
const YES_NO: Question = Question(3);

/// An answer: the value it settled to and its probability for each value,
/// in thousandths.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Answer {
    settled: u8,
    p: Vec<u16>,
}

impl Reply for Answer {
    fn settled(&self) -> u8 {
        self.settled
    }

    fn p(&self, outcome: u8) -> Option<u16> {
        self.p.get(usize::from(outcome)).copied()
    }
}

/// An answer with P(yes) of `p`: yes from 500, otherwise no.
fn yes(p: u16) -> Answer {
    Answer {
        settled: u8::from(p < 500),
        p: vec![p, 1000_u16.saturating_sub(p), 0],
    }
}

/// An answer settled to `settled`, sure of it.
fn said(settled: u8) -> Answer {
    let p = (0..3)
        .map(|outcome| if outcome == settled { 1000 } else { 0 })
        .collect();

    Answer { settled, p }
}

/// Answers every request with its scripted outcome, counting its
/// `judge_all` calls and the requests they held.
struct Scripted {
    calls: Cell<u32>,
    asked: Cell<usize>,
    outcome: Outcome<Answer>,
}

impl Scripted {
    const fn new(outcome: Outcome<Answer>) -> Self {
        Self {
            calls: Cell::new(0),
            asked: Cell::new(0),
            outcome,
        }
    }

    const fn answering(answer: Answer) -> Self {
        Self::new(Outcome::Value(answer))
    }
}

impl Judge<Question, Answer> for Scripted {
    fn judge(&self, _: &Question) -> Outcome<Answer> {
        self.outcome.clone()
    }

    fn judge_all(&self, requests: &[&Question]) -> Vec<Outcome<Answer>> {
        self.calls.set(self.calls.get().saturating_add(1));
        self.asked
            .set(self.asked.get().saturating_add(requests.len()));
        requests.iter().map(|request| self.judge(request)).collect()
    }
}

fn ids(steps: &[&str]) -> Vec<String> {
    steps.iter().map(|step| (*step).to_owned()).collect()
}

fn started(steps: &[&str]) -> Event<Answer> {
    Event::Started { steps: ids(steps) }
}

fn ready(round: u32, steps: &[&str]) -> Event<Answer> {
    Event::RoundReady {
        round,
        steps: ids(steps),
    }
}

fn settled(step: &str, outcome: Outcome<Answer>) -> Event<Answer> {
    Event::StepSettled {
        step: step.to_owned(),
        outcome,
    }
}

fn skipped(step: &str) -> Event<Answer> {
    Event::StepSkipped {
        step: step.to_owned(),
    }
}

fn skips(run: &Run<Answer>) -> Vec<&str> {
    run.verdict.skipped().collect()
}

/// The commit nudge's bar: P(yes) within 0.15 of 0.7.
fn near_yes() -> Near {
    Near::new(
        150,
        [Bar {
            outcome: 0,
            at: 700,
        }],
    )
}

/// A need on `step` settling to `value`.
fn is(step: &str, value: u8) -> Need {
    Need::outcome(step, None, Some(vec![When::Outcome(value)]))
}

/// fast asks Jev; sure asks the glide judge when Jev is near the bar or failed.
fn escalation() -> [Step<Question>; 2] {
    [
        Step::new("fast", "jev", YES_NO),
        Step::new("sure", "glide", YES_NO).after([Need::outcome(
            "fast",
            Some(near_yes()),
            Some(vec![When::Failed]),
        )]),
    ]
}

/// Runs the escalation with Jev's `fast` outcome, and returns the run and
/// the glide judge's `judge_all` calls.
fn escalate(fast: Outcome<Answer>) -> Result<(Run<Answer>, u32), PlanError> {
    let plan = Plan::new(escalation())?;
    let jev = Scripted::new(fast);
    let glide = Scripted::answering(yes(900));
    let panel = Panel::new().with("jev", &jev).with("glide", &glide);

    let run = plan.run(&panel)?;

    Ok((run, glide.calls.get()))
}

#[test]
fn jev_near_the_bar_asks_glide_once_and_sure_settles() {
    for p in [550, 600, 700, 800, 850] {
        let (run, calls) = escalate(Outcome::Value(yes(p))).unwrap();

        assert_eq!(calls, 1, "P(yes) {p}");
        assert_eq!(run.verdict.get("sure"), Some(&Outcome::Value(yes(900))));
        assert_eq!(skips(&run), Vec::<&str>::new());
    }

    let (run, _) = escalate(Outcome::Value(yes(800))).unwrap();
    assert_eq!(
        run.events,
        [
            started(&["fast", "sure"]),
            ready(1, &["fast"]),
            settled("fast", Outcome::Value(yes(800))),
            ready(2, &["sure"]),
            settled("sure", Outcome::Value(yes(900))),
            Event::Finished,
        ]
    );
}

#[test]
fn jev_far_from_the_bar_never_asks_glide_and_sure_is_skipped() {
    for p in [250, 549, 851, 1000] {
        let (run, calls) = escalate(Outcome::Value(yes(p))).unwrap();

        assert_eq!(calls, 0, "P(yes) {p}");
        assert_eq!(run.verdict.get("sure"), None);
        assert_eq!(skips(&run), ["sure"]);
    }

    let (run, _) = escalate(Outcome::Value(yes(250))).unwrap();
    assert_eq!(
        run.events,
        [
            started(&["fast", "sure"]),
            ready(1, &["fast"]),
            settled("fast", Outcome::Value(yes(250))),
            skipped("sure"),
            Event::Finished,
        ]
    );
}

#[test]
fn jev_failing_asks_glide_when_the_need_lists_failed() {
    for failure in [Failure::Timeout, Failure::Invalid] {
        let (run, calls) = escalate(Outcome::Failed(failure)).unwrap();

        assert_eq!(calls, 1, "{failure}");
        assert_eq!(run.verdict.get("fast"), Some(&Outcome::Failed(failure)));
        assert_eq!(run.verdict.get("sure"), Some(&Outcome::Value(yes(900))));
    }
}

#[test]
fn a_value_in_when_matches_even_far_from_the_bar() {
    let plan = Plan::new([
        Step::new("fast", "jev", YES_NO),
        Step::new("sure", "glide", YES_NO).after([Need::outcome(
            "fast",
            Some(near_yes()),
            Some(vec![When::Outcome(2)]),
        )]),
    ])
    .unwrap();
    let unsure = Answer {
        settled: 2,
        p: vec![300, 300, 400],
    };
    let asks = |fast: Answer| {
        let jev = Scripted::answering(fast);
        let glide = Scripted::answering(yes(900));
        let panel = Panel::new().with("jev", &jev).with("glide", &glide);
        let run = plan.run(&panel).unwrap();
        (glide.calls.get(), run.verdict.get("sure").cloned())
    };

    assert_eq!(asks(unsure), (1, Some(Outcome::Value(yes(900)))));
    assert_eq!(asks(yes(300)), (0, None));
}

/// Runs the escalation by hand: Jev answers near the bar, then round 2 is
/// asked for `left` before the end. Returns round 2's number, if it
/// started, and the finished run.
fn escalate_with(left: Duration) -> Result<(Option<u32>, Run<Answer>), Box<dyn Error>> {
    let now = Instant::now();
    let deadline = now
        .checked_add(Duration::from_secs(60))
        .ok_or("no deadline")?;
    let plan = Plan::new(escalation())?;
    let mut running = plan.start(Some(deadline));
    let end = running.end().ok_or("no end")?;

    running.questions(now).ok_or("no first round")?;
    running.answer([("fast".to_owned(), Outcome::Value(yes(800)))])?;

    let late = end.checked_sub(left).ok_or("no time left")?;
    let second = running.questions(late).map(|round| round.number());

    Ok((second, running.finish()))
}

#[test]
fn the_steps_end_one_second_before_the_deadline() {
    let plan = Plan::new(escalation()).unwrap();
    let deadline = Instant::now().checked_add(Duration::from_secs(5)).unwrap();

    let running: Running<'_, Question, Answer> = plan.start(Some(deadline));
    let unbounded: Running<'_, Question, Answer> = plan.start(None);

    assert_eq!(running.end(), deadline.checked_sub(Duration::from_secs(1)));
    assert_eq!(unbounded.end(), None);
}

#[test]
fn an_escalation_with_less_than_500_ms_left_is_not_started() {
    let (second, run) = escalate_with(Duration::from_millis(499)).unwrap();

    assert_eq!(second, None);
    assert_eq!(skips(&run), ["sure"]);
    assert_eq!(
        run.events,
        [
            started(&["fast", "sure"]),
            ready(1, &["fast"]),
            settled("fast", Outcome::Value(yes(800))),
            skipped("sure"),
            Event::Finished,
        ]
    );
}

#[test]
fn an_escalation_with_500_ms_left_starts() {
    let (second, run) = escalate_with(Duration::from_millis(500)).unwrap();

    assert_eq!(second, Some(2));
    assert_eq!(skips(&run), Vec::<&str>::new());
    assert_eq!(
        run.events,
        [
            started(&["fast", "sure"]),
            ready(1, &["fast"]),
            settled("fast", Outcome::Value(yes(800))),
            ready(2, &["sure"]),
            settled("sure", Outcome::Failed(Failure::Deadline)),
            Event::Finished,
        ]
    );
}

/// The open round's number and step IDs.
fn open(running: &mut Running<'_, Question, Answer>, now: Instant) -> Option<(u32, Vec<String>)> {
    running.questions(now).map(|round| {
        let steps = round.steps().iter().map(|step| step.id().to_owned());
        (round.number(), steps.collect())
    })
}

#[test]
fn the_first_round_starts_even_past_the_end() {
    let plan = Plan::new(escalation()).unwrap();
    let deadline = Instant::now();
    let mut running = plan.start(Some(deadline));
    let later = deadline.checked_add(Duration::from_secs(5)).unwrap();

    assert_eq!(open(&mut running, later), Some((1, ids(&["fast"]))));
}

/// Runs the escalation with Jev saying no, so `sure` is skipped, and a
/// third step `last` waiting on `need`.
fn after_skipped(need: Need) -> Result<Run<Answer>, PlanError> {
    let [fast, sure] = escalation();
    let plan = Plan::new([fast, sure, Step::new("last", "tail", YES_NO).after([need])])?;
    let jev = Scripted::answering(yes(250));
    let glide = Scripted::answering(yes(900));
    let tail = Scripted::answering(said(0));
    let panel = Panel::new()
        .with("jev", &jev)
        .with("glide", &glide)
        .with("tail", &tail);

    plan.run(&panel)
}

#[test]
fn a_string_need_waits_for_a_skipped_step_and_then_runs() {
    let run = after_skipped(Need::settled("sure")).unwrap();

    assert_eq!(
        run.events,
        [
            started(&["fast", "sure", "last"]),
            ready(1, &["fast"]),
            settled("fast", Outcome::Value(yes(250))),
            skipped("sure"),
            ready(2, &["last"]),
            settled("last", Outcome::Value(said(0))),
            Event::Finished,
        ]
    );
}

#[test]
fn an_object_need_never_matches_a_skipped_step() {
    let every = vec![
        When::Outcome(0),
        When::Outcome(1),
        When::Outcome(2),
        When::Failed,
    ];
    let run = after_skipped(Need::outcome("sure", None, Some(every))).unwrap();

    assert_eq!(
        run.events,
        [
            started(&["fast", "sure", "last"]),
            ready(1, &["fast"]),
            settled("fast", Outcome::Value(yes(250))),
            skipped("sure"),
            skipped("last"),
            Event::Finished,
        ]
    );
    assert_eq!(skips(&run), ["sure", "last"]);
}

/// Runs `a` and `b` with these outcomes, then `last` waiting on all of
/// `all` and one of `any`. Returns how often `last`'s judge was called and
/// the skipped steps.
fn gate(
    a: Outcome<Answer>,
    b: Outcome<Answer>,
    all: Vec<Need>,
    any: Vec<Need>,
) -> Result<(u32, Vec<String>), PlanError> {
    let plan = Plan::new([
        Step::new("a", "ja", YES_NO),
        Step::new("b", "jb", YES_NO),
        Step::new("last", "jl", YES_NO).after(all).after_any(any),
    ])?;
    let ja = Scripted::new(a);
    let jb = Scripted::new(b);
    let jl = Scripted::answering(said(0));
    let panel = Panel::new().with("ja", &ja).with("jb", &jb).with("jl", &jl);

    let run = plan.run(&panel)?;
    let skipped = run.verdict.skipped().map(str::to_owned).collect();

    Ok((jl.calls.get(), skipped))
}

#[test]
fn failed_matches_only_when_listed() {
    let timeout = || Outcome::Failed(Failure::Timeout);
    let zero = || Outcome::Value(said(0));
    let or_failed = Need::outcome("a", None, Some(vec![When::Outcome(0), When::Failed]));

    let unlisted = gate(timeout(), zero(), vec![is("a", 0)], vec![]).unwrap();
    let listed = gate(timeout(), zero(), vec![or_failed], vec![]).unwrap();

    assert_eq!(unlisted, (0, ids(&["last"])));
    assert_eq!(listed, (1, ids(&[])));
}

#[test]
fn a_value_need_matches_only_its_listed_values() {
    let one = || Outcome::Value(said(1));

    assert_eq!(
        gate(one(), one(), vec![is("a", 1)], vec![]).unwrap(),
        (1, ids(&[]))
    );
    assert_eq!(
        gate(one(), one(), vec![is("a", 0)], vec![]).unwrap(),
        (0, ids(&["last"]))
    );
}

#[test]
fn a_step_with_all_and_any_runs_only_when_both_hold() {
    let gated = |all: Vec<Need>, any: Vec<Need>| {
        let (a, b) = (Outcome::Value(said(0)), Outcome::Value(said(1)));
        gate(a, b, all, any).unwrap()
    };
    let runs = (1, ids(&[]));
    let skipped = (0, ids(&["last"]));

    assert_eq!(gated(vec![is("a", 0)], vec![is("b", 1)]), runs);
    assert_eq!(gated(vec![is("a", 0)], vec![is("b", 0)]), skipped);
    assert_eq!(gated(vec![is("a", 1)], vec![is("b", 1)]), skipped);
    assert_eq!(gated(vec![is("a", 1)], vec![is("b", 0)]), skipped);
    assert_eq!(gated(vec![], vec![is("b", 0), is("a", 0)]), runs);
    assert_eq!(gated(vec![is("a", 0), is("b", 0)], vec![]), skipped);
    assert_eq!(gated(vec![is("a", 0)], vec![is("b", 0), is("a", 0)]), runs);
}

/// A near need on fast's P(yes): within `margin` of `at`.
fn near(margin: u16, at: u16) -> Need {
    Need::outcome(
        "fast",
        Some(Near::new(margin, [Bar { outcome: 0, at }])),
        None,
    )
}

/// Runs fast, answering P(yes) `p`, and three steps reading it with near
/// needs: low near 0.2, high near 0.8, and both near 0.7 and 0.8 at once.
/// Returns the skipped steps.
fn bars(p: u16) -> Result<Vec<String>, PlanError> {
    let plan = Plan::new([
        Step::new("fast", "jev", YES_NO),
        Step::new("low", "glide", YES_NO).after([near(100, 200)]),
        Step::new("high", "glide", YES_NO).after([near(100, 800)]),
        Step::new("both", "glide", YES_NO).after([near(150, 700), near(50, 800)]),
    ])?;
    let jev = Scripted::answering(yes(p));
    let glide = Scripted::answering(said(0));
    let panel = Panel::new().with("jev", &jev).with("glide", &glide);

    let run = plan.run(&panel)?;

    Ok(run.verdict.skipped().map(str::to_owned).collect())
}

#[test]
fn near_needs_reading_one_step_each_get_their_own_bit() {
    assert_eq!(bars(800).unwrap(), ids(&["low"]));
    assert_eq!(bars(200).unwrap(), ids(&["high", "both"]));
    assert_eq!(bars(700).unwrap(), ids(&["low", "both"]));
}

#[test]
fn a_plan_keeps_its_steps_and_their_needs_in_order() {
    let plan = Plan::new(escalation()).unwrap();
    let steps: Vec<&str> = plan.steps().iter().map(Step::id).collect();
    let need = Need::outcome("fast", Some(near_yes()), Some(vec![When::Failed]));

    assert_eq!(steps, ["fast", "sure"]);
    assert_eq!(plan.steps()[1].all(), [need]);
    assert_eq!(plan.steps()[1].any(), []);
}

/// Why a plan of `steps` is refused, with its message; none when it is not.
fn refusal(steps: impl IntoIterator<Item = Step<Question>>) -> Option<(PlanError, String)> {
    Plan::new(steps).err().map(|error| {
        let text = error.to_string();
        (error, text)
    })
}

/// Why step `b` waiting on `need`, after step `a` asking `target`, is
/// refused.
fn waits(target: Question, need: Need) -> Option<(PlanError, String)> {
    refusal([
        Step::new("a", "j", target),
        Step::new("b", "j", YES_NO).after([need]),
    ])
}

/// The error `error` makes for step `b` waiting on `a`, with `text`.
fn on_a(error: fn(String, String) -> PlanError, text: &str) -> (PlanError, String) {
    (error("b".into(), "a".into()), text.to_owned())
}

#[test]
fn an_empty_plan_too_many_steps_or_a_duplicate_is_refused() {
    let numbered =
        |count: usize| (0..count).map(|index| Step::new(format!("s{index}"), "j", YES_NO));

    assert_eq!(
        refusal([]),
        Some((PlanError::Empty, "a plan needs at least one step".into()))
    );
    assert_eq!(
        refusal(numbered(17)),
        Some((
            PlanError::TooManySteps(17),
            "a plan has at most 16 steps, not 17".into()
        ))
    );
    assert_eq!(refusal(numbered(16)), None);
    assert_eq!(
        refusal([
            Step::new("a", "j", YES_NO),
            Step::new("b", "j", YES_NO),
            Step::new("a", "j", YES_NO),
        ]),
        Some((
            PlanError::DuplicateStep("a".into()),
            "two steps are named `a`".into()
        ))
    );
}

#[test]
fn a_need_on_an_unknown_later_or_own_step_is_refused() {
    let unknown = refusal([
        Step::new("a", "j", YES_NO),
        Step::new("b", "j", YES_NO).after([Need::settled("ghost")]),
    ]);
    let later = refusal([
        Step::new("a", "j", YES_NO).after_any([Need::settled("b")]),
        Step::new("b", "j", YES_NO),
    ]);
    let own = refusal([Step::new("a", "j", YES_NO).after([Need::settled("a")])]);
    let later_need = |step: &str, need: &str| PlanError::LaterNeed {
        step: step.into(),
        need: need.into(),
    };

    assert_eq!(
        unknown,
        Some((
            PlanError::UnknownNeed {
                step: "b".into(),
                need: "ghost".into()
            },
            "step `b` waits on `ghost`, which the plan lacks".into()
        ))
    );
    assert_eq!(
        later,
        Some((
            later_need("a", "b"),
            "step `a` waits on `b`, which is not an earlier step".into()
        ))
    );
    assert_eq!(
        own,
        Some((
            later_need("a", "a"),
            "step `a` waits on `a`, which is not an earlier step".into()
        ))
    );
}

#[test]
fn a_need_with_an_empty_when_or_nothing_to_match_is_refused() {
    let empty_when = |step, need| PlanError::EmptyWhen { step, need };
    let empty_need = |step, need| PlanError::EmptyNeed { step, need };
    let barless = || Some(Near::new(100, []));
    let neither = "step `b` waits on `a` with neither a bar nor a `when`";
    let failed = Some(vec![When::Failed]);

    assert_eq!(
        waits(YES_NO, Need::outcome("a", None, Some(vec![]))),
        Some(on_a(
            empty_when,
            "step `b` waits on `a` with an empty `when`"
        ))
    );
    assert_eq!(
        waits(YES_NO, Need::outcome("a", None, None)),
        Some(on_a(empty_need, neither))
    );
    assert_eq!(
        waits(YES_NO, Need::outcome("a", barless(), None)),
        Some(on_a(empty_need, neither))
    );
    assert_eq!(waits(YES_NO, Need::outcome("a", barless(), failed)), None);
    assert_eq!(
        waits(YES_NO, Need::outcome("a", Some(near_yes()), None)),
        None
    );
}

/// A near need on `a` with one bar.
fn bar(margin: u16, outcome: u8, at: u16) -> Need {
    Need::outcome("a", Some(Near::new(margin, [Bar { outcome, at }])), None)
}

#[test]
fn a_bar_or_margin_over_one_is_refused() {
    let over_one = |step, need| PlanError::OverOne { step, need };
    let text = "step `b` waits on `a` with a bar or margin over 1";

    assert_eq!(waits(YES_NO, bar(1001, 0, 500)), Some(on_a(over_one, text)));
    assert_eq!(waits(YES_NO, bar(100, 0, 1001)), Some(on_a(over_one, text)));
    assert_eq!(waits(YES_NO, bar(1000, 0, 1000)), None);
}

#[test]
fn a_need_on_an_outcome_its_step_lacks_is_refused() {
    let no_such = |step, need| PlanError::NoSuchOutcome { step, need };
    let text = "step `b` waits on an outcome `a` does not have";

    assert_eq!(waits(YES_NO, bar(100, 3, 500)), Some(on_a(no_such, text)));
    assert_eq!(waits(YES_NO, is("a", 3)), Some(on_a(no_such, text)));
    assert_eq!(waits(YES_NO, bar(100, 2, 500)), None);
    assert_eq!(waits(YES_NO, is("a", 2)), None);
}

#[test]
fn a_step_whose_codes_do_not_fit_is_refused() {
    let nears = |outcomes: u8, count: usize| {
        let needs = (0..count).map(|_| bar(100, 0, 500));
        refusal([
            Step::new("a", "j", Question(outcomes)),
            Step::new("b", "j", YES_NO).after(needs),
        ])
    };
    let outcomes = Some((
        PlanError::Outcomes { step: "a".into() },
        "step `a` has no outcomes, or more outcomes and near needs than its codes hold".into(),
    ));

    assert_eq!(refusal([Step::new("a", "j", Question(0))]), outcomes);
    assert_eq!(refusal([Step::new("a", "j", Question(127))]), outcomes);
    assert_eq!(refusal([Step::new("a", "j", Question(126))]), None);
    assert_eq!(nears(1, 7), outcomes);
    assert_eq!(nears(1, 6), None);
    assert_eq!(nears(65, 1), outcomes);
    assert_eq!(nears(63, 1), None);
}

#[test]
fn errors_the_step_machine_gives_read_as_sentences() {
    assert_eq!(
        PlanError::Machine("no steps".into()).to_string(),
        "the step machine refused the plan: no steps"
    );
    assert_eq!(
        ReplyError::Refused("wrong status".into()).to_string(),
        "the step machine refused the answers: wrong status"
    );
}

/// a and b in round 1, then c, which waits on a.
fn trio() -> Result<Plan<Question>, PlanError> {
    Plan::new([
        Step::new("a", "ja", YES_NO),
        Step::new("b", "jb", YES_NO),
        Step::new("c", "jc", YES_NO).after([Need::settled("a")]),
    ])
}

/// A reply settling each named step to its value.
fn reply(answers: &[(&str, u8)]) -> Vec<(String, Outcome<Answer>)> {
    answers
        .iter()
        .map(|(step, value)| ((*step).to_owned(), Outcome::Value(said(*value))))
        .collect()
}

/// A refusal of `error`, with its message `text`.
fn refused(error: ReplyError, text: &str) -> (ReplyError, String) {
    (error, text.to_owned())
}

#[test]
fn a_refused_reply_leaves_the_round_as_it_was() {
    let plan = trio().unwrap();
    let now = Instant::now();
    let mut running = plan.start(None);
    let first = Some((1, ids(&["a", "b"])));
    assert_eq!(open(&mut running, now), first);

    let not_asked = |step: &str| ReplyError::NotAsked(step.into());
    let missing = |step: &str| ReplyError::Missing(step.into());
    let repeated = ReplyError::Repeated("a".into());
    let refusals = [
        (
            reply(&[("ghost", 0)]),
            refused(
                not_asked("ghost"),
                "step `ghost` was not asked in this round",
            ),
        ),
        (
            reply(&[("a", 0), ("c", 0)]),
            refused(not_asked("c"), "step `c` was not asked in this round"),
        ),
        (
            reply(&[("a", 0), ("a", 0)]),
            refused(repeated, "step `a` was answered twice"),
        ),
        (
            reply(&[("a", 0)]),
            refused(missing("b"), "step `b` was asked but not answered"),
        ),
        (
            reply(&[("b", 0)]),
            refused(missing("a"), "step `a` was asked but not answered"),
        ),
    ];
    for (answers, expected) in refusals {
        let error = running.answer(answers).unwrap_err();

        assert_eq!((error.clone(), error.to_string()), expected);
        assert_eq!(open(&mut running, now), first);
    }

    running.answer(reply(&[("b", 1), ("a", 0)])).unwrap();
    assert_eq!(open(&mut running, now), Some((2, ids(&["c"]))));
    running.answer(reply(&[("c", 2)])).unwrap();
    assert_eq!(open(&mut running, now), None);

    assert_eq!(
        running.finish().events,
        [
            started(&["a", "b", "c"]),
            ready(1, &["a", "b"]),
            settled("a", Outcome::Value(said(0))),
            settled("b", Outcome::Value(said(1))),
            ready(2, &["c"]),
            settled("c", Outcome::Value(said(2))),
            Event::Finished,
        ]
    );
}

#[test]
fn a_failed_step_settles_alone_and_the_rest_are_answered_after() {
    let plan = trio().unwrap();
    let now = Instant::now();
    let mut running = plan.start(None);
    let not_asked = |step: &str| Err(ReplyError::NotAsked(step.into()));
    open(&mut running, now);

    assert_eq!(running.fail("a", Failure::Timeout), Ok(()));
    assert_eq!(running.fail("a", Failure::Timeout), not_asked("a"));
    assert_eq!(running.fail("ghost", Failure::Timeout), not_asked("ghost"));
    assert_eq!(running.answer(reply(&[("a", 0), ("b", 0)])), not_asked("a"));
    assert_eq!(open(&mut running, now), Some((1, ids(&["b"]))));

    assert_eq!(running.answer(reply(&[("b", 1)])), Ok(()));
    assert_eq!(open(&mut running, now), Some((2, ids(&["c"]))));
    assert_eq!(running.fail("b", Failure::Timeout), not_asked("b"));
    assert_eq!(running.fail("c", Failure::RateLimited), Ok(()));
    assert_eq!(open(&mut running, now), None);

    assert_eq!(
        running.finish().events,
        [
            started(&["a", "b", "c"]),
            ready(1, &["a", "b"]),
            settled("a", Outcome::Failed(Failure::Timeout)),
            settled("b", Outcome::Value(said(1))),
            ready(2, &["c"]),
            settled("c", Outcome::Failed(Failure::RateLimited)),
            Event::Finished,
        ]
    );
}

/// a and b in round 1, then c, which waits on a failing.
fn after_failed() -> Result<Plan<Question>, PlanError> {
    let failed = Need::outcome("a", None, Some(vec![When::Failed]));

    Plan::new([
        Step::new("a", "ja", YES_NO),
        Step::new("b", "jb", YES_NO),
        Step::new("c", "jc", YES_NO).after([failed]),
    ])
}

#[test]
fn failing_the_round_settles_every_open_step_and_the_next_round_opens() {
    let plan = after_failed().unwrap();
    let now = Instant::now();
    let mut running = plan.start(None);
    open(&mut running, now);

    running.fail_round(Failure::Timeout);
    assert_eq!(open(&mut running, now), Some((2, ids(&["c"]))));
    running.answer(reply(&[("c", 0)])).unwrap();

    assert_eq!(
        running.finish().events,
        [
            started(&["a", "b", "c"]),
            ready(1, &["a", "b"]),
            settled("a", Outcome::Failed(Failure::Timeout)),
            settled("b", Outcome::Failed(Failure::Timeout)),
            ready(2, &["c"]),
            settled("c", Outcome::Value(said(0))),
            Event::Finished,
        ]
    );
}

#[test]
fn a_step_of_a_failed_round_is_no_longer_asked() {
    let plan = after_failed().unwrap();
    let mut running = plan.start(None);
    open(&mut running, Instant::now());

    running.fail_round(Failure::Timeout);

    assert_eq!(
        running.answer(reply(&[("a", 0)])),
        Err(ReplyError::NotAsked("a".into()))
    );
}

#[test]
fn failing_the_round_with_none_open_changes_nothing() {
    let plan = trio().unwrap();
    let now = Instant::now();
    let mut running = plan.start(None);
    open(&mut running, now);
    running.answer(reply(&[("a", 0), ("b", 1)])).unwrap();

    running.fail_round(Failure::Timeout);
    assert_eq!(open(&mut running, now), Some((2, ids(&["c"]))));
    running.answer(reply(&[("c", 2)])).unwrap();

    assert_eq!(
        running.finish().events,
        [
            started(&["a", "b", "c"]),
            ready(1, &["a", "b"]),
            settled("a", Outcome::Value(said(0))),
            settled("b", Outcome::Value(said(1))),
            ready(2, &["c"]),
            settled("c", Outcome::Value(said(2))),
            Event::Finished,
        ]
    );
}

#[test]
fn an_answer_outside_its_values_settles_as_invalid() {
    let plan = Plan::new([Step::new("a", "j", YES_NO)]).unwrap();
    let outcome = |answer: Answer| {
        let judge = Scripted::answering(answer);
        let run = plan.run(&Panel::new().with("j", &judge)).unwrap();
        run.verdict.get("a").cloned()
    };

    assert_eq!(outcome(said(3)), Some(Outcome::Failed(Failure::Invalid)));
    assert_eq!(outcome(said(2)), Some(Outcome::Value(said(2))));
}

#[test]
fn finishing_fails_the_open_round_by_the_deadline_and_skips_the_rest() {
    let plan = trio().unwrap();
    let mut running = plan.start(None);
    open(&mut running, Instant::now());

    let run = running.finish();
    let deadline = Outcome::Failed(Failure::Deadline);
    let verdict: Vec<_> = run
        .verdict
        .iter()
        .map(|(id, outcome)| (id, outcome.clone()))
        .collect();

    assert_eq!(
        run.events,
        [
            started(&["a", "b", "c"]),
            ready(1, &["a", "b"]),
            settled("a", deadline.clone()),
            settled("b", deadline.clone()),
            skipped("c"),
            Event::Finished,
        ]
    );
    assert_eq!(verdict, [("a", deadline.clone()), ("b", deadline)]);
    assert_eq!(skips(&run), ["c"]);
}

#[test]
fn finishing_before_any_round_skips_every_step() {
    let plan = trio().unwrap();
    let running: Running<'_, Question, Answer> = plan.start(None);

    let run = running.finish();

    assert_eq!(
        run.events,
        [
            started(&["a", "b", "c"]),
            skipped("a"),
            skipped("b"),
            skipped("c"),
            Event::Finished,
        ]
    );
    assert_eq!(run.verdict.iter().count(), 0);
}

#[test]
fn a_round_asks_each_judge_once_in_order_and_fails_a_missing_one() {
    let plan = Plan::new([
        Step::new("a", "one", YES_NO),
        Step::new("b", "two", YES_NO),
        Step::new("c", "one", YES_NO),
    ])
    .unwrap();
    let one = Scripted::answering(said(0));
    let panel = Panel::new().with("one", &one);
    let mut running: Running<'_, Question, Answer> = plan.start(None);

    let round = running.questions(Instant::now()).unwrap();
    let asked = round.ask(&panel);

    assert_eq!(
        asked,
        [
            ("a".to_owned(), Outcome::Value(said(0))),
            ("c".to_owned(), Outcome::Value(said(0))),
            ("b".to_owned(), Outcome::Failed(Failure::Unavailable)),
        ]
    );
    assert_eq!((one.calls.get(), one.asked.get()), (1, 2));
}

#[test]
fn each_judge_answers_its_share_of_round_two_in_one_call() {
    let plan = Plan::new([
        Step::new("a", "one", YES_NO),
        Step::new("b", "two", YES_NO).after([Need::settled("a")]),
        Step::new("c", "two", YES_NO).after([Need::settled("a")]),
    ])
    .unwrap();
    let one = Scripted::answering(said(0));
    let two = Scripted::answering(said(1));
    let panel = Panel::new().with("one", &one).with("two", &two);

    let run = plan.run(&panel).unwrap();

    assert_eq!((one.calls.get(), one.asked.get()), (1, 1));
    assert_eq!((two.calls.get(), two.asked.get()), (1, 2));
    assert_eq!(
        run.events,
        [
            started(&["a", "b", "c"]),
            ready(1, &["a"]),
            settled("a", Outcome::Value(said(0))),
            ready(2, &["b", "c"]),
            settled("b", Outcome::Value(said(1))),
            settled("c", Outcome::Value(said(1))),
            Event::Finished,
        ]
    );
}
