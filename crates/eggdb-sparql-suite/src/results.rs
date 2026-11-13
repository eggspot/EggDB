use crate::json::Json;
use eggdb_rdf::{Format, Literal, Quad, QueryOutcome, RDF_TYPE, Term, XSD_STRING};
use std::path::Path;

const RESULT_SET: &str = "http://www.w3.org/2001/sw/DataAccess/tests/result-set#";
const MAX_MATCHING_STEPS: usize = 2_000_000;

/// What a query answered, or what a test expects it to answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Answer {
    Solutions(Vec<Vec<(String, Term)>>),
    Boolean(bool),
    Graph(Vec<Quad>),
}

fn solution(mut bindings: Vec<(String, Term)>) -> Vec<(String, Term)> {
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    bindings
}

impl Answer {
    pub(crate) fn of(outcome: QueryOutcome) -> Answer {
        match outcome {
            QueryOutcome::Solutions { variables, rows } => Answer::Solutions(
                rows.into_iter()
                    .map(|row| solution(variables.iter().cloned().zip(row).filter_map(|(name, term)| Some((name, term?))).collect()))
                    .collect(),
            ),
            QueryOutcome::Boolean(answer) => Answer::Boolean(answer),
            QueryOutcome::Graph(quads) => Answer::Graph(quads),
        }
    }
}

fn xml_term(element: &crate::xml::Element) -> Option<Term> {
    match element.local_name() {
        "uri" => Some(Term::Iri(element.text())),
        "bnode" => Some(Term::Blank(element.text())),
        "literal" => {
            let text = element.text();
            Some(Term::Literal(if let Some(language) = element.attribute("xml:lang") {
                Literal::tagged(&text, language)
            } else if let Some(datatype) = element.attribute("datatype") {
                Literal::typed(&text, datatype)
            } else {
                Literal::string(&text)
            }))
        }
        _ => None,
    }
}

fn read_srx(text: &str) -> Result<Answer, String> {
    let root = crate::xml::parse(text).ok_or("the .srx file is not XML")?;
    if let Some(boolean) = root.child("boolean") {
        return Ok(Answer::Boolean(boolean.text().trim() == "true"));
    }
    let results = root.child("results").ok_or("the .srx file has neither results nor boolean")?;
    let mut rows = Vec::new();
    for result in results.elements().filter(|element| element.local_name() == "result") {
        let mut bindings = Vec::new();
        for binding in result.elements().filter(|element| element.local_name() == "binding") {
            let name = binding.attribute("name").ok_or("a binding has no name")?.to_owned();
            let term = binding.elements().find_map(xml_term).ok_or("a binding has no value")?;
            bindings.push((name, term));
        }
        rows.push(solution(bindings));
    }
    Ok(Answer::Solutions(rows))
}

fn json_term(json: &Json) -> Option<Term> {
    let value = json.field("value")?.text()?;
    Some(match json.field("type")?.text()? {
        "uri" => Term::Iri(value.to_owned()),
        "bnode" => Term::Blank(value.to_owned()),
        "literal" | "typed-literal" => Term::Literal(if let Some(language) = json.field("xml:lang").and_then(Json::text) {
            Literal::tagged(value, language)
        } else if let Some(datatype) = json.field("datatype").and_then(Json::text) {
            Literal::typed(value, datatype)
        } else {
            Literal::string(value)
        }),
        _ => return None,
    })
}

fn read_srj(text: &str) -> Result<Answer, String> {
    let root = crate::json::parse(text).ok_or("the .srj file is not JSON")?;
    if let Some(Json::Bool(answer)) = root.field("boolean") {
        return Ok(Answer::Boolean(*answer));
    }
    let Some(Json::Array(bindings)) = root.field("results").and_then(|results| results.field("bindings")) else {
        return Err("the .srj file has neither results.bindings nor boolean".into());
    };
    let mut rows = Vec::new();
    for row in bindings {
        let Json::Object(fields) = row else {
            return Err("a .srj solution is not an object".into());
        };
        let mut found = Vec::new();
        for (name, value) in fields {
            found.push((name.clone(), json_term(value).ok_or("a .srj binding is not a term")?));
        }
        rows.push(solution(found));
    }
    Ok(Answer::Solutions(rows))
}

