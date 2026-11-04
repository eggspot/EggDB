use super::Parser;
use crate::ast::{
    AlterAction, AlterTable, CreateTable, ExprKind, Name, NamedPartition, NamedPartitionBound,
    OnCommit, PartitionBoundSpec, PartitionBoundValue, PartitionBy, PartitionKeyPart, PartitionOf,
    PartitionStrategy, Statement,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

const MAX_PARTITION_KEY_COLUMNS: usize = 32;
const MAX_NAMED_PARTITIONS: usize = 8192;
const MAX_BOUND_VALUES: usize = 65_536;

impl Parser<'_> {
    pub(super) fn create_partition_of(
        &mut self,
        name: Name,
        temporary: Option<OnCommit>,
        if_not_exists: bool,
    ) -> SqlResult<Statement> {
        self.expect_word("partition")?;
        self.expect_word("of")?;
        let parent = self.table_name()?;
        if self.peek_kind() == &TokenKind::LeftParen {
            return Err(self.not_supported("column constraints in CREATE TABLE ... PARTITION OF"));
        }
        let bound = self.partition_bound()?;
        let partition_by = if self.is_word("partition") && self.is_word_at(1, "by") {
            Some(self.partition_by()?)
        } else {
            None
        };
        Ok(Statement::CreateTable(CreateTable {
            name,
            temporary,
            if_not_exists,
            columns: Vec::new(),
            primary_key: None,
            primary_key_name: None,
            indexes: Vec::new(),
            constraints: Vec::new(),
            comment: None,
            partition_by,
            partition_of: Some(PartitionOf { parent, bound }),
            sqlite_text: None,
        }))
    }

    pub(super) fn partition_bound(&mut self) -> SqlResult<PartitionBoundSpec> {
        if self.take_word("default") {
            return Ok(PartitionBoundSpec::Default);
        }
        self.expect_word("for")?;
        self.expect_word("values")?;
        if self.take_word("from") {
            let from = self.bound_values()?;
            self.expect_word("to")?;
            let to = self.bound_values()?;
            return Ok(PartitionBoundSpec::Range { from, to });
        }
        if self.take_word("in") {
            return Ok(PartitionBoundSpec::List(self.bound_values()?));
        }
        self.expect_word("with")?;
        self.expect(&TokenKind::LeftParen)?;
        self.expect_word("modulus")?;
        let modulus = self.partition_count()?;
        self.expect(&TokenKind::Comma)?;
        self.expect_word("remainder")?;
        let remainder = self.partition_number()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(PartitionBoundSpec::Hash { modulus, remainder })
    }

    pub(super) fn partition_by(&mut self) -> SqlResult<PartitionBy> {
        let position = self.position();
        self.expect_word("partition")?;
        self.expect_word("by")?;
        let strategy = if self.take_word("range") {
            PartitionStrategy::Range
        } else if self.take_word("list") {
            PartitionStrategy::List
        } else if self.take_word("hash") || (self.mysql() && self.take_word("key")) {
            PartitionStrategy::Hash
        } else {
            return Err(self.unexpected());
        };
        if !self.postgresql() {
            self.take_word("columns");
        }
        let key = self.partition_key()?;
        let mut partition_by = PartitionBy {
            strategy,
            key,
            partitions: Vec::new(),
            hash_partitions: None,
            position,
        };
        if self.postgresql() {
            return Ok(partition_by);
        }
        if self.take_word("partitions") {
            partition_by.hash_partitions = Some(self.partition_count()?);
        }
        if self.peek_kind() == &TokenKind::LeftParen {
            self.at += 1;
            loop {
                if partition_by.partitions.len() >= MAX_NAMED_PARTITIONS {
                    return Err(too_many_partitions(self.position()));
                }
                partition_by
                    .partitions
                    .push(self.named_partition(strategy)?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if self.oracle() {
            self.oracle_row_movement()?;
        }
        Ok(partition_by)
    }

    fn partition_key(&mut self) -> SqlResult<Vec<PartitionKeyPart>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut parts = Vec::new();
        loop {
            if parts.len() >= MAX_PARTITION_KEY_COLUMNS {
                return Err(SqlError::new(
                    SqlState::TOO_MANY_COLUMNS,
                    format!(
                        "cannot use more than {MAX_PARTITION_KEY_COLUMNS} columns in partition key"
                    ),
                )
                .at(self.position()));
            }
            parts.push(self.partition_key_part()?);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(parts)
    }

    fn partition_key_part(&mut self) -> SqlResult<PartitionKeyPart> {
        let first_token = self.at;
        let position = self.position();
        let takes_expressions = self.postgresql() || self.mysql();
        if self.peek_kind() == &TokenKind::LeftParen {
            if !takes_expressions {
                return Err(self.not_supported("expressions in a partition key"));
            }
            self.at += 1;
            let start = self.position();
            let expr = self.expression()?;
            let end = self.previous_end();
            self.expect(&TokenKind::RightParen)?;
            let text = self.text.get(start..end).unwrap_or("").to_owned();
            return Ok(key_part(expr, text, position));
        }
        let column = self.name()?;
        if self.peek_kind() != &TokenKind::LeftParen && self.at_partition_key_end() {
            return Ok(PartitionKeyPart::Column(column));
        }
        if !takes_expressions {
            return Err(self.not_supported("expressions in a partition key"));
        }
        self.at = first_token;
        let expr = self.expression()?;
        let end = self.previous_end();
        let text = self.text.get(position..end).unwrap_or("").to_owned();
        Ok(key_part(expr, text, position))
    }

    fn at_partition_key_end(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Comma | TokenKind::RightParen)
    }

    fn named_partition(&mut self, strategy: PartitionStrategy) -> SqlResult<NamedPartition> {
        self.expect_word("partition")?;
        let name = self.name()?;
        let bound = match strategy {
            PartitionStrategy::Range => {
                self.expect_word("values")?;
                self.expect_word("less")?;
                self.expect_word("than")?;
                if self.take_word("maxvalue") {
                    NamedPartitionBound::LessThan(vec![PartitionBoundValue::MaxValue])
                } else {
                    NamedPartitionBound::LessThan(self.bound_values()?)
                }
            }
            PartitionStrategy::List => {
                self.expect_word("values")?;
                if self.oracle() && self.take_word("default") {
                    NamedPartitionBound::In(Vec::new())
                } else {
                    self.take_word("in");
                    NamedPartitionBound::In(self.bound_values()?)
                }
            }
            PartitionStrategy::Hash => NamedPartitionBound::Hash,
        };
        self.skip_partition_options()?;
        Ok(NamedPartition { name, bound })
    }

    fn skip_partition_options(&mut self) -> SqlResult<()> {
        loop {
            if self.take_word("tablespace") {
                self.take_operator("=");
                self.name()?;
            } else if self.mysql() && (self.is_word("engine") || self.is_word("comment")) {
                self.at += 1;
                self.take_operator("=");
                self.at += 1;
            } else if self.oracle()
                && ["logging", "nologging", "compress", "nocompress"]
                    .iter()
                    .any(|word| self.is_word(word))
            {
                self.at += 1;
            } else {
                return Ok(());
            }
        }
    }

    pub(super) fn alter_index(&mut self) -> SqlResult<Statement> {
        self.expect_word("index")?;
        let if_exists = self.is_word("if") && self.is_word_at(1, "exists");
        if if_exists {
            self.at += 2;
        }
        let name = self.table_name()?;
        if !(self.is_word("attach") && self.is_word_at(1, "partition")) {
            return Err(self.not_supported("ALTER INDEX other than ATTACH PARTITION"));
        }
        self.at += 2;
        let partition_index = self.table_name()?;
        Ok(Statement::AlterTable(AlterTable {
            name,
            if_exists,
            actions: vec![AlterAction::AttachIndex(partition_index)],
        }))
    }

    fn oracle_row_movement(&mut self) -> SqlResult<()> {
        if (self.is_word("enable") || self.is_word("disable"))
            && self.is_word_at(1, "row")
            && self.is_word_at(2, "movement")
        {
            self.at += 3;
        }
        Ok(())
    }

    fn bound_values(&mut self) -> SqlResult<Vec<PartitionBoundValue>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut values = Vec::new();
        loop {
            if values.len() >= MAX_BOUND_VALUES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("a partition bound can list at most {MAX_BOUND_VALUES} values"),
                )
                .at(self.position()));
            }
            let value = if self.take_word("minvalue") {
                PartitionBoundValue::MinValue
            } else if self.take_word("maxvalue") {
                PartitionBoundValue::MaxValue
            } else {
                PartitionBoundValue::Value(self.expression()?)
            };
            values.push(value);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(values)
    }

    fn partition_count(&mut self) -> SqlResult<u32> {
        let position = self.position();
        let count = self.partition_number()?;
        if count == 0 {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "the number of partitions must be a positive integer",
            )
            .at(position));
        }
        Ok(count)
    }

    fn partition_number(&mut self) -> SqlResult<u32> {
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let Ok(number) = digits.parse::<u32>() else {
            return Err(self.unexpected());
        };
        if number as usize > MAX_NAMED_PARTITIONS {
            return Err(too_many_partitions(self.position()));
        }
        self.at += 1;
        Ok(number)
    }
}

fn key_part(expr: crate::ast::Expr, text: String, position: usize) -> PartitionKeyPart {
    if let ExprKind::Column {
        qualifier: None,
        name,
    } = &expr.kind
    {
        return PartitionKeyPart::Column(Name {
            text: name.clone(),
            position,
        });
    }
    PartitionKeyPart::Expression {
        expr,
        text,
        position,
    }
}

fn too_many_partitions(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a table can have at most {MAX_NAMED_PARTITIONS} partitions"),
    )
    .at(position)
}
