use crate::{
    HEADER_BYTES, INDEX_INTERIOR, INDEX_LEAF, MAGIC, Result, SqliteFormatError, SqliteRow,
    SqliteValue, TABLE_INTERIOR, TABLE_LEAF,
};
use std::cmp::Ordering;

const PAGE_BYTES: usize = 4096;
const WRITTEN_BY_VERSION: u32 = 3_045_000;
const LEAF_HEADER_BYTES: usize = 8;
const INTERIOR_HEADER_BYTES: usize = 12;
const CELL_POINTER_BYTES: usize = 2;
const CHILD_POINTER_BYTES: usize = 4;
const MOST_PAGES: usize = 1 << 28;

/// How one key column of an index sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyOrder {
    /// Whether the column sorts from the largest value to the smallest (`DESC`).
    pub descending: bool,
}

/// One object of a new SQLite file, written in the order given, as `sqlite_schema` lists it.
#[derive(Debug, Clone, PartialEq)]
pub enum SqliteObject {
    /// A rowid table and its rows.
    Table {
        /// The table's name.
        name: String,
        /// Its `CREATE TABLE` statement.
        sql: String,
        /// Its rows, in any order; their rowids must differ.
        rows: Vec<SqliteRow>,
    },
    /// An index of a rowid table and its entries.
    Index {
        /// The index's name.
        name: String,
        /// The table it indexes.
        table: String,
        /// Its `CREATE INDEX` statement; `None` for an index a constraint made.
        sql: Option<String>,
        /// One entry per row: the key columns' values followed by the row's rowid.
        entries: Vec<Vec<SqliteValue>>,
        /// How each key column sorts, in key order; a missing one sorts ascending.
        orders: Vec<KeyOrder>,
    },
    /// A view.
    View {
        /// The view's name.
        name: String,
        /// Its `CREATE VIEW` statement.
        sql: String,
    },
    /// A trigger.
    Trigger {
        /// The trigger's name.
        name: String,
        /// The table or view it fires on.
        table: String,
        /// Its `CREATE TRIGGER` statement.
        sql: String,
    },
}

/// The bytes of a new SQLite database file, version 3, holding `objects`: 4,096-byte pages,
/// UTF-8 text, rollback journal mode, and no free pages. Index entries are sorted with SQLite's
/// `BINARY` collation; the caller leaves out indexes that need another one.
pub fn write_database(objects: &[SqliteObject]) -> Result<Vec<u8>> {
    let mut pages = Pages::new();
    let mut schema = Vec::with_capacity(objects.len());
    for object in objects {
        schema.push(match object {
            SqliteObject::Table { name, sql, rows } => {
                let root = table_tree(&mut pages, rows.clone(), None)?;
                schema_row("table", name, name, root, Some(sql))
            }
            SqliteObject::Index {
                name,
                table,
                sql,
                entries,
                orders,
            } => {
                let root = index_tree(&mut pages, entries, orders)?;
                schema_row("index", name, table, root, sql.as_ref())
            }
            SqliteObject::View { name, sql } => schema_row("view", name, name, 0, Some(sql)),
            SqliteObject::Trigger { name, table, sql } => {
                schema_row("trigger", name, table, 0, Some(sql))
            }
        });
    }
    let rows = schema
        .into_iter()
        .zip(1i64..)
        .map(|(values, rowid)| SqliteRow { rowid, values })
        .collect();
    table_tree(&mut pages, rows, Some(1))?;
    pages.finish()
}

fn schema_row(
    kind: &str,
    name: &str,
    table: &str,
    root: u32,
    sql: Option<&String>,
) -> Vec<SqliteValue> {
    vec![
        SqliteValue::Text(kind.to_owned()),
        SqliteValue::Text(name.to_owned()),
        SqliteValue::Text(table.to_owned()),
        SqliteValue::Integer(i64::from(root)),
        sql.map_or(SqliteValue::Null, |text| SqliteValue::Text(text.clone())),
    ]
}

