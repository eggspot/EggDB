use super::Parser;
use crate::ast::{
    AlterAction, AlterTable, CreateTable, ExprKind, Name, NamedPartition, NamedPartitionBound,
    OnCommit, PartitionBoundSpec, PartitionBoundValue, PartitionBy, PartitionKeyPart, PartitionOf,
    PartitionSelection, PartitionStrategy, SplitPoint, Statement, SubpartitionBy,
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
            system_versioning: None,
            rowversion_column: None,
            lsm_storage: false,
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
        let mut by_key = false;
        let strategy = if self.take_word("range") {
            PartitionStrategy::Range
        } else if self.take_word("list") {
            PartitionStrategy::List
        } else if self.take_word("hash") {
            PartitionStrategy::Hash
        } else if self.mysql() && self.take_word("key") {
            by_key = true;
            PartitionStrategy::Hash
        } else {
            return Err(self.unexpected());
        };
        let by_columns = !self.postgresql() && self.take_word("columns");
        let mysql_integer_key = self.mysql() && !by_key && !by_columns;
        let key = self.partition_key()?;
        let mut partition_by = PartitionBy {
            strategy,
            key,
            partitions: Vec::new(),
            hash_partitions: None,
            mysql_integer_key,
            subpartition_by: None,
            row_movement: None,
            interval: None,
            position,
            scheme: None,
        };
        if self.postgresql() {
            return Ok(partition_by);
        }
        if self.oracle() && self.is_word("interval") && self.peek_kind_at(1) == &TokenKind::LeftParen {
            self.at += 2;
            let start = self.position();
            self.expression()?;
            let end = self.previous_end();
            self.expect(&TokenKind::RightParen)?;
            partition_by.interval = Some(self.text.get(start..end).unwrap_or("").to_owned());
            if self.is_word("store") && self.is_word_at(1, "in") {
                self.at += 2;
                self.expect(&TokenKind::LeftParen)?;
                self.name()?;
                while self.take(&TokenKind::Comma) {
                    self.name()?;
                }
                self.expect(&TokenKind::RightParen)?;
            }
        }
        if self.take_word("partitions") {
            partition_by.hash_partitions = Some(self.partition_count()?);
        }
        if self.is_word("subpartition") && self.is_word_at(1, "by") {
            if strategy == PartitionStrategy::Hash {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    "It is only possible to mix RANGE/LIST partitioning with HASH/KEY partitioning for subpartitioning",
                )
                .with_vendor_code(1500)
                .at(self.position()));
            }
            partition_by.subpartition_by = Some(Box::new(self.subpartition_by()?));
        }
        let subpartition_strategy = partition_by
            .subpartition_by
            .as_ref()
            .map(|sub| sub.strategy);
        if self.peek_kind() == &TokenKind::LeftParen {
            self.at += 1;
            loop {
                if partition_by.partitions.len() >= MAX_NAMED_PARTITIONS {
                    return Err(too_many_partitions(self.position()));
                }
                partition_by
                    .partitions
                    .push(self.named_partition(strategy, subpartition_strategy)?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if self.oracle() {
            partition_by.row_movement = self.oracle_row_movement();
        }
        Ok(partition_by)
    }

    fn subpartition_by(&mut self) -> SqlResult<SubpartitionBy> {
        let position = self.position();
        self.expect_word("subpartition")?;
        self.expect_word("by")?;
        let mut by_key = false;
        let strategy = if self.take_word("hash") {
            PartitionStrategy::Hash
        } else if self.mysql() && self.take_word("key") {
            by_key = true;
            PartitionStrategy::Hash
        } else if self.oracle() && self.take_word("range") {
            PartitionStrategy::Range
        } else if self.oracle() && self.take_word("list") {
            PartitionStrategy::List
        } else {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                "It is only possible to mix RANGE/LIST partitioning with HASH/KEY partitioning for subpartitioning",
            )
            .with_vendor_code(1500)
            .at(self.position()));
        };
        if by_key {
            self.take_word("algorithm");
            if self.take_operator("=") {
                self.partition_number()?;
            }
        }
        let key = self.partition_key()?;
        let mut count = if self.take_word("subpartitions") {
            Some(self.partition_count()?)
        } else {
            None
        };
        let mut template = Vec::new();
        if self.oracle() && self.is_word("subpartition") && self.is_word_at(1, "template") {
            self.at += 2;
            if self.peek_kind() == &TokenKind::LeftParen {
                template = self.subpartition_definitions(Some(strategy))?;
                if template.is_empty() {
                    return Err(self.unexpected());
                }
            } else {
                if strategy != PartitionStrategy::Hash {
                    return Err(SqlError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "a subpartition template of a range or list subpartitioning names its subpartitions",
                    )
                    .with_oracle_code(14160)
                    .at(self.position()));
                }
                count = Some(self.partition_count()?);
            }
        }
        Ok(SubpartitionBy {
            strategy,
            key,
            count,
            template,
            mysql_integer_key: self.mysql() && !by_key,
            position,
        })
    }

    fn subpartition_definitions(
        &mut self,
        strategy: Option<PartitionStrategy>,
    ) -> SqlResult<Vec<NamedPartition>> {
        let Some(strategy) = strategy else {
            return Ok(Vec::new());
        };
        if !(self.peek_kind() == &TokenKind::LeftParen && self.is_word_at(1, "subpartition")) {
            return Ok(Vec::new());
        }
        self.at += 1;
        let mut subpartitions = Vec::new();
        loop {
            if subpartitions.len() >= MAX_NAMED_PARTITIONS {
                return Err(too_many_partitions(self.position()));
            }
            self.expect_word("subpartition")?;
            let name = self.name()?;
            let bound = self.named_bound(strategy)?;
            self.skip_partition_options()?;
            subpartitions.push(NamedPartition {
                name,
                bound,
                subpartitions: Vec::new(),
            });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(subpartitions)
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
            let text = self.text_without_column_qualifiers(&expr, start, end);
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
        let expr = if self.postgresql() {
            let call = self.primary()?;
            if !self.at_partition_key_end() {
                return Err(self.unexpected());
            }
            call
        } else {
            self.expression()?
        };
        let end = self.previous_end();
        let text = self.text_without_column_qualifiers(&expr, position, end);
        Ok(key_part(expr, text, position))
    }

    pub(super) fn text_without_column_qualifiers(
        &self,
        expr: &crate::ast::Expr,
        start: usize,
        end: usize,
    ) -> String {
        let mut qualified = Vec::new();
        qualified_column_starts(expr, &mut qualified);
        qualified.sort_unstable();
        qualified.dedup();
        let mut out = String::with_capacity(end.saturating_sub(start));
        let mut at = start;
        for begin in qualified {
            if begin < at || begin >= end {
                continue;
            }
            let Some(column_start) = self.column_name_start(begin, end) else {
                continue;
            };
            out.push_str(self.text.get(at..begin).unwrap_or(""));
            at = column_start;
        }
        out.push_str(self.text.get(at..end).unwrap_or(""));
        out
    }

    fn column_name_start(&self, begin: usize, end: usize) -> Option<usize> {
        let is_name = |at: usize| {
            matches!(
                self.tokens.get(at).map(|token| &token.kind),
                Some(TokenKind::Word(_) | TokenKind::QuotedIdentifier(_))
            )
        };
        let first = self.tokens.iter().position(|token| token.start == begin)?;
        let mut last = first;
        for _ in 0..2 {
            if self.tokens.get(last + 1).map(|token| &token.kind) == Some(&TokenKind::Dot)
                && is_name(last + 2)
            {
                last += 2;
            }
        }
        let column = self.tokens.get(last)?;
        (last > first && is_name(first) && column.end <= end).then_some(column.start)
    }

    fn at_partition_key_end(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Comma | TokenKind::RightParen)
    }

    fn named_partition(
        &mut self,
        strategy: PartitionStrategy,
        subpartition_strategy: Option<PartitionStrategy>,
    ) -> SqlResult<NamedPartition> {
        self.expect_word("partition")?;
        let name = self.name()?;
        let bound = self.named_bound(strategy)?;
        self.skip_partition_options()?;
        let subpartitions = self.subpartition_definitions(subpartition_strategy)?;
        self.skip_partition_options()?;
        Ok(NamedPartition {
            name,
            bound,
            subpartitions,
        })
    }

    fn named_bound(&mut self, strategy: PartitionStrategy) -> SqlResult<NamedPartitionBound> {
        Ok(match strategy {
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
                if self.oracle() && self.take_oracle_default_values() {
                    NamedPartitionBound::In(Vec::new())
                } else {
                    self.take_word("in");
                    NamedPartitionBound::In(self.bound_values()?)
                }
            }
            PartitionStrategy::Hash => NamedPartitionBound::Hash,
        })
    }

    pub(super) fn partition_definition(&mut self) -> SqlResult<NamedPartition> {
        self.take_word("partition");
        let name = self.name()?;
        let bound = if self.is_word("values")
            && self.is_word_at(1, "less")
            && self.is_word_at(2, "than")
        {
            self.at += 3;
            if self.take_word("maxvalue") {
                NamedPartitionBound::LessThan(vec![PartitionBoundValue::MaxValue])
            } else {
                NamedPartitionBound::LessThan(self.bound_values()?)
            }
        } else if self.take_word("values") {
            if self.oracle() && self.take_oracle_default_values() {
                NamedPartitionBound::In(Vec::new())
            } else {
                self.take_word("in");
                NamedPartitionBound::In(self.bound_values()?)
            }
        } else {
            NamedPartitionBound::Hash
        };
        self.skip_partition_options()?;
        Ok(NamedPartition {
            name,
            bound,
            subpartitions: Vec::new(),
        })
    }

    pub(super) fn partition_definitions(&mut self) -> SqlResult<Vec<NamedPartition>> {
        let parenthesized = self.take(&TokenKind::LeftParen);
        let mut partitions = vec![self.partition_definition()?];
        while self.take(&TokenKind::Comma) {
            if !parenthesized && !self.is_word("partition") {
                self.at -= 1;
                break;
            }
            partitions.push(self.partition_definition()?);
        }
        if parenthesized {
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(partitions)
    }

    pub(super) fn partition_names(&mut self) -> SqlResult<Vec<Name>> {
        let mut names = vec![self.name()?];
        while self.peek_kind() == &TokenKind::Comma && !self.is_word_at(1, "partition") {
            self.at += 1;
            if self.is_word("add")
                || self.is_word("drop")
                || self.is_word("modify")
                || self.is_word("truncate")
            {
                self.at -= 1;
                break;
            }
            names.push(self.name()?);
        }
        Ok(names)
    }

    pub(super) fn partition_alter_action(
        &mut self,
        actions: &mut Vec<AlterAction>,
    ) -> SqlResult<bool> {
        if !(self.mysql() || self.oracle()) {
            return Ok(false);
        }
        let position = self.position();
        let partition_word = |parser: &Self, at: usize| {
            parser.is_word_at(at, "partition") || parser.is_word_at(at, "partitions")
        };
        if self.is_word("add") && partition_word(self, 1) {
            self.at += 1;
            if self.mysql() && self.take_word("partitions") {
                let count = self.partition_count()?;
                actions.push(AlterAction::AddPartitions {
                    partitions: Vec::new(),
                    hash_count: Some(count),
                    position,
                });
                return Ok(true);
            }
            self.at += 1;
            let partitions = if self.peek_kind() == &TokenKind::LeftParen {
                self.partition_definitions()?
            } else {
                let mut first = vec![self.partition_definition()?];
                while self.peek_kind() == &TokenKind::Comma && self.is_word_at(1, "partition") {
                    self.at += 1;
                    first.push(self.partition_definition()?);
                }
                first
            };
            actions.push(AlterAction::AddPartitions {
                partitions,
                hash_count: None,
                position,
            });
            return Ok(true);
        }
        if self.oracle() && self.is_word("drop") && self.is_word_at(1, "subpartition") {
            self.at += 2;
            actions.push(AlterAction::DropSubpartitions(self.partition_names()?));
            self.skip_oracle_index_clauses();
            return Ok(true);
        }
        if self.oracle() && self.is_word("truncate") && self.is_word_at(1, "subpartition") {
            self.at += 2;
            actions.push(AlterAction::TruncateSubpartitions(self.partition_names()?));
            self.skip_oracle_index_clauses();
            return Ok(true);
        }
        if self.is_word("drop") && partition_word(self, 1) {
            self.at += 2;
            actions.push(AlterAction::DropPartitions(self.partition_names()?));
            self.skip_oracle_index_clauses();
            return Ok(true);
        }
        if self.is_word("truncate") && partition_word(self, 1) {
            self.at += 2;
            actions.push(AlterAction::TruncatePartitions(self.partition_names()?));
            self.skip_oracle_index_clauses();
            return Ok(true);
        }
        if self.mysql() && self.is_word("reorganize") && self.is_word_at(1, "partition") {
            self.at += 2;
            let from = if self.is_word("into") {
                Vec::new()
            } else {
                self.partition_names()?
            };
            self.expect_word("into")?;
            let into = self.partition_definitions()?;
            actions.push(AlterAction::ReorganizePartitions {
                from,
                into,
                position,
            });
            return Ok(true);
        }
        if self.oracle() && self.is_word("split") && self.is_word_at(1, "partition") {
            self.at += 2;
            let partition = self.name()?;
            let point = if self.take_word("at") {
                SplitPoint::At(self.bound_values()?)
            } else {
                self.expect_word("values")?;
                SplitPoint::Values(self.bound_values()?)
            };
            self.expect_word("into")?;
            self.expect(&TokenKind::LeftParen)?;
            self.expect_word("partition")?;
            let first = self.name()?;
            self.skip_partition_options()?;
            self.expect(&TokenKind::Comma)?;
            self.expect_word("partition")?;
            let second = self.name()?;
            self.skip_partition_options()?;
            self.expect(&TokenKind::RightParen)?;
            self.skip_oracle_index_clauses();
            actions.push(AlterAction::SplitPartition {
                partition,
                point,
                into: [first, second],
            });
            return Ok(true);
        }
        if self.mysql()
            && ["analyze", "check", "optimize", "repair", "rebuild"]
                .iter()
                .any(|word| self.is_word(word))
            && self.is_word_at(1, "partition")
        {
            self.at += 2;
            actions.push(AlterAction::MaintainPartitions(self.partition_names()?));
            return Ok(true);
        }
        if self.mysql() && self.is_word("coalesce") && self.is_word_at(1, "partition") {
            self.at += 2;
            let count = self.partition_count()?;
            actions.push(AlterAction::CoalescePartitions { count, position });
            return Ok(true);
        }
        if self.oracle()
            && (self.is_word("enable") || self.is_word("disable"))
            && self.is_word_at(1, "row")
            && self.is_word_at(2, "movement")
        {
            if let Some(enabled) = self.oracle_row_movement() {
                actions.push(AlterAction::RowMovement(enabled));
            }
            return Ok(true);
        }
        if self.oracle() && self.is_word("merge") && self.is_word_at(1, "partitions") {
            self.at += 2;
            let first = self.name()?;
            let last = if self.is_word("to") {
                self.at += 1;
                Some(self.name()?)
            } else {
                None
            };
            let mut from = vec![first];
            while last.is_none() && self.take(&TokenKind::Comma) {
                from.push(self.name()?);
            }
            self.expect_word("into")?;
            self.expect_word("partition")?;
            let into = self.name()?;
            self.skip_partition_options()?;
            self.skip_oracle_index_clauses();
            actions.push(match (last, from.pop()) {
                (Some(last), Some(first)) => AlterAction::MergePartitionSpan { first, last, into },
                (_, Some(only)) => {
                    from.push(only);
                    AlterAction::MergePartitions { from, into }
                }
                (_, None) => AlterAction::MergePartitions { from, into },
            });
            return Ok(true);
        }
        if self.oracle()
            && self.is_word("modify")
            && self.is_word_at(1, "partition")
            && (self.is_word_at(3, "add") || self.is_word_at(3, "drop"))
            && self.is_word_at(4, "values")
        {
            self.at += 2;
            let partition = self.name()?;
            let adding = self.take_word("add");
            if !adding {
                self.expect_word("drop")?;
            }
            self.expect_word("values")?;
            let values = self.bound_values()?;
            self.skip_oracle_index_clauses();
            actions.push(AlterAction::ModifyPartitionValues {
                partition,
                values,
                adding,
            });
            return Ok(true);
        }
        if self.is_word("exchange") && self.is_word_at(1, "partition") {
            self.at += 2;
            let partition = self.name()?;
            self.expect_word("with")?;
            self.expect_word("table")?;
            let table = self.table_name()?;
            self.skip_oracle_index_clauses();
            let mut validate = true;
            if self.is_word("with") && self.is_word_at(1, "validation") {
                self.at += 2;
            } else if self.is_word("without") && self.is_word_at(1, "validation") {
                self.at += 2;
                validate = false;
            }
            self.skip_oracle_index_clauses();
            actions.push(AlterAction::ExchangePartition {
                partition,
                table,
                validate,
            });
            return Ok(true);
        }
        Ok(false)
    }

    fn skip_oracle_index_clauses(&mut self) {
        loop {
            if (self.is_word("including") || self.is_word("excluding"))
                && self.is_word_at(1, "indexes")
            {
                self.at += 2;
            } else if self.is_word("update") && self.is_word_at(1, "global") {
                self.at += 2;
                self.take_word("indexes");
            } else if self.is_word("update") && self.is_word_at(1, "indexes") {
                self.at += 2;
            } else {
                return;
            }
        }
    }

    fn take_oracle_default_values(&mut self) -> bool {
        if self.take_word("default") {
            return true;
        }
        let parenthesized = self.peek_kind() == &TokenKind::LeftParen
            && self.is_word_at(1, "default")
            && self.peek_kind_at(2) == &TokenKind::RightParen;
        if parenthesized {
            self.at += 3;
        }
        parenthesized
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

    pub(super) fn oracle_row_movement(&mut self) -> Option<bool> {
        if (self.is_word("enable") || self.is_word("disable"))
            && self.is_word_at(1, "row")
            && self.is_word_at(2, "movement")
        {
            let enabled = self.is_word("enable");
            self.at += 3;
            return Some(enabled);
        }
        None
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

    pub(super) fn partition_selection(&mut self) -> SqlResult<Option<Box<PartitionSelection>>> {
        let selects_partitions = self.mysql() || self.oracle();
        let at_keyword =
            self.is_word("partition") || (self.oracle() && self.is_word("subpartition"));
        if !(selects_partitions && at_keyword) {
            return Ok(None);
        }
        let holding_key = self.oracle()
            && self.is_word_at(1, "for")
            && self.peek_kind_at(2) == &TokenKind::LeftParen;
        if !holding_key && self.peek_kind_at(1) != &TokenKind::LeftParen {
            return Ok(None);
        }
        let position = self.position();
        self.at += if holding_key { 2 } else { 1 };
        self.expect(&TokenKind::LeftParen)?;
        let selection = if holding_key {
            let mut values = vec![self.expression()?];
            while self.take(&TokenKind::Comma) {
                values.push(self.expression()?);
            }
            PartitionSelection::HoldingKey { values, position }
        } else {
            let mut names = vec![self.name()?];
            while self.mysql() && self.take(&TokenKind::Comma) {
                names.push(self.name()?);
            }
            PartitionSelection::Named(names)
        };
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(Box::new(selection)))
    }
}

fn qualified_column_starts(expr: &crate::ast::Expr, found: &mut Vec<usize>) {
    if let ExprKind::Column {
        qualifier: Some(_), ..
    } = &expr.kind
    {
        found.push(expr.position);
    }
    for child in crate::eval::children(expr) {
        qualified_column_starts(child, found);
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