fn objects<'q>(quads: &'q [Quad], subject: &'q Term, predicate: &'q str) -> impl Iterator<Item = &'q Term> {
    quads
        .iter()
        .filter(move |quad| quad.subject == *subject && matches!(&quad.predicate, Term::Iri(iri) if iri == predicate))
        .map(|quad| &quad.object)
}

fn result_set(quads: &[Quad]) -> Option<Answer> {
    let set_type = Term::Iri(format!("{RESULT_SET}ResultSet"));
    let set = quads
        .iter()
        .find(|quad| matches!(&quad.predicate, Term::Iri(iri) if iri == RDF_TYPE) && quad.object == set_type)?
        .subject
        .clone();
    if let Some(Term::Literal(answer)) = objects(quads, &set, &format!("{RESULT_SET}boolean")).next() {
        return Some(Answer::Boolean(answer.lexical == "true"));
    }
    let binding_predicate = format!("{RESULT_SET}binding");
    let variable_predicate = format!("{RESULT_SET}variable");
    let value_predicate = format!("{RESULT_SET}value");
    let mut rows = Vec::new();
    for row in objects(quads, &set, &format!("{RESULT_SET}solution")) {
        let mut bindings = Vec::new();
        for binding in objects(quads, row, &binding_predicate) {
            let name = match objects(quads, binding, &variable_predicate).next() {
                Some(Term::Literal(name)) => name.lexical.clone(),
                _ => continue,
            };
            if let Some(value) = objects(quads, binding, &value_predicate).next() {
                bindings.push((name, value.clone()));
            }
        }
        rows.push(solution(bindings));
    }
    Some(Answer::Solutions(rows))
}

/// The answer a test's result file states.
pub(crate) fn read_expected(path: &Path, base: &str) -> Result<Answer, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or_default();
    match extension {
        "srx" => read_srx(&text),
        "srj" => read_srj(&text),
        "ttl" | "nt" | "nq" | "trig" => {
            let format = Format::named(extension).ok_or("an RDF result file of an unknown format")?;
            let quads = eggdb_rdf::parse(&text, format, base).map_err(|error| error.message)?;
            Ok(result_set(&quads).unwrap_or(Answer::Graph(quads)))
        }
        other => Err(format!("result files in .{other} are not read yet")),
    }
}

fn is_blank(term: &Term) -> bool {
    matches!(term, Term::Blank(_))
}

fn simple(term: &Term) -> Term {
    match term {
        Term::Literal(literal) if literal.datatype.is_empty() => Term::Literal(Literal::typed(&literal.lexical, XSD_STRING)),
        other => other.clone(),
    }
}

struct Matching {
    forward: Vec<(String, String)>,
    steps: usize,
}

impl Matching {
    fn same(&mut self, expected: &Term, actual: &Term, added: &mut Vec<(String, String)>) -> bool {
        match (expected, actual) {
            (Term::Blank(left), Term::Blank(right)) => {
                if let Some((_, mapped)) = self.forward.iter().find(|(from, _)| from == left) {
                    return mapped == right;
                }
                if self.forward.iter().any(|(_, to)| to == right) {
                    return false;
                }
                self.forward.push((left.clone(), right.clone()));
                added.push((left.clone(), right.clone()));
                true
            }
            (left, right) => simple(left) == simple(right),
        }
    }

    fn undo(&mut self, added: &[(String, String)]) {
        self.forward.retain(|pair| !added.contains(pair));
    }

    fn rows(&mut self, expected: &[Vec<Term>], actual: &[Vec<Term>], used: &mut Vec<bool>, at: usize, ordered: bool) -> bool {
        self.steps += 1;
        if self.steps > MAX_MATCHING_STEPS {
            return false;
        }
        let Some(row) = expected.get(at) else {
            return true;
        };
        let candidates: Vec<usize> = if ordered { vec![at] } else { (0..actual.len()).filter(|index| !used[*index]).collect() };
        for candidate in candidates {
            let Some(other) = actual.get(candidate) else {
                continue;
            };
            if other.len() != row.len() {
                continue;
            }
            let mut added = Vec::new();
            if row.iter().zip(other).all(|(left, right)| self.same(left, right, &mut added)) {
                used[candidate] = true;
                if self.rows(expected, actual, used, at + 1, ordered) {
                    return true;
                }
                used[candidate] = false;
            }
            self.undo(&added);
        }
        false
    }
}

