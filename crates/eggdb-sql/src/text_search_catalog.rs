use crate::ast::Name;
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use eggdb_search::{Analyzer, Config, Dictionary, StopWords};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const TEXT_SEARCH_SPACE: u8 = 0x19;
const DICTIONARY_KIND: u8 = b'd';
const CONFIGURATION_KIND: u8 = b'c';
const RECORD_VERSION: u8 = 1;
const MAX_MAPPED_DICTIONARIES: usize = 64;
const SIMPLE_DICTIONARY: &str = "simple";
const STEM_SUFFIX: &str = "_stem";
const TEXT_SEARCH_FUNCTIONS: [&str; 9] = [
    "to_tsvector",
    "to_tsquery",
    "plainto_tsquery",
    "phraseto_tsquery",
    "websearch_to_tsquery",
    "ts_headline",
    "json_to_tsvector",
    "jsonb_to_tsvector",
    "regconfig",
];

/// Which kind of text search object a command names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextSearchKind {
    Dictionary,
    Configuration,
}

impl TextSearchKind {
    fn key_kind(self) -> u8 {
        match self {
            Self::Dictionary => DICTIONARY_KIND,
            Self::Configuration => CONFIGURATION_KIND,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Dictionary => "dictionary",
            Self::Configuration => "configuration",
        }
    }
}

/// Where `CREATE TEXT SEARCH CONFIGURATION` takes its mappings from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConfigurationSource {
    Copy(Name),
    Parser(Name),
}

/// A change to a configuration's mappings.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MappingChange {
    Add {
        token_types: Vec<Name>,
        dictionaries: Vec<Name>,
    },
    Alter {
        token_types: Vec<Name>,
        dictionaries: Vec<Name>,
    },
    Replace {
        token_types: Option<Vec<Name>>,
        old: Name,
        new: Name,
    },
    Drop {
        token_types: Vec<Name>,
        if_exists: bool,
    },
}

/// `CREATE`, `ALTER` and `DROP TEXT SEARCH DICTIONARY` and `CONFIGURATION`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TextSearchCommand {
    CreateDictionary {
        name: Name,
        options: Vec<(Name, Option<String>)>,
    },
    AlterDictionary {
        name: Name,
        options: Vec<(Name, Option<String>)>,
    },
    CreateConfiguration {
        name: Name,
        source: ConfigurationSource,
    },
    AlterMapping {
        name: Name,
        change: MappingChange,
    },
    Rename {
        kind: TextSearchKind,
        name: Name,
        to: Name,
    },
    Owner {
        kind: TextSearchKind,
        name: Name,
    },
    Drop {
        kind: TextSearchKind,
        names: Vec<Name>,
        if_exists: bool,
    },
}

