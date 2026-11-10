use crate::error::{DocumentError, DocumentResult};
use crate::store::check_collection_name;
use crate::update::{Update, parse_update_statement};
use eggdb_bson::{Bson, Document};

/// Where an aggregation's final `$out` or `$merge` stage writes its results.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineOutput {
    /// `$out`: the results replace the collection's documents.
    Out {
        /// The target database, when not the aggregate's own.
        database: Option<String>,
        /// The target collection.
        collection: String,
    },
    /// `$merge`: each result is merged into the collection by `_id`.
    Merge {
        /// The target database, when not the aggregate's own.
        database: Option<String>,
        /// The target collection.
        collection: String,
        /// What happens to a result whose `_id` is already there.
        when_matched: WhenMatched,
        /// What happens to a result whose `_id` is not.
        when_not_matched: WhenNotMatched,
    },
}

/// `$merge`'s `whenMatched`.
#[derive(Debug, Clone, PartialEq)]
pub enum WhenMatched {
    /// The result replaces the document.
    Replace,
    /// The document stays as it is.
    KeepExisting,
    /// The result's top-level fields are set on the document.
    Merge,
    /// The aggregation fails with a duplicate key error.
    Fail,
    /// An update pipeline runs over the document, with `let` variables (by default `new`, the
    /// result) bound to values of the result.
    Pipeline {
        /// The update pipeline as written.
        stages: Vec<Bson>,
        /// The variables and their expressions over the result.
        variables: Vec<(String, Bson)>,
    },
}

/// `$merge`'s `whenNotMatched`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenNotMatched {
    /// The result is inserted.
    Insert,
    /// The result is dropped.
    Discard,
    /// The aggregation fails.
    Fail,
}

impl WhenMatched {
    /// The update to apply to a matched document for `result`.
    pub fn update_for(&self, result: &Document) -> DocumentResult<Option<Update>> {
        Ok(match self {
            WhenMatched::Replace => {
                let mut replacement = result.clone();
                replacement.remove("_id");
                Some(Update::Replacement(replacement))
            }
            WhenMatched::KeepExisting | WhenMatched::Fail => None,
            WhenMatched::Merge => {
                let changes: Vec<(String, Bson)> = result
                    .iter()
                    .filter(|(name, _)| name.as_str() != "_id")
                    .map(|(name, value)| {
                        (
                            name.clone(),
                            Bson::Document(Document::from_entries(vec![(
                                "$literal".to_owned(),
                                value.clone(),
                            )])),
                        )
                    })
                    .collect();
                if changes.is_empty() {
                    return Ok(None);
                }
                let stage = Bson::Document(Document::from_entries(vec![(
                    "$set".to_owned(),
                    Bson::Document(Document::from_entries(changes)),
                )]));
                Some(parse_update_statement(&Bson::Array(vec![stage]), None)?)
            }
            WhenMatched::Pipeline { stages, variables } => {
                let mut bound = Vec::with_capacity(variables.len());
                for (name, expression) in variables {
                    bound.push((
                        name.clone(),
                        crate::expression::evaluate(expression, result)?,
                    ));
                }
                let stages = crate::pipeline::pipeline_with_variables(stages, &bound);
                Some(parse_update_statement(&Bson::Array(stages), None)?)
            }
        })
    }
}

fn location(code: i32, code_name: &'static str, message: impl Into<String>) -> DocumentError {
    DocumentError::new(code, code_name, message)
}

/// Splits a final `$out` or `$merge` stage off `stages`, checking that neither appears
/// anywhere else, and reads it.
pub fn split_output(stages: &[Bson]) -> DocumentResult<(&[Bson], Option<PipelineOutput>)> {
    let name_of = |stage: &Bson| -> Option<String> {
        stage
            .as_document()
            .and_then(Document::first)
            .map(|(name, _)| name.clone())
            .filter(|name| name == "$out" || name == "$merge")
    };
    for stage in stages.iter().take(stages.len().saturating_sub(1)) {
        if let Some(name) = name_of(stage) {
            return Err(location(
                40601,
                "Location40601",
                format!("{name} can only be the final stage in the pipeline"),
            ));
        }
    }
    let Some((last, rest)) = stages.split_last() else {
        return Ok((stages, None));
    };
    let Bson::Document(stage) = last else {
        return Ok((stages, None));
    };
    let Some(name) = name_of(last) else {
        return Ok((stages, None));
    };
    if stage.len() != 1 {
        return Err(DocumentError::failed_to_parse(
            "A pipeline stage specification object must contain exactly one field.",
        ));
    }
    let argument = stage.get(&name).cloned().unwrap_or(Bson::Null);
    let output = if name == "$out" {
        out_of(&argument)?
    } else {
        merge_of(&argument)?
    };
    Ok((rest, Some(output)))
}

