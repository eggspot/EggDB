//! Query results in the W3C formats: SPARQL 1.1 Query Results JSON, the SPARQL Query Results
//! XML Format, and SPARQL 1.1 Query Results CSV and TSV; graphs as N-Triples.

use crate::sparql_eval::QueryOutcome;
use crate::term::{Term, XSD_STRING, escaped_string};

/// A results format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultFormat {
    /// `application/sparql-results+json`.
    Json,
    /// `application/sparql-results+xml`.
    Xml,
    /// `text/csv`.
    Csv,
    /// `text/tab-separated-values`.
    Tsv,
}

impl ResultFormat {
    /// The format an HTTP `Accept` header or a `format` parameter asks for: the first known
    /// media type or short name in it, JSON otherwise.
    pub fn negotiated(accept: &str) -> Self {
        for part in accept.split(',') {
            let media = part.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
            let found = match media.as_str() {
                "application/sparql-results+json" | "application/json" | "json" => Some(Self::Json),
                "application/sparql-results+xml" | "application/xml" | "text/xml" | "xml" => Some(Self::Xml),
                "text/csv" | "csv" => Some(Self::Csv),
                "text/tab-separated-values" | "tsv" => Some(Self::Tsv),
                _ => None,
            };
            if let Some(found) = found {
                return found;
            }
        }
        Self::Json
    }

    /// The media type the format is sent as.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Json => "application/sparql-results+json",
            Self::Xml => "application/sparql-results+xml",
            Self::Csv => "text/csv; charset=utf-8",
            Self::Tsv => "text/tab-separated-values; charset=utf-8",
        }
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn xml_escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn json_term(term: &Term) -> String {
    match term {
        Term::Iri(iri) => format!("{{\"type\":\"uri\",\"value\":{}}}", json_string(iri)),
        Term::Blank(label) => format!("{{\"type\":\"bnode\",\"value\":{}}}", json_string(label)),
        Term::Literal(literal) => {
            let mut out = format!("{{\"type\":\"literal\",\"value\":{}", json_string(&literal.lexical));
            if let Some(language) = &literal.language {
                out.push_str(&format!(",\"xml:lang\":{}", json_string(language)));
            } else if literal.datatype != XSD_STRING {
                out.push_str(&format!(",\"datatype\":{}", json_string(&literal.datatype)));
            }
            out.push('}');
            out
        }
    }
}

