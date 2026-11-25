//! `eggdb-docsite <docs directory> <output directory>`: writes every Markdown page under the docs
//! directory as a static HTML page at the same place under the output directory, with its
//! `.md` links pointing at the `.html` pages, a navigation bar of the top-level pages, a table of
//! contents, and an `index.html` listing every page; other files are copied as they are.

mod markdown;
mod site;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [docs, output] = arguments.as_slice() else {
        eprintln!("usage: eggdb-docsite <docs directory> <output directory>");
        return ExitCode::from(2);
    };
    match site::build(&PathBuf::from(docs), &PathBuf::from(output)) {
        Ok(pages) => {
            println!("wrote {pages} pages to {output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("eggdb-docsite: {error}");
            ExitCode::FAILURE
        }
    }
}