fn too_large() -> SqliteFormatError {
    SqliteFormatError::Unsupported("a database larger than 1 TiB".to_owned())
}

struct Pages {
    pages: Vec<Vec<u8>>,
}

impl Pages {
    fn new() -> Self {
        Self {
            pages: vec![vec![0; PAGE_BYTES]],
        }
    }

    fn allocate(&mut self) -> Result<u32> {
        if self.pages.len() >= MOST_PAGES {
            return Err(too_large());
        }
        self.pages.push(vec![0; PAGE_BYTES]);
        u32::try_from(self.pages.len()).map_err(|_| too_large())
    }

    fn set(&mut self, number: u32, bytes: Vec<u8>) -> Result<()> {
        let slot = usize::try_from(number)
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| self.pages.get_mut(index))
            .ok_or_else(too_large)?;
        *slot = bytes;
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>> {
        let count = u32::try_from(self.pages.len()).map_err(|_| too_large())?;
        let header = file_header(count)?;
        self.pages[0][..HEADER_BYTES].copy_from_slice(&header);
        Ok(self.pages.concat())
    }
}

fn file_header(page_count: u32) -> Result<[u8; HEADER_BYTES]> {
    let mut header = [0u8; HEADER_BYTES];
    header[..16].copy_from_slice(MAGIC);
    let page_bytes = u16::try_from(PAGE_BYTES).map_err(|_| too_large())?;
    header[16..18].copy_from_slice(&page_bytes.to_be_bytes());
    header[18] = 1;
    header[19] = 1;
    header[20] = 0;
    header[21] = 64;
    header[22] = 32;
    header[23] = 32;
    header[24..28].copy_from_slice(&1u32.to_be_bytes());
    header[28..32].copy_from_slice(&page_count.to_be_bytes());
    header[40..44].copy_from_slice(&1u32.to_be_bytes());
    header[44..48].copy_from_slice(&4u32.to_be_bytes());
    header[56..60].copy_from_slice(&1u32.to_be_bytes());
    header[92..96].copy_from_slice(&1u32.to_be_bytes());
    header[96..100].copy_from_slice(&WRITTEN_BY_VERSION.to_be_bytes());
    Ok(header)
}

pub(crate) fn put_varint(out: &mut Vec<u8>, value: u64) {
    if value > 0x00ff_ffff_ffff_ffff {
        let mut bytes = [0u8; 9];
        bytes[8] = value.to_be_bytes()[7];
        let mut rest = value >> 8;
        for slot in bytes[..8].iter_mut().rev() {
            *slot = rest.to_be_bytes()[7] & 0x7f | 0x80;
            rest >>= 7;
        }
        out.extend_from_slice(&bytes);
        return;
    }
    let mut groups = Vec::with_capacity(8);
    let mut rest = value;
    loop {
        groups.push(rest.to_be_bytes()[7] & 0x7f);
        rest >>= 7;
        if rest == 0 {
            break;
        }
    }
    let last = groups.len() - 1;
    for (position, group) in groups.iter().rev().enumerate() {
        out.push(if position < last { group | 0x80 } else { *group });
    }
}

fn varint_bytes(value: u64) -> usize {
    let mut out = Vec::with_capacity(9);
    put_varint(&mut out, value);
    out.len()
}

