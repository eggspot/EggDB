use super::{Assertion, CaseSensitivity, CharClass, MATCH_STEPS, Node, Regex, StepBudget};
use crate::error::SqlResult;
use std::collections::HashMap;

const MOST_REMEMBERED_STATES: usize = 1 << 26;

#[derive(Debug, Clone, Copy)]
enum Op {
    Char(char),
    Any,
    Class(u32),
    Assert(Assertion),
    Split(u32, u32),
    Jump(u32),
    Save(u32),
    Progress(u32),
    BackReference(u32),
    Look(u32),
    Match,
}

#[derive(Debug, Clone, Copy)]
struct Look {
    entry: u32,
    behind: bool,
    negated: bool,
}

#[derive(Debug)]
pub(crate) struct Program {
    ops: Vec<Op>,
    looks: Vec<Look>,
    slots: usize,
    groups: usize,
    longest: bool,
    back_references: bool,
}

struct Builder<'n> {
    ops: Vec<Op>,
    looks: Vec<Look>,
    pending: Vec<(usize, &'n Node)>,
    next_slot: usize,
    back_references: bool,
}

fn index(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl<'n> Builder<'n> {
    fn next(&self) -> u32 {
        index(self.ops.len())
    }

    fn emit(&mut self, op: Op) -> usize {
        self.ops.push(op);
        self.ops.len() - 1
    }

    fn patch(&mut self, at: usize, op: Op) {
        if let Some(slot) = self.ops.get_mut(at) {
            *slot = op;
        }
    }

    fn register(&mut self) -> u32 {
        let slot = self.next_slot;
        self.next_slot += 1;
        index(slot)
    }

    fn compile(&mut self, node: &'n Node) {
        match node {
            Node::Empty => {}
            Node::Literal(character) => {
                self.emit(Op::Char(*character));
            }
            Node::Any => {
                self.emit(Op::Any);
            }
            Node::Class(class) => {
                self.emit(Op::Class(index(*class)));
            }
            Node::Start => {
                self.emit(Op::Assert(Assertion::TextStart));
            }
            Node::End => {
                self.emit(Op::Assert(Assertion::TextEnd));
            }
            Node::Assertion(assertion) => {
                self.emit(Op::Assert(*assertion));
            }
            Node::Concat(nodes) => {
                for node in nodes {
                    self.compile(node);
                }
            }
            Node::Alternate(nodes) => {
                let mut jumps = Vec::with_capacity(nodes.len());
                for (at, node) in nodes.iter().enumerate() {
                    if at + 1 == nodes.len() {
                        self.compile(node);
                        break;
                    }
                    let split = self.emit(Op::Split(0, 0));
                    let first = self.next();
                    self.compile(node);
                    jumps.push(self.emit(Op::Jump(0)));
                    let second = self.next();
                    self.patch(split, Op::Split(first, second));
                }
                let end = self.next();
                for jump in jumps {
                    self.patch(jump, Op::Jump(end));
                }
            }
            Node::Repeat {
                node,
                min,
                max,
                greedy,
                ..
            } => {
                for _ in 0..*min {
                    self.compile(node);
                }
                let choice = |body: u32, end: u32| {
                    if *greedy {
                        Op::Split(body, end)
                    } else {
                        Op::Split(end, body)
                    }
                };
                match max {
                    None => {
                        let register = self.register();
                        let split = self.emit(Op::Split(0, 0));
                        let body = self.next();
                        self.emit(Op::Save(register));
                        self.compile(node);
                        self.emit(Op::Progress(register));
                        self.emit(Op::Jump(index(split)));
                        let end = self.next();
                        self.patch(split, choice(body, end));
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.emit(Op::Split(0, 0)));
                            self.compile(node);
                        }
                        let end = self.next();
                        for split in splits {
                            self.patch(split, choice(index(split + 1), end));
                        }
                    }
                }
            }
            Node::Group { index: group, node } => {
                self.emit(Op::Save(index(2 * group)));
                self.compile(node);
                self.emit(Op::Save(index(2 * group + 1)));
            }
            Node::BackReference(group) => {
                self.back_references = true;
                self.emit(Op::BackReference(index(*group)));
            }
            Node::Look {
                behind,
                negated,
                node,
            } => {
                let look = self.looks.len();
                self.looks.push(Look {
                    entry: 0,
                    behind: *behind,
                    negated: *negated,
                });
                self.pending.push((look, &**node));
                self.emit(Op::Look(index(look)));
            }
        }
    }
}

impl Program {
    pub(super) fn build(regex: &Regex) -> Self {
        let mut builder = Builder {
            ops: Vec::new(),
            looks: Vec::new(),
            pending: Vec::new(),
            next_slot: 2 * (regex.groups + 1),
            back_references: false,
        };
        builder.compile(&regex.root);
        builder.emit(Op::Match);
        while let Some((look, node)) = builder.pending.pop() {
            let entry = builder.next();
            builder.compile(node);
            builder.emit(Op::Match);
            if let Some(found) = builder.looks.get_mut(look) {
                found.entry = entry;
            }
        }
        Self {
            ops: builder.ops,
            looks: builder.looks,
            slots: builder.next_slot,
            groups: regex.groups,
            longest: regex.root.prefers_longest().unwrap_or(true),
            back_references: builder.back_references,
        }
    }
}