fn target(
    argument: &Bson,
    stage: &str,
    collection_field: &str,
) -> DocumentResult<(Option<String>, String)> {
    let (database, collection) = match argument {
        Bson::String(collection) => (None, collection.clone()),
        Bson::Document(spec) => {
            let database = match spec.get("db") {
                None => None,
                Some(Bson::String(database)) => Some(database.clone()),
                Some(_) => {
                    return Err(DocumentError::type_mismatch(format!(
                        "{stage} 'db' must be a string"
                    )));
                }
            };
            let Some(Bson::String(collection)) = spec.get(collection_field) else {
                return Err(DocumentError::failed_to_parse(format!(
                    "{stage} needs the string field '{collection_field}'"
                )));
            };
            (database, collection.clone())
        }
        _ => {
            return Err(DocumentError::type_mismatch(format!(
                "{stage} only supports a string or object argument"
            )));
        }
    };
    check_collection_name(&collection)?;
    if collection.starts_with("system.") {
        return Err(DocumentError::invalid_namespace(format!(
            "{stage} cannot write to the system collection {collection}"
        )));
    }
    Ok((database, collection))
}

fn out_of(argument: &Bson) -> DocumentResult<PipelineOutput> {
    let (database, collection) = target(argument, "$out", "coll")?;
    Ok(PipelineOutput::Out {
        database,
        collection,
    })
}

fn merge_of(argument: &Bson) -> DocumentResult<PipelineOutput> {
    let spec = match argument {
        Bson::String(_) => Document::from_entries(vec![("into".to_owned(), argument.clone())]),
        Bson::Document(spec) => spec.clone(),
        _ => {
            return Err(DocumentError::type_mismatch(
                "$merge requires a string or object argument",
            ));
        }
    };
    let Some(into) = spec.get("into") else {
        return Err(DocumentError::failed_to_parse(
            "BSON field '$merge.into' is missing but a required field",
        ));
    };
    let (database, collection) = target(into, "$merge", "coll")?;
    let only_id = match spec.get("on") {
        None => true,
        Some(Bson::String(field)) => field == "_id",
        Some(Bson::Array(fields)) => {
            fields.len() == 1 && matches!(&fields[0], Bson::String(field) if field == "_id")
        }
        Some(_) => {
            return Err(location(
                51186,
                "Location51186",
                "$merge 'on' field must be either a string or an array of strings",
            ));
        }
    };
    if !only_id {
        return Err(location(
            51183,
            "Location51183",
            "Cannot find index to verify that join fields will be unique",
        ));
    }
    let variables = match spec.get("let") {
        None => vec![("new".to_owned(), Bson::String("$$ROOT".to_owned()))],
        Some(Bson::Document(names)) => names
            .iter()
            .map(|(name, expression)| (name.clone(), expression.clone()))
            .collect(),
        Some(_) => {
            return Err(DocumentError::type_mismatch(
                "$merge 'let' must be an object",
            ));
        }
    };
    let when_matched = match spec.get("whenMatched") {
        None => WhenMatched::Merge,
        Some(Bson::String(mode)) => match mode.as_str() {
            "replace" => WhenMatched::Replace,
            "keepExisting" => WhenMatched::KeepExisting,
            "merge" => WhenMatched::Merge,
            "fail" => WhenMatched::Fail,
            other => {
                return Err(DocumentError::bad_value(format!(
                    "Enumeration value '{other}' for field '$merge.whenMatched' is not a valid value."
                )));
            }
        },
        Some(Bson::Array(stages)) => {
            parse_update_statement(&Bson::Array(stages.clone()), None)?;
            WhenMatched::Pipeline {
                stages: stages.clone(),
                variables: variables.clone(),
            }
        }
        Some(_) => {
            return Err(DocumentError::type_mismatch(
                "$merge 'whenMatched' must be a string or an array",
            ));
        }
    };
    if spec.contains_key("let") && !matches!(when_matched, WhenMatched::Pipeline { .. }) {
        return Err(location(
            51199,
            "Location51199",
            "Cannot use 'let' variables with 'whenMatched: replace|keepExisting|merge|fail' mode",
        ));
    }
    let when_not_matched = match spec.get("whenNotMatched") {
        None => WhenNotMatched::Insert,
        Some(Bson::String(mode)) => match mode.as_str() {
            "insert" => WhenNotMatched::Insert,
            "discard" => WhenNotMatched::Discard,
            "fail" => WhenNotMatched::Fail,
            other => {
                return Err(DocumentError::bad_value(format!(
                    "Enumeration value '{other}' for field '$merge.whenNotMatched' is not a valid value."
                )));
            }
        },
        Some(_) => {
            return Err(DocumentError::type_mismatch(
                "$merge 'whenNotMatched' must be a string",
            ));
        }
    };
    Ok(PipelineOutput::Merge {
        database,
        collection,
        when_matched,
        when_not_matched,
    })
}

pub(crate) fn not_matched_error() -> DocumentError {
    location(
        13113,
        "Location13113",
        "$merge could not find a matching document in the target collection for at least one document in the source collection",
    )
}