fn length(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn serial_of(value: &SqliteValue) -> (u64, Vec<u8>) {
    match value {
        SqliteValue::Null => (0, Vec::new()),
        SqliteValue::Integer(0) => (8, Vec::new()),
        SqliteValue::Integer(1) => (9, Vec::new()),
        SqliteValue::Integer(number) => {
            let (serial, width) = match *number {
                -0x80..=0x7f => (1, 1),
                -0x8000..=0x7fff => (2, 2),
                -0x80_0000..=0x7f_ffff => (3, 3),
                -0x8000_0000..=0x7fff_ffff => (4, 4),
                -0x8000_0000_0000..=0x7fff_ffff_ffff => (5, 6),
                _ => (6, 8),
            };
            (serial, number.to_be_bytes()[8 - width..].to_vec())
        }
        SqliteValue::Real(number) if number.is_nan() => (0, Vec::new()),
        SqliteValue::Real(number) => (7, number.to_be_bytes().to_vec()),
        SqliteValue::Text(text) => (13 + 2 * length(text.len()), text.as_bytes().to_vec()),
        SqliteValue::Blob(bytes) => (12 + 2 * length(bytes.len()), bytes.clone()),
    }
}

/// The record SQLite stores for `values`: its header of serial types, then the values' bodies.
pub fn encode_record(values: &[SqliteValue]) -> Vec<u8> {
    let serials: Vec<(u64, Vec<u8>)> = values.iter().map(serial_of).collect();
    let types_bytes: usize = serials.iter().map(|(serial, _)| varint_bytes(*serial)).sum();
    let mut header_bytes = types_bytes + 1;
    while types_bytes + varint_bytes(length(header_bytes)) != header_bytes {
        header_bytes = types_bytes + varint_bytes(length(header_bytes));
    }
    let body_bytes: usize = serials.iter().map(|(_, body)| body.len()).sum();
    let mut record = Vec::with_capacity(header_bytes + body_bytes);
    put_varint(&mut record, length(header_bytes));
    for (serial, _) in &serials {
        put_varint(&mut record, *serial);
    }
    for (_, body) in serials {
        record.extend_from_slice(&body);
    }
    record
}

fn table_max_local() -> usize {
    PAGE_BYTES - 35
}

fn index_max_local() -> usize {
    (PAGE_BYTES - 12) * 64 / 255 - 23
}

fn min_local() -> usize {
    (PAGE_BYTES - 12) * 32 / 255 - 23
}

fn local_bytes(payload: usize, max_local: usize) -> usize {
    if payload <= max_local {
        return payload;
    }
    let kept = min_local() + (payload - min_local()) % (PAGE_BYTES - 4);
    let local = if kept <= max_local { kept } else { min_local() };
    local + CHILD_POINTER_BYTES
}

fn spilled(pages: &mut Pages, payload: &[u8], max_local: usize) -> Result<Vec<u8>> {
    if payload.len() <= max_local {
        return Ok(payload.to_vec());
    }
    let kept = min_local() + (payload.len() - min_local()) % (PAGE_BYTES - 4);
    let local = if kept <= max_local { kept } else { min_local() };
    let mut out = payload[..local].to_vec();
    let chunks: Vec<&[u8]> = payload[local..].chunks(PAGE_BYTES - 4).collect();
    let numbers = chunks
        .iter()
        .map(|_| pages.allocate())
        .collect::<Result<Vec<u32>>>()?;
    for (position, chunk) in chunks.iter().enumerate() {
        let next = numbers.get(position + 1).copied().unwrap_or(0);
        let mut page = vec![0u8; PAGE_BYTES];
        page[..4].copy_from_slice(&next.to_be_bytes());
        page[4..4 + chunk.len()].copy_from_slice(chunk);
        pages.set(numbers[position], page)?;
    }
    out.extend_from_slice(&numbers[0].to_be_bytes());
    Ok(out)
}

struct Node {
    kind: u8,
    cells: Vec<Vec<u8>>,
    right: Option<u32>,
}

impl Node {
    fn image(&self, number: u32) -> Result<Vec<u8>> {
        let start = if number == 1 { HEADER_BYTES } else { 0 };
        let header_bytes = if self.right.is_some() {
            INTERIOR_HEADER_BYTES
        } else {
            LEAF_HEADER_BYTES
        };
        let mut page = vec![0u8; PAGE_BYTES];
        let mut content = PAGE_BYTES;
        let mut pointer = start + header_bytes;
        for cell in &self.cells {
            content = content
                .checked_sub(cell.len())
                .filter(|content| *content >= pointer + CELL_POINTER_BYTES)
                .ok_or_else(|| SqliteFormatError::Damaged("a page overflowed".to_owned()))?;
            page[content..content + cell.len()].copy_from_slice(cell);
            let offset = u16::try_from(content).map_err(|_| too_large())?;
            page[pointer..pointer + CELL_POINTER_BYTES].copy_from_slice(&offset.to_be_bytes());
            pointer += CELL_POINTER_BYTES;
        }
        page[start] = self.kind;
        let count = u16::try_from(self.cells.len()).map_err(|_| too_large())?;
        page[start + 3..start + 5].copy_from_slice(&count.to_be_bytes());
        let content_start = if content == 65_536 {
            0
        } else {
            u16::try_from(content).map_err(|_| too_large())?
        };
        page[start + 5..start + 7].copy_from_slice(&content_start.to_be_bytes());
        if let Some(right) = self.right {
            page[start + 8..start + 12].copy_from_slice(&right.to_be_bytes());
        }
        Ok(page)
    }
}

fn reserve_for(root: Option<u32>) -> usize {
    if root == Some(1) { HEADER_BYTES } else { 0 }
}

fn write_node(pages: &mut Pages, node: &Node) -> Result<u32> {
    let number = pages.allocate()?;
    pages.set(number, node.image(number)?)?;
    Ok(number)
}

fn write_root(pages: &mut Pages, node: &Node, root: Option<u32>) -> Result<u32> {
    match root {
        Some(number) => {
            pages.set(number, node.image(number)?)?;
            Ok(number)
        }
        None => write_node(pages, node),
    }
}

fn table_tree(pages: &mut Pages, mut rows: Vec<SqliteRow>, root: Option<u32>) -> Result<u32> {
    rows.sort_by_key(|row| row.rowid);
    if rows.windows(2).any(|pair| pair[0].rowid == pair[1].rowid) {
        return Err(SqliteFormatError::Damaged(
            "two rows of a table share a rowid".to_owned(),
        ));
    }
    let capacity = PAGE_BYTES - reserve_for(root);
    let mut level: Vec<(Node, i64)> = Vec::new();
    let mut cells: Vec<Vec<u8>> = Vec::new();
    let mut used = LEAF_HEADER_BYTES;
    let mut last_rowid = 0i64;
    for row in &rows {
        let payload = encode_record(&row.values);
        let size = varint_bytes(length(payload.len()))
            + varint_bytes(row.rowid as u64)
            + local_bytes(payload.len(), table_max_local())
            + CELL_POINTER_BYTES;
        if used + size > capacity && !cells.is_empty() {
            level.push((leaf(TABLE_LEAF, std::mem::take(&mut cells)), last_rowid));
            used = LEAF_HEADER_BYTES;
        }
        let mut cell = Vec::with_capacity(size);
        put_varint(&mut cell, length(payload.len()));
        put_varint(&mut cell, row.rowid as u64);
        cell.extend_from_slice(&spilled(pages, &payload, table_max_local())?);
        cells.push(cell);
        used += size;
        last_rowid = row.rowid;
    }
    level.push((leaf(TABLE_LEAF, cells), last_rowid));
    loop {
        if level.len() == 1 {
            return write_root(pages, &level[0].0, root);
        }
        let mut children = Vec::with_capacity(level.len());
        for (node, key) in &level {
            children.push((write_node(pages, node)?, *key));
        }
        level = table_interior_level(&children, capacity);
    }
}

fn leaf(kind: u8, cells: Vec<Vec<u8>>) -> Node {
    Node {
        kind,
        cells,
        right: None,
    }
}

fn table_interior_level(children: &[(u32, i64)], capacity: usize) -> Vec<(Node, i64)> {
    let cell_size = |key: i64| CHILD_POINTER_BYTES + varint_bytes(key as u64) + CELL_POINTER_BYTES;
    let mut groups: Vec<Vec<(u32, i64)>> = Vec::new();
    let mut group: Vec<(u32, i64)> = Vec::new();
    let mut used = INTERIOR_HEADER_BYTES;
    for child in children {
        if let Some(previous) = group.last() {
            let size = cell_size(previous.1);
            if used + size > capacity {
                groups.push(std::mem::take(&mut group));
                used = INTERIOR_HEADER_BYTES;
            } else {
                used += size;
            }
        }
        group.push(*child);
    }
    groups.push(group);
    if groups.len() > 1 && groups.last().is_some_and(|last| last.len() == 1) {
        let count = groups.len();
        if let Some(moved) = groups[count - 2].pop() {
            groups[count - 1].insert(0, moved);
        }
    }
    groups
        .into_iter()
        .filter_map(|group| {
            let (&(right, key), rest) = group.split_last()?;
            let cells = rest
                .iter()
                .map(|(page, key)| {
                    let mut cell = page.to_be_bytes().to_vec();
                    put_varint(&mut cell, *key as u64);
                    cell
                })
                .collect();
            Some((
                Node {
                    kind: TABLE_INTERIOR,
                    cells,
                    right: Some(right),
                },
                key,
            ))
        })
        .collect()
}

fn class_rank(value: &SqliteValue) -> u8 {
    match value {
        SqliteValue::Null => 0,
        SqliteValue::Integer(_) | SqliteValue::Real(_) => 1,
        SqliteValue::Text(_) => 2,
        SqliteValue::Blob(_) => 3,
    }
}

fn compare_numbers(left: &SqliteValue, right: &SqliteValue) -> Ordering {
    match (left, right) {
        (SqliteValue::Integer(left), SqliteValue::Integer(right)) => left.cmp(right),
        (SqliteValue::Integer(left), SqliteValue::Real(right)) => compare_integer_real(*left, *right),
        (SqliteValue::Real(left), SqliteValue::Integer(right)) => {
            compare_integer_real(*right, *left).reverse()
        }
        (SqliteValue::Real(left), SqliteValue::Real(right)) => {
            left.partial_cmp(right).unwrap_or(Ordering::Equal)
        }
        _ => Ordering::Equal,
    }
}

fn compare_integer_real(integer: i64, real: f64) -> Ordering {
    if real.is_nan() {
        return Ordering::Greater;
    }
    if real < -9.223_372_036_854_775_808e18 {
        return Ordering::Greater;
    }
    if real >= 9.223_372_036_854_775_808e18 {
        return Ordering::Less;
    }
    let truncated = real.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => {
            let fraction = real - real.trunc();
            if fraction > 0.0 {
                Ordering::Less
            } else if fraction < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    }
}

/// How SQLite's `BINARY` collation orders two values of a record.
pub fn compare_values(left: &SqliteValue, right: &SqliteValue) -> Ordering {
    match class_rank(left).cmp(&class_rank(right)) {
        Ordering::Equal => match (left, right) {
            (SqliteValue::Text(left), SqliteValue::Text(right)) => {
                left.as_bytes().cmp(right.as_bytes())
            }
            (SqliteValue::Blob(left), SqliteValue::Blob(right)) => left.cmp(right),
            (SqliteValue::Null, SqliteValue::Null) => Ordering::Equal,
            _ => compare_numbers(left, right),
        },
        other => other,
    }
}

fn compare_entries(left: &[SqliteValue], right: &[SqliteValue], orders: &[KeyOrder]) -> Ordering {
    for (position, (left, right)) in left.iter().zip(right).enumerate() {
        let order = compare_values(left, right);
        let order = if orders.get(position).is_some_and(|order| order.descending) {
            order.reverse()
        } else {
            order
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn index_tree(
    pages: &mut Pages,
    entries: &[Vec<SqliteValue>],
    orders: &[KeyOrder],
) -> Result<u32> {
    let mut sorted: Vec<&Vec<SqliteValue>> = entries.iter().collect();
    sorted.sort_by(|left, right| compare_entries(left, right, orders));
    let records: Vec<Vec<u8>> = sorted.iter().map(|entry| encode_record(entry)).collect();
    let leaf_size = |record: &Vec<u8>| {
        varint_bytes(length(record.len()))
            + local_bytes(record.len(), index_max_local())
            + CELL_POINTER_BYTES
    };
    let mut leaves: Vec<Vec<usize>> = Vec::new();
    let mut dividers: Vec<usize> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut used = LEAF_HEADER_BYTES;
    let mut position = 0;
    while position < records.len() {
        let size = leaf_size(&records[position]);
        if used + size <= PAGE_BYTES || current.is_empty() {
            current.push(position);
            used += size;
            position += 1;
            continue;
        }
        if position + 1 < records.len() {
            leaves.push(std::mem::take(&mut current));
            dividers.push(position);
            position += 1;
        } else if let Some(divider) = current.pop() {
            leaves.push(std::mem::take(&mut current));
            dividers.push(divider);
            current.push(position);
            position += 1;
        }
        used = LEAF_HEADER_BYTES + current.iter().map(|at| leaf_size(&records[*at])).sum::<usize>();
    }
    leaves.push(current);
    let mut level: Vec<Node> = Vec::with_capacity(leaves.len());
    for members in leaves {
        let mut cells = Vec::with_capacity(members.len());
        for at in members {
            let mut cell = Vec::new();
            put_varint(&mut cell, length(records[at].len()));
            cell.extend_from_slice(&spilled(pages, &records[at], index_max_local())?);
            cells.push(cell);
        }
        level.push(leaf(INDEX_LEAF, cells));
    }
    loop {
        if level.len() == 1 {
            return write_root(pages, &level[0], None);
        }
        let mut children = Vec::with_capacity(level.len());
        for node in &level {
            children.push(write_node(pages, node)?);
        }
        let (groups, promoted) = index_interior_groups(&children, &dividers, &records);
        level = Vec::with_capacity(groups.len());
        for (members, separators) in groups {
            let Some((&right, rest)) = members.split_last() else {
                continue;
            };
            let mut cells = Vec::with_capacity(rest.len());
            for (child, at) in rest.iter().zip(&separators) {
                let mut cell = child.to_be_bytes().to_vec();
                put_varint(&mut cell, length(records[*at].len()));
                cell.extend_from_slice(&spilled(pages, &records[*at], index_max_local())?);
                cells.push(cell);
            }
            level.push(Node {
                kind: INDEX_INTERIOR,
                cells,
                right: Some(right),
            });
        }
        dividers = promoted;
    }
}

type IndexGroup = (Vec<u32>, Vec<usize>);

fn index_interior_groups(
    children: &[u32],
    dividers: &[usize],
    records: &[Vec<u8>],
) -> (Vec<IndexGroup>, Vec<usize>) {
    let cell_size = |at: usize| {
        CHILD_POINTER_BYTES
            + varint_bytes(length(records[at].len()))
            + local_bytes(records[at].len(), index_max_local())
            + CELL_POINTER_BYTES
    };
    let mut groups: Vec<IndexGroup> = Vec::new();
    let mut promoted: Vec<usize> = Vec::new();
    let mut members: Vec<u32> = vec![children[0]];
    let mut separators: Vec<usize> = Vec::new();
    let mut used = INTERIOR_HEADER_BYTES;
    for (index, &divider) in dividers.iter().enumerate() {
        let next = children[index + 1];
        let size = cell_size(divider);
        if used + size <= PAGE_BYTES || separators.is_empty() {
            separators.push(divider);
            members.push(next);
            used += size;
            continue;
        }
        let last_child = index + 2 == children.len();
        if last_child && separators.len() >= 2 {
            let (Some(moved_child), Some(moved_separator)) = (members.pop(), separators.pop())
            else {
                continue;
            };
            groups.push((std::mem::take(&mut members), std::mem::take(&mut separators)));
            promoted.push(moved_separator);
            members = vec![moved_child, next];
            separators = vec![divider];
            used = INTERIOR_HEADER_BYTES + cell_size(divider);
        } else {
            groups.push((std::mem::take(&mut members), std::mem::take(&mut separators)));
            promoted.push(divider);
            members = vec![next];
            used = INTERIOR_HEADER_BYTES;
        }
    }
    groups.push((members, separators));
    (groups, promoted)
}
