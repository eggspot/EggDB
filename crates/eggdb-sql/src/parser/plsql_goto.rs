use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::swap_uniform_case;
use crate::plpgsql::{Block, Step, StepKind};

pub(super) fn check_gotos(block: &Block) -> SqlResult<()> {
    let mut visible = Vec::new();
    check_block(block, block, &mut visible)
}

fn check_block<'b>(unit: &Block, block: &'b Block, visible: &mut Vec<&'b [Step]>) -> SqlResult<()> {
    check_sequence(unit, &block.steps, visible)?;
    block
        .handlers
        .iter()
        .try_for_each(|handler| check_sequence(unit, &handler.steps, visible))
}

fn check_sequence<'b>(
    unit: &Block,
    steps: &'b [Step],
    visible: &mut Vec<&'b [Step]>,
) -> SqlResult<()> {
    visible.push(steps);
    let outcome = steps
        .iter()
        .try_for_each(|step| check_step(unit, step, visible));
    visible.pop();
    outcome
}

fn check_step<'b>(unit: &Block, step: &'b Step, visible: &mut Vec<&'b [Step]>) -> SqlResult<()> {
    match &step.kind {
        StepKind::Goto(label) => {
            if visible.iter().any(|steps| labels(steps, label)) {
                Ok(())
            } else if block_has_label(unit, label) {
                Err(unreachable_label(label, step.position))
            } else {
                Err(undeclared_label(label, step.position))
            }
        }
        StepKind::Block(block) => check_block(unit, block, visible),
        StepKind::If {
            branches,
            otherwise,
        } => {
            for branch in branches {
                check_sequence(unit, &branch.steps, visible)?;
            }
            otherwise
                .iter()
                .try_for_each(|steps| check_sequence(unit, steps, visible))
        }
        StepKind::Case {
            arms, otherwise, ..
        } => {
            for arm in arms {
                check_sequence(unit, &arm.steps, visible)?;
            }
            otherwise
                .iter()
                .try_for_each(|steps| check_sequence(unit, steps, visible))
        }
        StepKind::Loop(found) => check_sequence(unit, &found.body, visible),
        _ => Ok(()),
    }
}

fn labels(steps: &[Step], label: &str) -> bool {
    steps
        .iter()
        .any(|step| matches!(&step.kind, StepKind::Label(found) if found == label))
}

fn block_has_label(block: &Block, label: &str) -> bool {
    sequence_has_label(&block.steps, label)
        || block
            .handlers
            .iter()
            .any(|handler| sequence_has_label(&handler.steps, label))
}

fn sequence_has_label(steps: &[Step], label: &str) -> bool {
    steps.iter().any(|step| match &step.kind {
        StepKind::Label(found) => found == label,
        StepKind::Block(block) => block_has_label(block, label),
        StepKind::If {
            branches,
            otherwise,
        } => {
            branches
                .iter()
                .any(|branch| sequence_has_label(&branch.steps, label))
                || otherwise
                    .as_ref()
                    .is_some_and(|steps| sequence_has_label(steps, label))
        }
        StepKind::Case {
            arms, otherwise, ..
        } => {
            arms.iter().any(|arm| sequence_has_label(&arm.steps, label))
                || otherwise
                    .as_ref()
                    .is_some_and(|steps| sequence_has_label(steps, label))
        }
        StepKind::Loop(found) => sequence_has_label(&found.body, label),
        _ => false,
    })
}

fn unreachable_label(label: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!(
            "PLS-00375: illegal GOTO statement; this GOTO cannot transfer control to label '{}'",
            swap_uniform_case(label)
        ),
    )
    .with_hint(
        "A GOTO can jump to a label in its own statement sequence or an enclosing one, never into an IF, CASE, loop or block, nor from an exception handler back into its block.",
    )
    .at(position)
}

fn undeclared_label(label: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "PLS-00201: identifier '{}' must be declared",
            swap_uniform_case(label)
        ),
    )
    .at(position)
}