impl TextSearchCommand {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::CreateDictionary { .. } => "CREATE TEXT SEARCH DICTIONARY",
            Self::CreateConfiguration { .. } => "CREATE TEXT SEARCH CONFIGURATION",
            Self::AlterDictionary { .. }
            | Self::Rename {
                kind: TextSearchKind::Dictionary,
                ..
            }
            | Self::Owner {
                kind: TextSearchKind::Dictionary,
                ..
            } => "ALTER TEXT SEARCH DICTIONARY",
            Self::AlterMapping { .. } | Self::Rename { .. } | Self::Owner { .. } => {
                "ALTER TEXT SEARCH CONFIGURATION"
            }
            Self::Drop {
                kind: TextSearchKind::Dictionary,
                ..
            } => "DROP TEXT SEARCH DICTIONARY",
            Self::Drop { .. } => "DROP TEXT SEARCH CONFIGURATION",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Template {
    Simple,
    Snowball,
    Synonym,
}

impl Template {
    fn named(name: &str) -> Option<Self> {
        match name.trim_start_matches("pg_catalog.") {
            "simple" => Some(Self::Simple),
            "snowball" => Some(Self::Snowball),
            "synonym" => Some(Self::Synonym),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Snowball => "snowball",
            Self::Synonym => "synonym",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredDictionary {
    template: Template,
    options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct StoredConfiguration {
    segments: bool,
    mappings: BTreeMap<String, Vec<String>>,
}

/// The text search dictionaries and configurations a database defines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Catalog {
    dictionaries: BTreeMap<String, StoredDictionary>,
    configurations: BTreeMap<String, StoredConfiguration>,
}

fn base(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn object_key(kind: TextSearchKind, name: &str) -> Vec<u8> {
    let mut key = space_prefix();
    key.push(kind.key_kind());
    key.extend_from_slice(base(name).as_bytes());
    key
}

fn space_prefix() -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(TEXT_SEARCH_SPACE);
    key
}

fn push_text(out: &mut Vec<u8>, text: &str) {
    let length = u32::try_from(text.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(text.as_bytes().get(..length as usize).unwrap_or_default());
}

struct RecordReader<'b> {
    bytes: &'b [u8],
}

impl<'b> RecordReader<'b> {
    fn text(&mut self) -> SqlResult<&'b str> {
        let corrupt = || crate::encoding::corrupt("text search object");
        let (length, rest) = self.bytes.split_first_chunk::<4>().ok_or_else(corrupt)?;
        let length = u32::from_le_bytes(*length) as usize;
        let (text, rest) = rest.split_at_checked(length).ok_or_else(corrupt)?;
        self.bytes = rest;
        std::str::from_utf8(text).map_err(|_| corrupt())
    }

    fn count(&mut self) -> SqlResult<usize> {
        self.text()?
            .parse()
            .map_err(|_| crate::encoding::corrupt("text search object"))
    }
}

fn encode_dictionary(dictionary: &StoredDictionary) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    push_text(&mut out, dictionary.template.name());
    push_text(&mut out, &dictionary.options.len().to_string());
    for (option, value) in &dictionary.options {
        push_text(&mut out, option);
        push_text(&mut out, value);
    }
    out
}

fn decode_dictionary(bytes: &[u8]) -> SqlResult<StoredDictionary> {
    let corrupt = || crate::encoding::corrupt("text search dictionary");
    let Some((&RECORD_VERSION, rest)) = bytes.split_first() else {
        return Err(corrupt());
    };
    let mut reader = RecordReader { bytes: rest };
    let template = Template::named(reader.text()?).ok_or_else(corrupt)?;
    let mut options = BTreeMap::new();
    for _ in 0..reader.count()? {
        let option = reader.text()?.to_owned();
        options.insert(option, reader.text()?.to_owned());
    }
    Ok(StoredDictionary { template, options })
}

fn encode_configuration(configuration: &StoredConfiguration) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    push_text(&mut out, if configuration.segments { "1" } else { "0" });
    push_text(&mut out, &configuration.mappings.len().to_string());
    for (token_type, dictionaries) in &configuration.mappings {
        push_text(&mut out, token_type);
        push_text(&mut out, &dictionaries.len().to_string());
        for dictionary in dictionaries {
            push_text(&mut out, dictionary);
        }
    }
    out
}

fn decode_configuration(bytes: &[u8]) -> SqlResult<StoredConfiguration> {
    let corrupt = || crate::encoding::corrupt("text search configuration");
    let Some((&RECORD_VERSION, rest)) = bytes.split_first() else {
        return Err(corrupt());
    };
    let mut reader = RecordReader { bytes: rest };
    let segments = reader.text()? == "1";
    let mut mappings = BTreeMap::new();
    for _ in 0..reader.count()? {
        let token_type = reader.text()?.to_owned();
        let count = reader.count()?;
        if count > MAX_MAPPED_DICTIONARIES {
            return Err(corrupt());
        }
        let mut dictionaries = Vec::with_capacity(count);
        for _ in 0..count {
            dictionaries.push(reader.text()?.to_owned());
        }
        mappings.insert(token_type, dictionaries);
    }
    Ok(StoredConfiguration { segments, mappings })
}

fn built_in_language(dictionary: &str) -> Option<Config> {
    let language = dictionary.trim_start_matches("pg_catalog.").strip_suffix(STEM_SUFFIX)?;
    Config::named(language).filter(|config| !matches!(config, Config::Simple | Config::Cjk | Config::Thai))
}

fn is_built_in_dictionary(name: &str) -> bool {
    base(name) == SIMPLE_DICTIONARY || built_in_language(name).is_some()
}

fn stop_words(value: Option<&String>) -> SqlResult<StopWords> {
    let Some(value) = value else {
        return Ok(StopWords::None);
    };
    Config::named(value)
        .filter(|config| *config != Config::Simple)
        .map(StopWords::Of)
        .ok_or_else(|| missing_file(value, "stop"))
}

fn missing_file(name: &str, extension: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "could not open {extension} file \"{name}.{extension}\": EggDB keeps no tsearch_data files; list the words with the {} option",
            if extension == "stop" { "StopWordList" } else { "SynonymList" }
        ),
    )
}

