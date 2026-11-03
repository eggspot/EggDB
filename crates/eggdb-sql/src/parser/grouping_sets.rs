use super::{MAX_NESTING, Parser, too_deep};
use crate::ast::{Expr, ExprKind, GroupingSets, Name};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::grouping_sets::{GROUP_ID, GROUPING, GROUPING_ID};
use crate::lexer::TokenKind;

const MAX_GROUPING_SETS: usize = 4096;
const MAX_CUBE_ELEMENTS: usize = 12;

type Sets = Vec<Vec<usize>>;

fn too_many_sets(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        format!("too many grouping sets present (maximum {MAX_GROUPING_SETS})"),
    )
    .at(position)
}

fn rollup(units: &[Vec<usize>], position: usize) -> SqlResult<Sets> {
    if units.len() >= MAX_GROUPING_SETS {
        return Err(too_many_sets(position));
    }
    Ok((0..=units.len())
        .rev()
        .map(|length| units.get(..length).unwrap_or_default().concat())
        .collect())
}

fn cube(units: &[Vec<usize>], position: usize) -> SqlResult<Sets> {
    if units.len() > MAX_CUBE_ELEMENTS {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("CUBE is limited to {MAX_CUBE_ELEMENTS} elements"),
        )
        .at(position));
    }
    let count = units.len();
    Ok((0..1usize << count)
        .rev()
        .map(|mask| {
            units
                .iter()
                .enumerate()
                .filter(|(at, _)| mask & (1 << (count - 1 - at)) != 0)
                .flat_map(|(_, unit)| unit.iter().copied())
                .collect()
        })
        .collect())
}

fn cross(left: &Sets, right: &Sets, position: usize) -> SqlResult<Sets> {
    if left.len().saturating_mul(right.len()) > MAX_GROUPING_SETS {
        return Err(too_many_sets(position));
    }
    Ok(left
        .iter()
        .flat_map(|first| {
            right
                .iter()
                .map(move |second| [first.as_slice(), second.as_slice()].concat())
        })
        .collect())
}

fn single_keys(count: usize) -> Vec<Vec<usize>> {
    (0..count).map(|key| vec![key]).collect()
}

impl Parser<'_> {
    pub(super) fn group_by(&mut self) -> SqlResult<(Vec<Expr>, Option<GroupingSets>)> {
        let position = self.position();
        let distinct = self.postgresql() && self.take_word("distinct");
        if self.postgresql() && !distinct {
            self.take_word("all");
        }
        if self.is_word("all") || self.is_word("distinct") {
            return Err(self.not_supported("GROUP BY ALL and GROUP BY DISTINCT"));
        }
        let mut keys = Vec::new();
        let mut sets: Sets = vec![Vec::new()];
        let mut uses_sets = false;
        loop {
            let element = match self.grouping_construct(&mut keys)? {
                Some(element) => {
                    uses_sets = true;
                    element
                }
                None if self.mysql() || self.sqlite() => {
                    keys.push(self.expression()?);
                    vec![vec![keys.len() - 1]]
                }
                None => vec![self.grouping_unit(&mut keys)?],
            };
            sets = cross(&sets, &element, position)?;
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        if let Some(written) = self.with_rollup_or_cube(&keys, uses_sets)? {
            sets = written;
            uses_sets = true;
        }
        let grouping_sets = uses_sets.then_some(GroupingSets { sets, distinct });
        Ok((keys, grouping_sets))
    }

    fn with_rollup_or_cube(&mut self, keys: &[Expr], uses_sets: bool) -> SqlResult<Option<Sets>> {
        let rollup_word = self.is_word_at(1, "rollup") && (self.mysql() || self.sqlserver());
        let cube_word = self.is_word_at(1, "cube") && self.sqlserver();
        if !self.is_word("with") || !(rollup_word || cube_word) {
            return Ok(None);
        }
        if uses_sets {
            return Err(self.unexpected());
        }
        let position = self.position();
        self.at += 2;
        let units = single_keys(keys.len());
        if rollup_word {
            rollup(&units, position).map(Some)
        } else {
            cube(&units, position).map(Some)
        }
    }

    fn at_grouping_construct(&self) -> bool {
        let called = |word: &str| self.is_word(word) && self.peek_kind_at(1) == &TokenKind::LeftParen;
        called("rollup")
            || called("cube")
            || (self.is_word("grouping") && self.is_word_at(1, "sets"))
            || (self.peek_kind() == &TokenKind::LeftParen
                && self.peek_kind_at(1) == &TokenKind::RightParen)
    }

    fn grouping_construct(&mut self, keys: &mut Vec<Expr>) -> SqlResult<Option<Sets>> {
        if !self.at_grouping_construct() {
            return Ok(None);
        }
        if self.mysql() || self.sqlite() {
            return Err(self.unexpected());
        }
        let position = self.position();
        if self.take(&TokenKind::LeftParen) {
            self.expect(&TokenKind::RightParen)?;
            return Ok(Some(vec![Vec::new()]));
        }
        if self.take_word("rollup") {
            let units = self.grouping_units(keys)?;
            return rollup(&units, position).map(Some);
        }
        if self.take_word("cube") {
            let units = self.grouping_units(keys)?;
            return cube(&units, position).map(Some);
        }
        self.at += 2;
        self.expect(&TokenKind::LeftParen)?;
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(too_deep(position));
        }
        let listed = self.grouping_set_list(keys, position);
        self.depth -= 1;
        let sets = listed?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(sets))
    }

    fn grouping_set_list(&mut self, keys: &mut Vec<Expr>, position: usize) -> SqlResult<Sets> {
        let mut sets = Vec::new();
        loop {
            let element = match self.grouping_construct(keys)? {
                Some(element) => element,
                None => vec![self.grouping_unit(keys)?],
            };
            if sets.len() + element.len() > MAX_GROUPING_SETS {
                return Err(too_many_sets(position));
            }
            sets.extend(element);
            if !self.take(&TokenKind::Comma) {
                return Ok(sets);
            }
        }
    }

    fn grouping_units(&mut self, keys: &mut Vec<Expr>) -> SqlResult<Vec<Vec<usize>>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut units = Vec::new();
        loop {
            units.push(self.grouping_unit(keys)?);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(units)
    }

    fn grouping_unit(&mut self, keys: &mut Vec<Expr>) -> SqlResult<Vec<usize>> {
        if !self.at_parenthesized_list() {
            keys.push(self.expression()?);
            return Ok(vec![keys.len() - 1]);
        }
        self.expect(&TokenKind::LeftParen)?;
        let mut unit = Vec::new();
        loop {
            keys.push(self.expression()?);
            unit.push(keys.len() - 1);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(unit)
    }

    fn at_parenthesized_list(&self) -> bool {
        if self.peek_kind() != &TokenKind::LeftParen || self.at_subquery() {
            return false;
        }
        let mut depth = 0usize;
        let mut ahead = 0;
        loop {
            match self.peek_kind_at(ahead) {
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return false;
                    }
                }
                TokenKind::Comma if depth == 1 => return true,
                TokenKind::End => return false,
                _ => {}
            }
            ahead += 1;
        }
    }

    pub(super) fn grouping_operation(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let operation = match name.text.as_str() {
            "grouping" if !self.sqlite() => GROUPING,
            "grouping_id" if self.sqlserver() || self.oracle() => GROUPING_ID,
            "group_id" if self.oracle() => GROUP_ID,
            _ => return Ok(None),
        };
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        if operation != GROUP_ID {
            loop {
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(Expr {
            kind: ExprKind::Function {
                name: operation.to_owned(),
                arguments,
            },
            position: name.position,
        }))
    }
}