fn xml_term(term: &Term) -> String {
    match term {
        Term::Iri(iri) => format!("<uri>{}</uri>", xml_escaped(iri)),
        Term::Blank(label) => format!("<bnode>{}</bnode>", xml_escaped(label)),
        Term::Literal(literal) => match &literal.language {
            Some(language) => format!("<literal xml:lang=\"{}\">{}</literal>", xml_escaped(language), xml_escaped(&literal.lexical)),
            None if literal.datatype == XSD_STRING => format!("<literal>{}</literal>", xml_escaped(&literal.lexical)),
            None => format!(
                "<literal datatype=\"{}\">{}</literal>",
                xml_escaped(&literal.datatype),
                xml_escaped(&literal.lexical)
            ),
        },
    }
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

fn csv_term(term: &Term) -> String {
    match term {
        Term::Iri(iri) => csv_field(iri),
        Term::Blank(label) => format!("_:{label}"),
        Term::Literal(literal) => csv_field(&literal.lexical),
    }
}

fn graph_text(quads: &[crate::term::Quad]) -> String {
    let mut out = String::new();
    for quad in quads {
        out.push_str(&format!(
            "{} {} {} .\n",
            quad.subject.to_ntriples(),
            quad.predicate.to_ntriples(),
            quad.object.to_ntriples()
        ));
    }
    out
}

/// The outcome written in `format`, with its media type; a graph is always N-Triples.
pub fn write_results(outcome: &QueryOutcome, format: ResultFormat) -> (String, String) {
    let (variables, rows) = match outcome {
        QueryOutcome::Graph(quads) => return ("application/n-triples".to_owned(), graph_text(quads)),
        QueryOutcome::Boolean(answer) => {
            let body = match format {
                ResultFormat::Xml => format!(
                    "<?xml version=\"1.0\"?>\n<sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"><head/><boolean>{answer}</boolean></sparql>\n"
                ),
                ResultFormat::Csv | ResultFormat::Tsv => format!("{answer}\n"),
                ResultFormat::Json => format!("{{\"head\":{{}},\"boolean\":{answer}}}"),
            };
            return (format.media_type().to_owned(), body);
        }
        QueryOutcome::Solutions { variables, rows } => (variables, rows),
    };
    let body = match format {
        ResultFormat::Json => {
            let names: Vec<String> = variables.iter().map(|name| json_string(name)).collect();
            let bindings: Vec<String> = rows
                .iter()
                .map(|row| {
                    let fields: Vec<String> = variables
                        .iter()
                        .zip(row)
                        .filter_map(|(name, value)| value.as_ref().map(|term| format!("{}:{}", json_string(name), json_term(term))))
                        .collect();
                    format!("{{{}}}", fields.join(","))
                })
                .collect();
            format!(
                "{{\"head\":{{\"vars\":[{}]}},\"results\":{{\"bindings\":[{}]}}}}",
                names.join(","),
                bindings.join(",")
            )
        }
        ResultFormat::Xml => {
            let mut out = String::from("<?xml version=\"1.0\"?>\n<sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"><head>");
            for name in variables {
                out.push_str(&format!("<variable name=\"{}\"/>", xml_escaped(name)));
            }
            out.push_str("</head><results>");
            for row in rows {
                out.push_str("<result>");
                for (name, value) in variables.iter().zip(row) {
                    if let Some(term) = value {
                        out.push_str(&format!("<binding name=\"{}\">{}</binding>", xml_escaped(name), xml_term(term)));
                    }
                }
                out.push_str("</result>");
            }
            out.push_str("</results></sparql>\n");
            out
        }
        ResultFormat::Csv => {
            let mut out = variables.iter().map(|name| csv_field(name)).collect::<Vec<_>>().join(",");
            out.push_str("\r\n");
            for row in rows {
                let fields: Vec<String> = row.iter().map(|value| value.as_ref().map_or_else(String::new, csv_term)).collect();
                out.push_str(&fields.join(","));
                out.push_str("\r\n");
            }
            out
        }
        ResultFormat::Tsv => {
            let mut out = variables.iter().map(|name| format!("?{name}")).collect::<Vec<_>>().join("\t");
            out.push('\n');
            for row in rows {
                let fields: Vec<String> = row
                    .iter()
                    .map(|value| {
                        value.as_ref().map_or_else(String::new, |term| match term {
                            Term::Literal(literal) if literal.datatype == XSD_STRING && literal.language.is_none() => {
                                format!("\"{}\"", escaped_string(&literal.lexical).replace('\t', "\\t"))
                            }
                            other => other.to_ntriples().replace('\t', "\\t"),
                        })
                    })
                    .collect();
                out.push_str(&fields.join("\t"));
                out.push('\n');
            }
            out
        }
    };
    (format.media_type().to_owned(), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Literal;

    fn outcome() -> QueryOutcome {
        QueryOutcome::Solutions {
            variables: vec!["s".to_owned(), "name".to_owned()],
            rows: vec![
                vec![Some(Term::iri("http://ex/a")), Some(Term::Literal(Literal::tagged("Ann, Jr", "en")))],
                vec![Some(Term::Blank("b1".to_owned())), None],
            ],
        }
    }

    #[test]
    fn solutions_are_written_in_each_format() {
        let (_, json) = write_results(&outcome(), ResultFormat::Json);
        assert_eq!(
            json,
            "{\"head\":{\"vars\":[\"s\",\"name\"]},\"results\":{\"bindings\":[{\"s\":{\"type\":\"uri\",\"value\":\"http://ex/a\"},\"name\":{\"type\":\"literal\",\"value\":\"Ann, Jr\",\"xml:lang\":\"en\"}},{\"s\":{\"type\":\"bnode\",\"value\":\"b1\"}}]}}"
        );
        let (_, csv) = write_results(&outcome(), ResultFormat::Csv);
        assert_eq!(csv, "s,name\r\nhttp://ex/a,\"Ann, Jr\"\r\n_:b1,\r\n");
        let (_, tsv) = write_results(&outcome(), ResultFormat::Tsv);
        assert_eq!(tsv, "?s\t?name\n<http://ex/a>\t\"Ann, Jr\"@en\n_:b1\t\n");
        let (_, xml) = write_results(&QueryOutcome::Boolean(true), ResultFormat::Xml);
        assert!(xml.contains("<boolean>true</boolean>"));
        assert_eq!(ResultFormat::negotiated("text/csv;q=0.9, */*"), ResultFormat::Csv);
    }
}