fn boolean_option(value: Option<&String>, default: bool) -> bool {
    value.map_or(default, |value| {
        matches!(value.to_ascii_lowercase().as_str(), "true" | "on" | "yes" | "1")
    })
}

impl StoredDictionary {
    fn dictionary(&self) -> SqlResult<Dictionary> {
        let option = |name: &str| self.options.get(name);
        let stop_words = match option("stopwordlist") {
            Some(listed) => StopWords::Listed(eggdb_search::word_list(listed)),
            None => stop_words(option("stopwords"))?,
        };
        Ok(match self.template {
            Template::Simple => Dictionary::Simple {
                stop_words,
                accept: boolean_option(option("accept"), true),
            },
            Template::Snowball => {
                let language = option("language").ok_or_else(|| {
                    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "missing Language parameter")
                })?;
                let language = Config::named(language)
                    .filter(|config| built_in_language(&format!("{}{STEM_SUFFIX}", config.name())).is_some())
                    .ok_or_else(|| {
                        SqlError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("unrecognized language: \"{language}\""),
                        )
                    })?;
                Dictionary::Snowball { language, stop_words }
            }
            Template::Synonym => {
                let case_sensitive = boolean_option(option("casesensitive"), false);
                let synonyms = match (option("synonymlist"), option("synonyms")) {
                    (Some(listed), _) => eggdb_search::synonym_list(listed, case_sensitive),
                    (None, Some(file)) if base(file) == "synonym_sample" => eggdb_search::synonym_list(
                        "postgres pgsql\npostgresql pgsql\npostgre pgsql\ngogle googl\nindices index*",
                        case_sensitive,
                    ),
                    (None, Some(file)) => return Err(missing_file(file, "syn")),
                    (None, None) => {
                        return Err(SqlError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            "missing Synonyms parameter",
                        ));
                    }
                };
                Dictionary::Synonym { synonyms, case_sensitive }
            }
        })
    }
}

impl Catalog {
    pub(crate) fn load(reader: &dyn KeyValueRead) -> SqlResult<Self> {
        let prefix = space_prefix();
        let mut catalog = Self::default();
        let mut failure = None;
        reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |key, value| {
            let Some((&kind, name)) = key.get(prefix.len()..).and_then(<[u8]>::split_first) else {
                return Ok(true);
            };
            let Ok(name) = std::str::from_utf8(name) else {
                failure = Some(crate::encoding::corrupt("text search object"));
                return Ok(false);
            };
            match kind {
                DICTIONARY_KIND => match decode_dictionary(value) {
                    Ok(dictionary) => {
                        catalog.dictionaries.insert(name.to_owned(), dictionary);
                    }
                    Err(error) => failure = Some(error),
                },
                CONFIGURATION_KIND => match decode_configuration(value) {
                    Ok(configuration) => {
                        catalog.configurations.insert(name.to_owned(), configuration);
                    }
                    Err(error) => failure = Some(error),
                },
                _ => {}
            }
            Ok(failure.is_none())
        })?;
        match failure {
            Some(error) => Err(error),
            None => Ok(catalog),
        }
    }

    fn dictionary(&self, name: &str) -> SqlResult<Dictionary> {
        if let Some(stored) = self.dictionaries.get(base(name)) {
            return stored.dictionary();
        }
        if base(name) == SIMPLE_DICTIONARY {
            return Ok(Dictionary::Simple {
                stop_words: StopWords::None,
                accept: true,
            });
        }
        let language = built_in_language(name).ok_or_else(|| missing(TextSearchKind::Dictionary, name))?;
        Ok(Dictionary::Snowball {
            language,
            stop_words: StopWords::Of(language),
        })
    }

    /// The analyzer of the configuration named `name`, or `None` when the database defines
    /// none by that name.
    pub(crate) fn analyzer(&self, name: &str) -> Option<SqlResult<Analyzer>> {
        let configuration = self.configurations.get(base(name.trim()))?;
        Some(self.built(configuration))
    }

    fn built(&self, configuration: &StoredConfiguration) -> SqlResult<Analyzer> {
        let mut analyzer = Analyzer::new(configuration.segments);
        for (token_type, names) in &configuration.mappings {
            let dictionaries = names
                .iter()
                .map(|name| self.dictionary(name))
                .collect::<SqlResult<Vec<_>>>()?;
            analyzer.map(token_type, dictionaries);
        }
        Ok(analyzer)
    }

    fn knows_dictionary(&self, name: &str) -> bool {
        self.dictionaries.contains_key(base(name)) || is_built_in_dictionary(name)
    }
}