struct Visited {
    bits: Vec<u64>,
    touched: Vec<usize>,
    positions: usize,
}

impl Visited {
    fn new(ops: usize, positions: usize) -> Option<Self> {
        let states = ops.checked_mul(positions)?;
        if states > MOST_REMEMBERED_STATES {
            return None;
        }
        Some(Self {
            bits: vec![0; states.div_ceil(64)],
            touched: Vec::new(),
            positions,
        })
    }

    fn first_visit(&mut self, pc: u32, at: usize) -> bool {
        let state = (pc as usize)
            .saturating_mul(self.positions)
            .saturating_add(at);
        let (word, bit) = (state / 64, 1u64 << (state % 64));
        let Some(slot) = self.bits.get_mut(word) else {
            return true;
        };
        if *slot & bit != 0 {
            return false;
        }
        if *slot == 0 {
            self.touched.push(word);
        }
        *slot |= bit;
        true
    }

    fn clear(&mut self) {
        for word in self.touched.drain(..) {
            if let Some(slot) = self.bits.get_mut(word) {
                *slot = 0;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Goal {
    First,
    EndingAt(usize),
    Longest,
    Shortest,
}

enum Frame {
    Resume { pc: u32, at: usize },
    Restore { slot: usize, value: Option<usize> },
}

pub(crate) type Span = (usize, usize);

pub(crate) struct Finder<'r, 't> {
    program: &'r Program,
    classes: &'r [CharClass],
    case: CaseSensitivity,
    newline_excluded: bool,
    text: &'t [char],
    visited: Option<Visited>,
    looks: HashMap<(u32, usize), bool>,
}

fn is_word(character: Option<char>) -> bool {
    character.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn holds(assertion: Assertion, text: &[char], at: usize) -> bool {
    let before = at.checked_sub(1).and_then(|previous| text.get(previous)).copied();
    let after = text.get(at).copied();
    match assertion {
        Assertion::TextStart => at == 0,
        Assertion::TextEnd => at >= text.len(),
        Assertion::LineStart => before.is_none_or(|character| character == '\n'),
        Assertion::LineEnd => after.is_none_or(|character| character == '\n'),
        Assertion::WordStart => !is_word(before) && is_word(after),
        Assertion::WordEnd => is_word(before) && !is_word(after),
        Assertion::WordBoundary => is_word(before) != is_word(after),
        Assertion::NotWordBoundary => is_word(before) == is_word(after),
    }
}

impl<'r, 't> Finder<'r, 't> {
    pub(super) fn new(regex: &'r Regex, text: &'t [char]) -> Self {
        let program = regex.search_program();
        let visited = if program.back_references {
            None
        } else {
            Visited::new(program.ops.len(), text.len().saturating_add(1))
        };
        Self {
            program,
            classes: &regex.classes,
            case: regex.options.case,
            newline_excluded: regex.options.newline_excluded,
            text,
            visited,
            looks: HashMap::new(),
        }
    }

    pub(crate) fn find(&mut self, from: usize) -> SqlResult<Option<Vec<Option<Span>>>> {
        let mut budget = StepBudget::new(MATCH_STEPS);
        let goal = if self.program.longest {
            Goal::Longest
        } else {
            Goal::Shortest
        };
        if let Some(visited) = self.visited.as_mut() {
            visited.clear();
        }
        let mut slots = vec![None; self.program.slots];
        for start in from..=self.text.len() {
            slots.fill(None);
            if let Some(end) = self.run(0, start, goal, &mut slots, true, &mut budget)? {
                let mut spans = Vec::with_capacity(self.program.groups + 1);
                spans.push(Some((start, end)));
                for group in 1..=self.program.groups {
                    let begin = slots.get(2 * group).copied().flatten();
                    let finish = slots.get(2 * group + 1).copied().flatten();
                    spans.push(match (begin, finish) {
                        (Some(begin), Some(finish)) if begin <= finish => Some((begin, finish)),
                        _ => None,
                    });
                }
                return Ok(Some(spans));
            }
        }
        Ok(None)
    }

    pub(crate) fn any(&mut self, budget: &mut StepBudget) -> SqlResult<bool> {
        if let Some(visited) = self.visited.as_mut() {
            visited.clear();
        }
        let mut slots = vec![None; self.program.slots];
        for start in 0..=self.text.len() {
            slots.fill(None);
            if self
                .run(0, start, Goal::First, &mut slots, true, budget)?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn folded(&self, character: char) -> char {
        match self.case {
            CaseSensitivity::Sensitive => character,
            CaseSensitivity::Insensitive => character.to_ascii_lowercase(),
        }
    }

    fn in_class(&self, class: u32, character: char) -> bool {
        let Some(class) = self.classes.get(class as usize) else {
            return false;
        };
        if self.newline_excluded && character == '\n' && class.negated {
            return false;
        }
        class.matches(character, self.case)
    }

    fn back_reference(&self, group: u32, at: usize, slots: &[Option<usize>]) -> Option<usize> {
        let group = group as usize;
        let begin = slots.get(2 * group).copied().flatten()?;
        let finish = slots.get(2 * group + 1).copied().flatten()?;
        let wanted = self.text.get(begin..finish)?;
        let found = self.text.get(at..at.checked_add(wanted.len())?)?;
        let same = wanted
            .iter()
            .zip(found)
            .all(|(left, right)| self.folded(*left) == self.folded(*right));
        same.then_some(wanted.len())
    }

    fn look_holds(&mut self, look: u32, at: usize, budget: &mut StepBudget) -> SqlResult<bool> {
        if let Some(&known) = self.looks.get(&(look, at)) {
            return Ok(known);
        }
        let Some(&Look {
            entry,
            behind,
            negated,
        }) = self.program.looks.get(look as usize)
        else {
            return Ok(false);
        };
        let mut slots = vec![None; self.program.slots];
        let found = if behind {
            let mut found = false;
            for start in (0..=at).rev() {
                slots.fill(None);
                if self
                    .run(entry, start, Goal::EndingAt(at), &mut slots, false, budget)?
                    .is_some()
                {
                    found = true;
                    break;
                }
            }
            found
        } else {
            self.run(entry, at, Goal::First, &mut slots, false, budget)?
                .is_some()
        };
        let result = found != negated;
        self.looks.insert((look, at), result);
        Ok(result)
    }

    fn run(
        &mut self,
        entry: u32,
        start: usize,
        goal: Goal,
        slots: &mut [Option<usize>],
        remembered: bool,
        budget: &mut StepBudget,
    ) -> SqlResult<Option<usize>> {
        let program = self.program;
        let text = self.text;
        let mut stack = vec![Frame::Resume {
            pc: entry,
            at: start,
        }];
        let mut best: Option<(usize, Vec<Option<usize>>)> = None;
        while let Some(frame) = stack.pop() {
            let (mut pc, mut at) = match frame {
                Frame::Restore { slot, value } => {
                    if let Some(kept) = slots.get_mut(slot) {
                        *kept = value;
                    }
                    continue;
                }
                Frame::Resume { pc, at } => (pc, at),
            };
            loop {
                budget.spend(1)?;
                if let Goal::EndingAt(end) = goal
                    && at > end
                {
                    break;
                }
                if remembered
                    && let Some(visited) = self.visited.as_mut()
                    && !visited.first_visit(pc, at)
                {
                    break;
                }
                let Some(&op) = program.ops.get(pc as usize) else {
                    break;
                };
                match op {
                    Op::Char(expected) => match text.get(at) {
                        Some(&character) if self.folded(character) == expected => {
                            pc += 1;
                            at += 1;
                        }
                        _ => break,
                    },
                    Op::Any => match text.get(at) {
                        Some(&character) if !(self.newline_excluded && character == '\n') => {
                            pc += 1;
                            at += 1;
                        }
                        _ => break,
                    },
                    Op::Class(class) => match text.get(at) {
                        Some(&character) if self.in_class(class, character) => {
                            pc += 1;
                            at += 1;
                        }
                        _ => break,
                    },
                    Op::Assert(assertion) => {
                        if !holds(assertion, text, at) {
                            break;
                        }
                        pc += 1;
                    }
                    Op::Split(first, second) => {
                        stack.push(Frame::Resume { pc: second, at });
                        pc = first;
                    }
                    Op::Jump(target) => pc = target,
                    Op::Save(slot) => {
                        let slot = slot as usize;
                        if let Some(kept) = slots.get_mut(slot) {
                            stack.push(Frame::Restore { slot, value: *kept });
                            *kept = Some(at);
                        }
                        pc += 1;
                    }
                    Op::Progress(slot) => {
                        if slots.get(slot as usize).copied().flatten() == Some(at) {
                            break;
                        }
                        pc += 1;
                    }
                    Op::BackReference(group) => match self.back_reference(group, at, slots) {
                        Some(length) => {
                            at += length;
                            pc += 1;
                        }
                        None => break,
                    },
                    Op::Look(look) => {
                        if !self.look_holds(look, at, budget)? {
                            break;
                        }
                        pc += 1;
                    }
                    Op::Match => match goal {
                        Goal::First => return Ok(Some(at)),
                        Goal::EndingAt(end) => {
                            if at == end {
                                return Ok(Some(at));
                            }
                            break;
                        }
                        Goal::Longest | Goal::Shortest => {
                            let better = match &best {
                                None => true,
                                Some((found, _)) if goal == Goal::Longest => at > *found,
                                Some((found, _)) => at < *found,
                            };
                            if better {
                                best = Some((at, slots.to_vec()));
                            }
                            break;
                        }
                    },
                }
            }
        }
        Ok(best.map(|(end, kept)| {
            if kept.len() == slots.len() {
                slots.copy_from_slice(&kept);
            }
            end
        }))
    }
}
