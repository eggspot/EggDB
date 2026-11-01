use super::{PlsqlPackage, compilation_error, compile_package, name_in_use, object_missing};
use crate::ast::{AlterPackage, CreatePackage, DropPackage, Name, PackagePart, Statement};
use crate::dialect::Dialect;
use crate::encoding::{decode_package, encode_package, package_key, packages_prefix, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::lexer::tokenize_in;
use crate::parser::Parser;
use crate::routines::MAX_ROUTINE_TEXT;
use crate::schemas::{base_name, is_qualified, normalized, qualified};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredPackage {
    pub(crate) specification: String,
    pub(crate) body: Option<String>,
}

pub(crate) fn stored_package(
    reader: &dyn KeyValueRead,
    name: &str,
) -> SqlResult<Option<StoredPackage>> {
    match reader.get(&package_key(name))? {
        Some(bytes) => decode_package(name, &bytes).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn every_package(reader: &dyn KeyValueRead) -> SqlResult<Vec<(String, StoredPackage)>> {
    let prefix = packages_prefix();
    let mut found = Vec::new();
    let mut failure = None;
    let mut memory = crate::memory::WorkMemory::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            memory.charge(key.len() + value.len() + 64)?;
            let decoded = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| crate::encoding::corrupt("package definitions"))
                .and_then(|name| Ok((name.to_owned(), decode_package(name, value)?)));
            match decoded {
                Ok(package) => found.push(package),
                Err(error) => {
                    failure = Some(error);
                    return Ok(false);
                }
            }
            Ok(true)
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

fn save_package(
    writer: &mut dyn KeyValueWrite,
    name: &str,
    package: Option<&StoredPackage>,
) -> SqlResult<()> {
    let key = package_key(name);
    match package {
        Some(package) => writer.put(&key, &encode_package(package))?,
        None => {
            writer.delete(&key)?;
        }
    }
    Ok(())
}

pub(crate) fn resolve_package(
    reader: &dyn KeyValueRead,
    name: &str,
) -> SqlResult<Option<(String, StoredPackage)>> {
    let plain = normalized(name);
    if is_qualified(plain) || plain.len() != name.len() {
        return Ok(stored_package(reader, plain)?.map(|found| (plain.to_owned(), found)));
    }
    for schema in crate::schemas::relation_path(reader)? {
        let internal = qualified(&schema, name);
        if let Some(found) = stored_package(reader, &internal)? {
            return Ok(Some((internal, found)));
        }
    }
    Ok(None)
}

pub(crate) fn parse_package(text: &str) -> SqlResult<PlsqlPackage> {
    let tokens = tokenize_in(text, Dialect::Oracle)?;
    match Parser::new(text, &tokens)
        .in_dialect(Dialect::Oracle)
        .statement()?
    {
        Statement::CreatePackage(create) => Ok(create.package),
        _ => Err(crate::encoding::corrupt("package definitions")),
    }
}

fn body_without_specification(package: &PlsqlPackage) -> SqlError {
    let error = SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "PLS-00304: cannot compile body of '{}' without its specification",
            super::shown_object(&package.name.text)
        ),
    )
    .at(0);
    compilation_error(&package.text, error)
}

fn too_long(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a package's definition is at most {MAX_ROUTINE_TEXT} bytes"),
    )
    .at(position)
}

pub(crate) fn create_package(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreatePackage,
) -> SqlResult<QueryResult> {
    let written = &create.package.name;
    let position = written.position;
    crate::constraints::check_identifier(&Name {
        text: base_name(&written.text).to_owned(),
        position,
    })?;
    if create.text.len() > MAX_ROUTINE_TEXT {
        return Err(too_long(position));
    }
    let name = crate::schemas::creation_name(&*writer, written)?;
    let existing = stored_package(&*writer, &name)?;
    let (package, tag) = if create.package.body {
        let Some(known) = existing else {
            return Err(body_without_specification(&create.package));
        };
        if known.body.is_some() && !create.replace {
            return Err(name_in_use(position));
        }
        let specification = parse_package(&known.specification)?;
        compile_package(&create.package.text, Some(&specification.block))?;
        let package = StoredPackage {
            body: Some(create.text.clone()),
            ..known
        };
        (package, "CREATE PACKAGE BODY")
    } else {
        if tables.relation(&*writer, &name)?.is_some()
            || crate::routines::has_routines(&*writer, &name)?
        {
            return Err(name_in_use(position));
        }
        let body = match existing {
            Some(_) if !create.replace => return Err(name_in_use(position)),
            Some(known) => known.body,
            None => None,
        };
        let package = StoredPackage {
            specification: create.text.clone(),
            body,
        };
        (package, "CREATE PACKAGE")
    };
    save_package(writer, &name, Some(&package))?;
    super::with_session(|session| session.discard_package(&name));
    Ok(QueryResult::command(tag))
}

pub(crate) fn drop_package(
    writer: &mut dyn KeyValueWrite,
    drop: &DropPackage,
) -> SqlResult<QueryResult> {
    let tag = if drop.body {
        "DROP PACKAGE BODY"
    } else {
        "DROP PACKAGE"
    };
    let found = resolve_package(&*writer, &drop.name.text)?
        .filter(|(_, package)| !drop.body || package.body.is_some());
    let Some((name, package)) = found else {
        if drop.if_exists {
            return Ok(QueryResult::command(tag));
        }
        return Err(object_missing(&drop.name.text, drop.name.position));
    };
    if drop.body {
        let specification_only = StoredPackage {
            body: None,
            ..package
        };
        save_package(writer, &name, Some(&specification_only))?;
    } else {
        save_package(writer, &name, None)?;
    }
    super::with_session(|session| session.discard_package(&name));
    Ok(QueryResult::command(tag))
}

pub(crate) fn alter_package(
    reader: &dyn KeyValueRead,
    alter: &AlterPackage,
) -> SqlResult<QueryResult> {
    let missing = || object_missing(&alter.name.text, alter.name.position);
    let (_, package) = resolve_package(reader, &alter.name.text)?.ok_or_else(missing)?;
    let specification = parse_package(&package.specification)?;
    match (alter.part, &package.body) {
        (PackagePart::Body, None) => return Err(missing()),
        (PackagePart::Body | PackagePart::Whole, Some(body)) => {
            let body = parse_package(body)?;
            compile_package(&body.text, Some(&specification.block))?;
        }
        (PackagePart::Specification | PackagePart::Whole, _) => {}
    }
    Ok(QueryResult::command("ALTER PACKAGE"))
}