fn missing(kind: TextSearchKind, name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("text search {} \"{name}\" does not exist", kind.noun()),
    )
}

fn built_in_configuration(name: &str) -> Option<StoredConfiguration> {
    let config = Config::named(name)?;
    let word_dictionary = match config {
        Config::Simple | Config::Cjk | Config::Thai => SIMPLE_DICTIONARY.to_owned(),
        language => format!("{}{STEM_SUFFIX}", language.name()),
    };
    let mappings = eggdb_search::TOKEN_TYPES
        .iter()
        .filter(|(token_type, _)| *token_type != "blank")
        .map(|(token_type, _)| {
            let dictionary = if eggdb_search::Analyzer::word_types().contains(token_type)
                && !token_type.starts_with("num")
                && !token_type.ends_with("numpart")
            {
                word_dictionary.clone()
            } else {
                SIMPLE_DICTIONARY.to_owned()
            };
            ((*token_type).to_owned(), vec![dictionary])
        })
        .collect();
    Some(StoredConfiguration {
        segments: config.segments(),
        mappings,
    })
}

fn option_map(options: &[(Name, Option<String>)]) -> BTreeMap<String, String> {
    options
        .iter()
        .filter_map(|(name, value)| Some((name.text.to_ascii_lowercase(), value.clone()?)))
        .collect()
}

fn create_dictionary(writer: &mut dyn KeyValueWrite, name: &Name, options: &[(Name, Option<String>)]) -> SqlResult<()> {
    let key = object_key(TextSearchKind::Dictionary, &name.text);
    if writer.get(&key)?.is_some() || is_built_in_dictionary(&name.text) {
        return Err(SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!("text search dictionary \"{}\" already exists", base(&name.text)),
        )
        .at(name.position));
    }
    let mut options = option_map(options);
    let template_name = options.remove("template").ok_or_else(|| {
        SqlError::new(SqlState::INVALID_OBJECT_DEFINITION, "text search template is required").at(name.position)
    })?;
    let template = Template::named(&template_name).ok_or_else(|| {
        SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("text search template \"{template_name}\" is not supported yet: EggDB has the simple, snowball and synonym templates"),
        )
        .at(name.position)
    })?;
    let stored = StoredDictionary { template, options };
    stored.dictionary().map_err(|error| error.at(name.position))?;
    writer.put(&key, &encode_dictionary(&stored))
}

fn alter_dictionary(writer: &mut dyn KeyValueWrite, name: &Name, options: &[(Name, Option<String>)]) -> SqlResult<()> {
    let key = object_key(TextSearchKind::Dictionary, &name.text);
    let Some(bytes) = writer.get(&key)? else {
        return Err(missing(TextSearchKind::Dictionary, &name.text).at(name.position));
    };
    let mut stored = decode_dictionary(&bytes)?;
    for (option, value) in options {
        let option = option.text.to_ascii_lowercase();
        if option == "template" {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                "the template of a text search dictionary cannot be changed",
            )
            .at(name.position));
        }
        match value {
            Some(value) => stored.options.insert(option, value.clone()),
            None => stored.options.remove(&option),
        };
    }
    stored.dictionary().map_err(|error| error.at(name.position))?;
    writer.put(&key, &encode_dictionary(&stored))
}

fn stored_configuration(writer: &dyn KeyValueRead, name: &Name) -> SqlResult<StoredConfiguration> {
    match writer.get(&object_key(TextSearchKind::Configuration, &name.text))? {
        Some(bytes) => decode_configuration(&bytes),
        None => Err(missing(TextSearchKind::Configuration, &name.text).at(name.position)),
    }
}

