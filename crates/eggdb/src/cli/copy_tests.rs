use super::Terminal;
use super::tests::{Ran, TempDirectory, run_with};

fn run(args: &[&str], stdin: &str) -> Ran {
    run_with(args, stdin.as_bytes(), Terminal::default())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn ok(args: &[&str], stdin: &str) -> String {
    let ran = run(args, stdin);
    assert_eq!(ran.exit, 0, "{args:?}: {}", text(&ran.stderr));
    text(&ran.stdout)
}

fn database(directory: &TempDirectory) -> String {
    let file = directory.file("app.db");
    ok(
        &[
            "sql",
            "--create",
            &file,
            "-c",
            "CREATE TABLE people (id integer PRIMARY KEY, name text, score numeric)",
        ],
        "",
    );
    file
}

#[test]
fn a_sql_script_carries_copy_data_and_copy_to_stdout_prints_only_the_rows() {
    let directory = TempDirectory::new("copy-sql-script");
    let file = database(&directory);
    let script =
        "COPY people FROM stdin;\n1\tada\t9.5\n2\t\\N\t\\N\n\\.\nSELECT count(*) FROM people;\n";
    assert_eq!(
        ok(&["sql", &file], script),
        "COPY 2\n count\n-------\n     2\n(1 row)\n\n"
    );
    assert_eq!(
        ok(&["sql", &file, "-c", "COPY people TO STDOUT"], ""),
        "1\tada\t9.5\n2\t\\N\t\\N\n"
    );
    assert_eq!(
        ok(
            &[
                "sql",
                &file,
                "-c",
                "COPY people FROM STDIN (FORMAT csv, HEADER)"
            ],
            "id,name,score\n3,\"Grace, Hopper\",\n"
        ),
        "COPY 1\n"
    );
    assert_eq!(
        ok(
            &[
                "sql",
                &file,
                "-c",
                "COPY (SELECT name FROM people WHERE id > 1 ORDER BY id) TO STDOUT (FORMAT csv)"
            ],
            ""
        ),
        "\n\"Grace, Hopper\"\n"
    );
    let failed = run(
        &["sql", &file, "-c", "COPY people FROM STDIN"],
        "4\tx\t1\nfive\ty\t2\n",
    );
    assert_eq!(failed.exit, 3);
    assert_eq!(
        text(&failed.stderr),
        "ERROR:  invalid input syntax for type integer: \"five\"\n\
         CONTEXT:  COPY people, line 2, column id: \"five\"\n\
         SQLSTATE:  22P02\n"
    );
    assert_eq!(
        ok(&["sql", &file, "-c", "SELECT count(*) FROM people"], ""),
        " count\n-------\n     3\n(1 row)\n\n"
    );
}

#[test]
fn the_shell_reads_copy_data_from_the_lines_after_it_and_backslash_copy_uses_files() {
    let directory = TempDirectory::new("copy-shell");
    let file = database(&directory);
    let csv = directory.file("in.csv");
    std::fs::write(&csv, "id,name,score\n10,ten,1.5\n11,\"elev\nen\",\n").unwrap();
    let out = directory.file("out.tsv");
    let script = format!(
        "copy people from stdin;\n1\tone\t1\n\\.\n\\copy people from '{csv}' with (format csv, header)\n\\copy (select id, name from people order by id) to '{out}'\n\\copy people (id) to stdout\nselect count(*) from people;\n"
    );
    assert_eq!(
        ok(&["shell", &file], &script),
        "COPY 1\nCOPY 2\nCOPY 3\n1\n10\n11\n count\n-------\n     3\n(1 row)\n\n"
    );
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "1\tone\n10\tten\n11\telev\\nen\n"
    );
    let failed = run(
        &["shell", &file, "--keep-going"],
        "copy people from stdin;\n20\ttwenty\t1\n20\tagain\t2\n21\tnot SQL\t3\n\\.\nselect count(*) from people;\n\\copy people from 'missing.csv'\n\\copy people from program 'cat'\n",
    );
    assert_eq!(failed.exit, 3, "{}", text(&failed.stderr));
    assert_eq!(text(&failed.stdout), " count\n-------\n     3\n(1 row)\n\n");
    let errors = text(&failed.stderr);
    assert!(errors.contains("duplicate key value"), "{errors}");
    assert!(errors.contains("CONTEXT:  COPY people, line 2"), "{errors}");
    assert!(errors.contains("missing.csv"), "{errors}");
    assert!(errors.contains("program is not supported"), "{errors}");
}

#[test]
fn import_csv_loads_a_file_or_stdin_into_a_table_in_one_transaction() {
    let directory = TempDirectory::new("copy-import");
    let file = database(&directory);
    let csv = directory.file("people.csv");
    std::fs::write(&csv, "id;name;score\n1;Ada;NA\n2;\"Grace; H\";7\n").unwrap();
    assert_eq!(
        ok(
            &[
                "import",
                &file,
                "--csv",
                &csv,
                "--table",
                "people",
                "--header",
                "--delimiter",
                ";",
                "--null",
                "NA",
                "-F",
                "text",
            ],
            ""
        ),
        format!("imported 2 rows from {csv} into people in {file}\n")
    );
    assert_eq!(
        ok(
            &[
                "import",
                &file,
                "--csv",
                "-",
                "--table",
                "people (name, id)"
            ],
            "Linus,3\n"
        ),
        format!(
            "{{\"file\":\"{}\",\"from\":\"stdin\",\"table\":\"people (name, id)\",\"rows\":1}}\n",
            file.replace('\\', "\\\\")
        )
    );
    let failed = run(
        &["import", &file, "--csv", "-", "--table", "people"],
        "4,x,1\n4,y,2\n",
    );
    assert_eq!(failed.exit, 3);
    let errors = text(&failed.stderr);
    assert!(errors.contains("23505"), "{errors}");
    assert!(errors.contains("Nothing was imported"), "{errors}");
    assert_eq!(
        ok(
            &["sql", &file, "-c", "COPY people TO STDOUT (FORMAT csv)"],
            ""
        ),
        "1,Ada,\n2,Grace; H,7\n3,Linus,\n"
    );
    let refused = run(
        &[
            "import",
            &file,
            "--csv",
            &csv,
            "--table",
            "people; DROP TABLE people",
        ],
        "",
    );
    assert_eq!(refused.exit, 2, "{}", text(&refused.stderr));
    let missing = run(
        &[
            "import", &file, "--csv", &csv, "--table", "nobody", "--header",
        ],
        "",
    );
    assert_eq!(missing.exit, 3);
    assert!(text(&missing.stderr).contains("relation \"nobody\" does not exist"));
}
