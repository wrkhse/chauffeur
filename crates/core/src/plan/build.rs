//! The checks a plan passes before any judge is asked, and the step machine
//! built from it.

use super::near::{self, codes};
use super::need::{Near, Need, Wants, When};
use super::reply::Request;
use super::{DECISION, MAX_STEPS, PlanError, Shape, Step};

/// Each step's needs' bits among their targets' near needs, all then any;
/// none for a need without bars.
pub(super) type NeedBits = Vec<Vec<Option<u8>>>;

/// The most a bar or margin is, in thousandths.
const ONE: u16 = 1000;

pub(super) const fn check_count<Q>(steps: &[Step<Q>]) -> Result<(), PlanError> {
    if steps.is_empty() {
        return Err(PlanError::Empty);
    }
    if steps.len() > MAX_STEPS {
        return Err(PlanError::TooManySteps(steps.len()));
    }

    Ok(())
}

pub(super) fn check_ids<Q>(steps: &[Step<Q>]) -> Result<(), PlanError> {
    for (index, step) in steps.iter().enumerate() {
        if step.id() == DECISION {
            return Err(PlanError::ReservedStep(step.id().to_owned()));
        }

        if steps
            .iter()
            .take(index)
            .any(|earlier| earlier.id() == step.id())
        {
            return Err(PlanError::DuplicateStep(step.id().to_owned()));
        }
    }

    Ok(())
}

/// Each step's needs, all then any, as the indexes of the steps they name.
pub(super) fn check_needs<Q: Request>(steps: &[Step<Q>]) -> Result<Vec<Vec<usize>>, PlanError> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            step.all()
                .iter()
                .chain(step.any())
                .map(|need| check_need(steps, index, step, need))
                .collect()
        })
        .collect()
}

fn check_need<Q: Request>(
    steps: &[Step<Q>],
    index: usize,
    step: &Step<Q>,
    need: &Need,
) -> Result<usize, PlanError> {
    let ids = || (step.id().to_owned(), need.step().to_owned());
    let Some(target) = steps.iter().position(|other| other.id() == need.step()) else {
        let (step, need) = ids();
        return Err(PlanError::UnknownNeed { step, need });
    };
    if target >= index {
        let (step, need) = ids();
        return Err(PlanError::LaterNeed { step, need });
    }

    let Wants::Outcome { near, when } = need.wants() else {
        return Ok(target);
    };
    let outcomes = steps
        .get(target)
        .map_or(0, |target| target.request().outcomes());
    let bars = near.as_ref().map_or(&[][..], Near::bars);
    let values = when.as_deref().unwrap_or(&[]);

    if when.as_ref().is_some_and(Vec::is_empty) {
        let (step, need) = ids();
        return Err(PlanError::EmptyWhen { step, need });
    }
    if bars.is_empty() && when.is_none() {
        let (step, need) = ids();
        return Err(PlanError::EmptyNeed { step, need });
    }
    if near.as_ref().is_some_and(|near| near.margin() > ONE) || bars.iter().any(|bar| bar.at > ONE)
    {
        let (step, need) = ids();
        return Err(PlanError::OverOne { step, need });
    }

    let stray_bar = bars.iter().any(|bar| bar.outcome >= outcomes);
    let stray_when = values
        .iter()
        .any(|value| matches!(value, When::Outcome(value) if *value >= outcomes));
    if stray_bar || stray_when {
        let (step, need) = ids();
        return Err(PlanError::NoSuchOutcome { step, need });
    }

    Ok(target)
}

/// Each step's shape, and each need's bit among its target's near needs,
/// numbered in plan order.
pub(super) fn shapes<Q: Request>(
    steps: &[Step<Q>],
    targets: &[Vec<usize>],
) -> Result<(Vec<Shape>, NeedBits), PlanError> {
    let mut shapes: Vec<Shape> = steps
        .iter()
        .map(|step| Shape {
            outcomes: step.request().outcomes(),
            nears: Vec::new(),
        })
        .collect();

    let mut bits = Vec::with_capacity(steps.len());
    for (step, named) in steps.iter().zip(targets) {
        let mut step_bits = Vec::with_capacity(named.len());
        for (need, target) in step.all().iter().chain(step.any()).zip(named) {
            let near = match need.wants() {
                Wants::Outcome {
                    near: Some(near), ..
                } => near,
                Wants::Outcome { near: None, .. } | Wants::Settled => {
                    step_bits.push(None);
                    continue;
                }
            };
            let bit = shapes.get_mut(*target).map(|shape| {
                shape.nears.push(near.clone());
                shape.k().saturating_sub(1)
            });
            step_bits.push(bit);
        }
        bits.push(step_bits);
    }

    for (step, shape) in steps.iter().zip(&shapes) {
        if codes(shape.outcomes, shape.k()).is_none() {
            return Err(PlanError::Outcomes {
                step: step.id().to_owned(),
            });
        }
    }

    Ok((shapes, bits))
}

/// The step machine: each step settles to its codes, and each need accepts
/// the codes its bit or values name.
pub(super) fn machine<Q>(
    steps: &[Step<Q>],
    targets: &[Vec<usize>],
    bits: &NeedBits,
    shapes: &[Shape],
) -> Result<rungs::Plan, PlanError> {
    let mut built = Vec::with_capacity(steps.len());
    for (((step, shape), named), step_bits) in steps.iter().zip(shapes).zip(targets).zip(bits) {
        let needs: Vec<rungs::Need> = step
            .all()
            .iter()
            .chain(step.any())
            .zip(named)
            .zip(step_bits)
            .map(|((need, target), bit)| machine_need(need, *target, *bit, shapes))
            .collect();
        let (all, any) = needs.split_at(step.all().len().min(needs.len()));

        let codes = codes(shape.outcomes, shape.k()).unwrap_or(0);
        built.push(
            rungs::Step::new(codes)
                .after(all.iter().copied())
                .after_any(any.iter().copied()),
        );
    }

    rungs::Plan::new(built).map_err(|error| PlanError::Machine(error.to_string()))
}

fn machine_need(need: &Need, target: usize, bit: Option<u8>, shapes: &[Shape]) -> rungs::Need {
    match need.wants() {
        Wants::Settled => rungs::Need::settled(target),
        Wants::Outcome { when, .. } => {
            let (outcomes, k) = shapes
                .get(target)
                .map_or((0, 0), |shape| (shape.outcomes, shape.k()));
            let values = when.as_deref().unwrap_or(&[]);
            rungs::Need::outcome(target, near::accepts(outcomes, k, bit, values))
        }
    }
}