fn create_configuration(writer: &mut dyn KeyValueWrite, name: &Name, source: &ConfigurationSource) -> SqlResult<()> {
    let key = object_key(TextSearchKind::Configuration, &name.text);
    if writer.get(&key)?.is_some() || Config::named(base(&name.text)).is_some() {
        return Err(SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!("text search configuration \"{}\" already exists", base(&name.text)),
        )
        .at(name.position));
    }
    let configuration = match source {
        ConfigurationSource::Parser(parser) if matches!(base(&parser.text), "default") => StoredConfiguration::default(),
        ConfigurationSource::Parser(parser) => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("text search parser \"{}\" does not exist", parser.text),
            )
            .at(parser.position));
        }
        ConfigurationSource::Copy(from) => match built_in_configuration(base(&from.text)) {
            Some(configuration) => configuration,
            None => stored_configuration(&*writer, from)?,
        },
    };
    writer.put(&key, &encode_configuration(&configuration))
}

fn checked_token_types(token_types: &[Name]) -> SqlResult<Vec<String>> {
    token_types
        .iter()
        .map(|token_type| {
            let lower = token_type.text.to_ascii_lowercase();
            if eggdb_search::is_token_type(&lower) {
                Ok(lower)
            } else {
                Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("token type \"{}\" does not exist", token_type.text),
                )
                .at(token_type.position))
            }
        })
        .collect()
}

fn checked_dictionaries(catalog: &Catalog, dictionaries: &[Name]) -> SqlResult<Vec<String>> {
    if dictionaries.len() > MAX_MAPPED_DICTIONARIES {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("a mapping lists at most {MAX_MAPPED_DICTIONARIES} dictionaries"),
        ));
    }
    dictionaries
        .iter()
        .map(|dictionary| {
            if catalog.knows_dictionary(&dictionary.text) {
                Ok(base(&dictionary.text).to_owned())
            } else {
                Err(missing(TextSearchKind::Dictionary, &dictionary.text).at(dictionary.position))
            }
        })
        .collect()
}

fn alter_mapping(writer: &mut dyn KeyValueWrite, name: &Name, change: &MappingChange) -> SqlResult<()> {
    let catalog = Catalog::load(&*writer)?;
    let mut configuration = stored_configuration(&*writer, name)?;
    match change {
        MappingChange::Add { token_types, dictionaries } | MappingChange::Alter { token_types, dictionaries } => {
            let adding = matches!(change, MappingChange::Add { .. });
            let dictionaries = checked_dictionaries(&catalog, dictionaries)?;
            for (token_type, written) in checked_token_types(token_types)?.into_iter().zip(token_types) {
                if !adding && !configuration.mappings.contains_key(&token_type) {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "mapping for token type \"{}\" does not exist",
                            written.text
                        ),
                    )
                    .at(written.position));
                }
                configuration.mappings.insert(token_type, dictionaries.clone());
            }
        }
        MappingChange::Replace { token_types, old, new } => {
            let new = checked_dictionaries(&catalog, std::slice::from_ref(new))?
                .into_iter()
                .next()
                .unwrap_or_default();
            let only = token_types.as_deref().map(checked_token_types).transpose()?;
            for (token_type, dictionaries) in &mut configuration.mappings {
                if only.as_ref().is_some_and(|only| !only.contains(token_type)) {
                    continue;
                }
                for dictionary in dictionaries.iter_mut() {
                    if dictionary == base(&old.text) {
                        dictionary.clone_from(&new);
                    }
                }
            }
        }
        MappingChange::Drop { token_types, if_exists } => {
            for (token_type, written) in checked_token_types(token_types)?.into_iter().zip(token_types) {
                if configuration.mappings.remove(&token_type).is_none() && !*if_exists {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "mapping for token type \"{}\" does not exist",
                            written.text
                        ),
                    )
                    .at(written.position));
                }
            }
        }
    }
    writer.put(
        &object_key(TextSearchKind::Configuration, &name.text),
        &encode_configuration(&configuration),
    )
}