fn flattened(rows: &[Vec<(String, Term)>]) -> (Vec<Vec<String>>, Vec<Vec<Term>>) {
    (
        rows.iter().map(|row| row.iter().map(|(name, _)| name.clone()).collect()).collect(),
        rows.iter().map(|row| row.iter().map(|(_, term)| term.clone()).collect()).collect(),
    )
}

fn quad_terms(quads: &[Quad]) -> Vec<Vec<Term>> {
    quads
        .iter()
        .map(|quad| vec![quad.subject.clone(), quad.predicate.clone(), quad.object.clone(), quad.graph.clone().unwrap_or(Term::Iri(String::new()))])
        .collect()
}

fn same_rows(expected: &[Vec<Term>], actual: &[Vec<Term>], ordered: bool) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let blanks = expected.iter().chain(actual).flatten().any(is_blank);
    if !blanks && !ordered {
        let key = |rows: &[Vec<Term>]| {
            let mut keys: Vec<String> = rows.iter().map(|row| row.iter().map(|term| simple(term).to_ntriples()).collect::<Vec<_>>().join(" ")).collect();
            keys.sort();
            keys
        };
        return key(expected) == key(actual);
    }
    let mut matching = Matching { forward: Vec::new(), steps: 0 };
    matching.rows(expected, actual, &mut vec![false; actual.len()], 0, ordered)
}

/// Whether a query's answer is the one the test expects: solutions as a multiset (a sequence
/// when the query orders them) and graphs as sets, with blank nodes matched one-to-one.
pub(crate) fn same_answer(expected: &Answer, actual: &Answer, ordered: bool) -> bool {
    match (expected, actual) {
        (Answer::Boolean(left), Answer::Boolean(right)) => left == right,
        (Answer::Solutions(left), Answer::Solutions(right)) => {
            let (left_names, left_terms) = flattened(left);
            let (right_names, right_terms) = flattened(right);
            if !ordered {
                let mut left_shapes = left_names.clone();
                let mut right_shapes = right_names.clone();
                left_shapes.sort();
                right_shapes.sort();
                if left_shapes != right_shapes {
                    return false;
                }
            } else if left_names != right_names {
                return false;
            }
            let tagged = |names: &[Vec<String>], terms: &[Vec<Term>]| -> Vec<Vec<Term>> {
                names
                    .iter()
                    .zip(terms)
                    .map(|(names, terms)| names.iter().map(|name| Term::Iri(format!("?{name}"))).chain(terms.iter().cloned()).collect())
                    .collect()
            };
            same_rows(&tagged(&left_names, &left_terms), &tagged(&right_names, &right_terms), ordered)
        }
        (Answer::Graph(left), Answer::Graph(right)) => {
            let mut left = quad_terms(left);
            let mut right = quad_terms(right);
            left.dedup();
            right.dedup();
            same_rows(&left, &right, false)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_files_read_and_compare_with_blank_nodes_matched() {
        let srx = r#"<?xml version="1.0"?><sparql xmlns="http://www.w3.org/2005/sparql-results#"><head><variable name="x"/></head>
            <results><result><binding name="x"><bnode>b0</bnode></binding></result><result><binding name="x"><literal xml:lang="en">a &amp; b</literal></binding></result></results></sparql>"#;
        let expected = read_srx(srx).unwrap();
        let srj = r#"{"head":{"vars":["x"]},"results":{"bindings":[{"x":{"type":"literal","xml:lang":"en","value":"a & b"}},{"x":{"type":"bnode","value":"z9"}}]}}"#;
        let actual = read_srj(srj).unwrap();
        assert!(same_answer(&expected, &actual, false));
        assert!(!same_answer(&expected, &actual, true));
        assert!(same_answer(&Answer::Boolean(true), &read_srj(r#"{"head":{},"boolean":true}"#).unwrap(), false));
    }
}