fn rename(writer: &mut dyn KeyValueWrite, kind: TextSearchKind, name: &Name, to: &Name) -> SqlResult<()> {
    let from_key = object_key(kind, &name.text);
    let to_key = object_key(kind, &to.text);
    let Some(bytes) = writer.get(&from_key)? else {
        return Err(missing(kind, &name.text).at(name.position));
    };
    if writer.get(&to_key)?.is_some() {
        return Err(SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!("text search {} \"{}\" already exists", kind.noun(), base(&to.text)),
        )
        .at(to.position));
    }
    writer.delete(&from_key)?;
    writer.put(&to_key, &bytes)?;
    if kind == TextSearchKind::Dictionary {
        rename_in_mappings(writer, base(&name.text), base(&to.text))?;
    }
    Ok(())
}

fn rename_in_mappings(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    let catalog = Catalog::load(&*writer)?;
    for (name, configuration) in &catalog.configurations {
        let mut changed = configuration.clone();
        for dictionary in changed.mappings.values_mut().flatten() {
            if dictionary == from {
                to.clone_into(dictionary);
            }
        }
        if changed != *configuration {
            writer.put(
                &object_key(TextSearchKind::Configuration, name),
                &encode_configuration(&changed),
            )?;
        }
    }
    Ok(())
}

fn drop_objects(
    writer: &mut dyn KeyValueWrite,
    kind: TextSearchKind,
    names: &[Name],
    if_exists: bool,
) -> SqlResult<Vec<String>> {
    let mut notices = Vec::new();
    for name in names {
        let key = object_key(kind, &name.text);
        if writer.get(&key)?.is_none() {
            if if_exists {
                notices.push(format!(
                    "NOTICE:  text search {} \"{}\" does not exist, skipping",
                    kind.noun(),
                    name.text
                ));
                continue;
            }
            return Err(missing(kind, &name.text).at(name.position));
        }
        if kind == TextSearchKind::Dictionary {
            let catalog = Catalog::load(&*writer)?;
            if let Some((user, _)) = catalog
                .configurations
                .iter()
                .find(|(_, configuration)| configuration.mappings.values().flatten().any(|used| used == base(&name.text)))
            {
                return Err(SqlError::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop text search dictionary {} because text search configuration {user} depends on it",
                        base(&name.text)
                    ),
                )
                .at(name.position));
            }
        }
        writer.delete(&key)?;
    }
    Ok(notices)
}

/// Runs a text search DDL command.
pub(crate) fn run(writer: &mut dyn KeyValueWrite, command: &TextSearchCommand) -> SqlResult<QueryResult> {
    let mut notices = Vec::new();
    match command {
        TextSearchCommand::CreateDictionary { name, options } => create_dictionary(writer, name, options)?,
        TextSearchCommand::AlterDictionary { name, options } => alter_dictionary(writer, name, options)?,
        TextSearchCommand::CreateConfiguration { name, source } => create_configuration(writer, name, source)?,
        TextSearchCommand::AlterMapping { name, change } => alter_mapping(writer, name, change)?,
        TextSearchCommand::Rename { kind, name, to } => rename(writer, *kind, name, to)?,
        TextSearchCommand::Owner { kind, name } => {
            if writer.get(&object_key(*kind, &name.text))?.is_none() {
                return Err(missing(*kind, &name.text).at(name.position));
            }
        }
        TextSearchCommand::Drop { kind, names, if_exists } => {
            notices = drop_objects(writer, *kind, names, *if_exists)?;
        }
    }
    reload_if_loaded(&*writer)?;
    Ok(notices
        .into_iter()
        .fold(QueryResult::command(command.tag()), QueryResult::with_notice))
}

/// Whether a statement's tokens call a text search function or name a configuration, so the
/// database's text search objects are loaded for it.
pub(crate) fn mentions_text_search(tokens: &[Token<'_>]) -> bool {
    tokens.iter().any(|token| match &token.kind {
        TokenKind::Word(word) => TEXT_SEARCH_FUNCTIONS.iter().any(|name| word.eq_ignore_ascii_case(name)),
        TokenKind::Operator(operator) => *operator == "@@",
        _ => false,
    })
}

/// Whether stored text (a column default, a check, a routine's body) calls a text search
/// function.
pub(crate) fn text_mentions_text_search(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    TEXT_SEARCH_FUNCTIONS.iter().any(|name| lower.contains(name)) || lower.contains("@@")
}

struct Frame {
    wanted: bool,
    catalog: Option<Arc<Catalog>>,
    analyzers: BTreeMap<String, Arc<Analyzer>>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Keeps a statement's frame of loaded text search objects until it is dropped.
pub(crate) struct EnteredStatement {
    depth: usize,
}

impl Drop for EnteredStatement {
    fn drop(&mut self) {
        FRAMES.with(|frames| {
            if let Ok(mut frames) = frames.try_borrow_mut() {
                frames.truncate(self.depth);
            }
        });
    }
}

/// Starts a statement's frame; `wanted` loads the text search objects the first time the
/// statement reads the store.
pub(crate) fn enter_statement(wanted: bool) -> EnteredStatement {
    let depth = FRAMES.with(|frames| match frames.try_borrow_mut() {
        Ok(mut frames) => {
            let depth = frames.len();
            frames.push(Frame {
                wanted,
                catalog: None,
                analyzers: BTreeMap::new(),
            });
            depth
        }
        Err(_) => usize::MAX,
    });
    EnteredStatement { depth }
}

fn loaded() -> Option<Arc<Catalog>> {
    FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| frames.iter().rev().find_map(|frame| frame.catalog.clone()))
    })
}

fn install(catalog: Catalog) {
    FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut()
            && let Some(top) = frames.last_mut()
        {
            top.catalog = Some(Arc::new(catalog));
            top.analyzers.clear();
        }
    });
}

fn top_wants_loading() -> bool {
    FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| frames.last().map(|frame| frame.wanted))
            .unwrap_or(false)
    }) && loaded().is_none()
}

/// Loads the text search objects for a statement whose tokens wanted them.
pub(crate) fn load_if_wanted(reader: &dyn KeyValueRead) -> SqlResult<()> {
    if top_wants_loading() {
        install(Catalog::load(reader)?);
    }
    Ok(())
}

/// Loads the text search objects when the tokens of a statement run inside another call a
/// text search function.
pub(crate) fn load_for_tokens(reader: &dyn KeyValueRead, tokens: &[Token<'_>]) -> SqlResult<()> {
    if loaded().is_none() && mentions_text_search(tokens) {
        install(Catalog::load(reader)?);
    }
    Ok(())
}

/// Loads the text search objects for stored expressions about to run (a table's defaults,
/// checks or generated columns, a view's query) that call a text search function.
pub(crate) fn load_now(reader: &dyn KeyValueRead) -> SqlResult<()> {
    if loaded().is_none() {
        install(Catalog::load(reader)?);
    }
    Ok(())
}

fn reload_if_loaded(reader: &dyn KeyValueRead) -> SqlResult<()> {
    if loaded().is_some() {
        install(Catalog::load(reader)?);
    }
    Ok(())
}

/// The analyzer of the configuration named `name` that the database defines, or `None` when
/// it defines none by that name (or its objects were not loaded for this statement).
pub(crate) fn analyzer(name: &str) -> Option<SqlResult<Arc<Analyzer>>> {
    let key = base(name.trim()).to_owned();
    FRAMES.with(|frames| {
        let mut frames = frames.try_borrow_mut().ok()?;
        let frame = frames.iter_mut().rev().find(|frame| frame.catalog.is_some())?;
        if let Some(analyzer) = frame.analyzers.get(&key) {
            return Some(Ok(Arc::clone(analyzer)));
        }
        let built = frame.catalog.as_ref()?.analyzer(name)?;
        Some(built.map(|analyzer| {
            let analyzer = Arc::new(analyzer);
            frame.analyzers.insert(key, Arc::clone(&analyzer));
            analyzer
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let mut options = BTreeMap::new();
        options.insert("synonymlist".to_owned(), "postgres pgsql".to_owned());
        let dictionary = StoredDictionary {
            template: Template::Synonym,
            options,
        };
        assert_eq!(decode_dictionary(&encode_dictionary(&dictionary)).unwrap(), dictionary);
        let configuration = built_in_configuration("english").unwrap();
        assert_eq!(configuration.mappings.get("asciiword").unwrap(), &["english_stem".to_owned()]);
        assert_eq!(configuration.mappings.get("uint").unwrap(), &["simple".to_owned()]);
        assert_eq!(decode_configuration(&encode_configuration(&configuration)).unwrap(), configuration);
        assert!(decode_configuration(&[RECORD_VERSION, 9]).is_err());
    }
}
