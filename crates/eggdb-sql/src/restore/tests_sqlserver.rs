use super::classify::{Action, Effect, Ignored};
use super::classify_sqlserver::{classify_sqlserver, sqlserver_table_key, takes_rest_of_batch};
use super::column_rewrite::{AliasTypes, rewritten_table};
use super::script::{ScriptItem, ScriptSource};
use super::script_reader::{BatchItem, ScriptReader};
use super::script_text::DecodedLines;
use super::statement_spans::{SqlServerScript, statement_boundaries, unparsed_statement_end};
use super::{RestoreOptions, RestoreReport, restore_dump, sqlserver_batches};
use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;
use std::io::{BufRead, BufReader, Read};

fn utf16(text: &str) -> Vec<u8> {
    let mut bytes = b"\xFF\xFE".to_vec();
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

fn utf16_big_endian(text: &str) -> Vec<u8> {
    let mut bytes = b"\xFE\xFF".to_vec();
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    bytes
}

fn batches_of(bytes: &[u8]) -> Result<Vec<BatchItem>, crate::SqlError> {
    let mut input = bytes;
    let mut reader = ScriptReader::new(&mut input);
    let mut items = Vec::new();
    loop {
        let item = reader.next_item()?;
        if item == BatchItem::End {
            return Ok(items);
        }
        items.push(item);
    }
}

fn batch_texts(text: &str) -> Vec<(String, u64, u64)> {
    batches_of(text.as_bytes())
        .unwrap()
        .into_iter()
        .filter_map(|item| match item {
            BatchItem::Batch(batch) => Some((batch.text, batch.line, batch.repeat)),
            _ => None,
        })
        .collect()
}

fn statements_of(bytes: &[u8]) -> Result<Vec<(String, u64)>, crate::SqlError> {
    let mut input = bytes;
    let mut script = SqlServerScript::new(&mut input);
    let mut statements = Vec::new();
    loop {
        match script.next_item()? {
            ScriptItem::End => return Ok(statements),
            ScriptItem::Statement(statement) => statements.push((statement.text, statement.line)),
            ScriptItem::ClientCommand(_) | ScriptItem::Failed(_) => {}
        }
    }
}

fn restore(store: &MemoryStore, script: &[u8], keep_going: bool) -> RestoreReport {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    let mut input = script;
    let report = restore_dump(
        &mut session,
        &mut input,
        RestoreOptions {
            keep_going,
            ..RestoreOptions::default()
        },
        &mut |_| {},
    );
    assert_eq!(session.close(), Ok(false));
    report
}

fn query(store: &MemoryStore, sql: &str) -> String {
    let mut session = Session::new(store);
    let mut out = Vec::new();
    session
        .execute(sql, &mut |result| out.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn go_ends_a_batch_only_alone_on_its_line_outside_strings_and_comments() {
    let script = "SELECT 1\nGO\nselect 'a\ngo\nb'\n  go  \nSELECT 2 /*\nGO\n*/\nGo -- done\nSELECT 3\nGO /* c */ --x\nSELECT 4\nGO 2\nSELECT 5\nGOTO x\ngo;\nSELECT [go\nGO\n]\nGO\nSELECT 6";
    let batches = batch_texts(script);
    let texts: Vec<&str> = batches.iter().map(|(text, _, _)| text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "SELECT 1\n",
            "select 'a\ngo\nb'\n",
            "SELECT 2 /*\nGO\n*/\n",
            "SELECT 3\n",
            "SELECT 4\n",
            "SELECT 5\nGOTO x\ngo;\nSELECT [go\nGO\n]\n",
            "SELECT 6",
        ]
    );
    let lines: Vec<u64> = batches.iter().map(|(_, line, _)| *line).collect();
    assert_eq!(lines, [1, 3, 7, 11, 13, 15, 22]);
    assert_eq!(batches[4].2, 2);
    assert!(batches_of(b"SELECT 1\nGO /* open\n*/\n").is_err());
    assert!(batches_of(b"SELECT 1\nGO 0\n").is_err());
    assert!(batches_of(b"SELECT 'unclosed\n").is_err());
}

#[test]
fn utf16_scripts_of_either_byte_order_decode_with_surrogates_across_reads_and_keep_their_crlf() {
    let text = "INSERT t VALUES (N'a\r\nb 🎉 ế')\r\nGO\r\n";
    let bytes = utf16(text);
    let mut one_byte_at_a_time = BufReader::with_capacity(1, &bytes[..]);
    let mut lines = DecodedLines::new(&mut one_byte_at_a_time);
    let mut decoded = String::new();
    let mut line = String::new();
    while lines.next_line(&mut line).unwrap() {
        decoded.push_str(&line);
    }
    assert_eq!(decoded, text);
    assert_eq!(lines.line_number(), 3);
    let statements = statements_of(&bytes).unwrap();
    assert_eq!(
        statements,
        [("INSERT t VALUES (N'a\r\nb 🎉 ế')".to_owned(), 1)]
    );
    let mut lone = utf16("SELECT 1");
    lone.extend_from_slice(&0xD800u16.to_le_bytes());
    lone.extend_from_slice(b"A\0");
    assert!(statements_of(&lone).is_err());
    let mut odd = utf16("SELECT 1");
    odd.push(b'A');
    assert!(statements_of(&odd).is_err());
    assert_eq!(
        statements_of(&utf16_big_endian(text)).unwrap(),
        [("INSERT t VALUES (N'a\r\nb 🎉 ế')".to_owned(), 1)]
    );
    let mut odd_big_endian = utf16_big_endian("SELECT 1");
    odd_big_endian.push(b'A');
    assert!(statements_of(&odd_big_endian).is_err());
    let mut lone_big_endian = utf16_big_endian("SELECT 1");
    lone_big_endian.extend_from_slice(&0xDC00u16.to_be_bytes());
    assert!(statements_of(&lone_big_endian).is_err());
    let mut bad_utf8 = b"SELECT 'a".to_vec();
    bad_utf8.push(0xFF);
    bad_utf8.extend_from_slice(b"'\n");
    assert!(statements_of(&bad_utf8).is_err());
    assert_eq!(
        statements_of(b"\xEF\xBB\xBFSELECT 1\r\n").unwrap(),
        [("SELECT 1".to_owned(), 1)]
    );
}

#[test]
fn sqlcmd_variables_are_replaced_only_after_a_sqlcmd_command_and_must_be_defined() {
    let script = b"SELECT N'$(early)'\nGO\n:setvar Name \"egg \"\"one\"\"\"\nSELECT N'$(name)', N'$(not a name)'\nGO\n:r other.sql\n!! rm -rf /\n:connect server\n";
    let items = batches_of(script).unwrap();
    let batches: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            BatchItem::Batch(batch) => Some(batch.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        batches,
        [
            "SELECT N'$(early)'\n",
            "SELECT N'egg \"one\"', N'$(not a name)'\n"
        ]
    );
    let commands: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            BatchItem::Command(command) => Some(command.shown.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(commands, [":setvar", ":r", "!!", ":connect"]);
    let error = batches_of(b":on error exit\nSELECT '$(missing)'\nGO\n").unwrap_err();
    assert!(
        error
            .message()
            .contains("'missing' scripting variable not defined.")
    );
    assert!(batches_of(b":setvar\n").is_err());
    let mut many = String::new();
    for index in 0..=1024 {
        many.push_str(&format!(":setvar v{index} 1\n"));
    }
    assert!(batches_of(many.as_bytes()).is_err());
    let quit = batches_of(b"SELECT 1\nGO\nSELECT 2\n:quit\nSELECT 3\nGO\n").unwrap();
    assert_eq!(
        quit.iter()
            .filter(|item| matches!(item, BatchItem::Batch(_)))
            .count(),
        1
    );
    assert!(batches_of(b"SELECT 1\nGO 2000000000\n").is_err());
}

#[test]
fn statements_of_a_batch_are_found_by_the_parser_and_a_refused_one_by_its_line() {
    let script = "SET ANSI_NULLS ON\nSET QUOTED_IDENTIFIER ON;\nINSERT [dbo].[T] ([a]) VALUES (1)\nINSERT [dbo].[T] ([a]) VALUES (2)\nGO\nGRANT SELECT ON [dbo].[T] TO [u]\nDENY SELECT ON [dbo].[T] TO [u]\nALTER DATABASE [x] SET RECOVERY FULL\nUPDATE STATISTICS [dbo].[T]\nGO\n";
    let statements = statements_of(script.as_bytes()).unwrap();
    let texts: Vec<(&str, u64)> = statements
        .iter()
        .map(|(text, line)| (text.as_str(), *line))
        .collect();
    assert_eq!(
        texts,
        [
            ("SET ANSI_NULLS ON", 1),
            ("SET QUOTED_IDENTIFIER ON;", 2),
            ("INSERT [dbo].[T] ([a]) VALUES (1)", 3),
            ("INSERT [dbo].[T] ([a]) VALUES (2)", 4),
            ("GRANT SELECT ON [dbo].[T] TO [u]", 6),
            ("DENY SELECT ON [dbo].[T] TO [u]", 7),
            ("ALTER DATABASE [x] SET RECOVERY FULL", 8),
            ("UPDATE STATISTICS [dbo].[T]", 9),
        ]
    );
    for (text, end) in [
        ("ALTER DATABASE x SET A = 1;SELECT 1", 27),
        ("GRANT a ON (b\nSELECT) TO c\nSELECT 1", 27),
        ("REVOKE x\n  'a\nSELECT' y\nselect 1", 24),
        ("DENY x /* \nSELECT */ y", 22),
    ] {
        assert_eq!(unparsed_statement_end(text, 0), end, "{text:?}");
    }
}

#[test]
fn batches_the_lexer_refuses_are_classified_by_their_leading_words() {
    let script = "CREATE PROCEDURE [dbo].[P] AS\nSELECT * INTO #t FROM x; SELECT { fn now() }, $action\nGO\nCREATE TRIGGER [dbo].[tr] ON [dbo].[T] AFTER INSERT AS SELECT 1\nGO\nIF 1 = 1 SELECT #x\nGO\n";
    let statements = statements_of(script.as_bytes()).unwrap();
    assert_eq!(statements.len(), 3);
    assert!(takes_rest_of_batch(
        "create or alter procedure p as select 1"
    ));
    assert!(takes_rest_of_batch("  begin try select 1 end try"));
    assert!(!takes_rest_of_batch("BEGIN TRAN"));
    assert!(!takes_rest_of_batch("INSERT t VALUES (1)"));
}

#[test]
fn the_classifier_runs_skips_or_ignores_by_leading_words() {
    let known = |_: &str| false;
    let run = |text: &str| match classify_sqlserver(text, &known) {
        Action::Run(effect) => Some(effect),
        _ => None,
    };
    assert_eq!(
        run("INSERT [dbo].[Orders] ([a]) VALUES (1)"),
        Some(Effect::Insert("dbo.Orders".to_owned()))
    );
    for trigger in [
        "CREATE TRIGGER [dbo].[tr] ON [dbo].[T] AFTER INSERT AS SELECT 1",
        "CREATE OR ALTER TRIGGER tr ON T INSTEAD OF DELETE AS SELECT 1",
        "ALTER TRIGGER [dbo].[tr] ON [dbo].[T] AFTER UPDATE AS SELECT 1",
        "ALTER TABLE [a] ENABLE TRIGGER [t]",
        "DISABLE TRIGGER [t] ON [a]",
    ] {
        assert_eq!(run(trigger), Some(Effect::OtherDdl), "{trigger}");
    }
    assert!(matches!(
        classify_sqlserver("CREATE TRIGGER audit ON DATABASE FOR CREATE_TABLE AS SELECT 1", &known),
        Action::Skip(_)
    ));
    assert_eq!(
        run("insert into sales.Lines values (1)"),
        Some(Effect::Insert("sales.lines".to_owned()))
    );
    assert_eq!(
        run("SET IDENTITY_INSERT [sales].[Orders] ON"),
        Some(Effect::IdentityInsert("sales.Orders".to_owned()))
    );
    assert_eq!(
        run(
            "ALTER TABLE [dbo].[A]  WITH NOCHECK ADD  CONSTRAINT [FK_A] FOREIGN KEY([b]) REFERENCES [dbo].[B] ([id])"
        ),
        Some(Effect::AddConstraint {
            table: "dbo.A".to_owned(),
            name: Some("FK_A".to_owned())
        })
    );
    assert_eq!(
        run("ALTER TABLE [dbo].[A] ADD  DEFAULT (getdate()) FOR [At]"),
        Some(Effect::SetDefault {
            object: "dbo.A.At".to_owned(),
            default: "(getdate())".to_owned()
        })
    );
    assert_eq!(run("SET ANSI_NULLS ON"), Some(Effect::Setting));
    assert_eq!(run("SET NUMERIC_ROUNDABORT OFF"), Some(Effect::Setting));
    for (text, kind) in [
        ("USE [master]", "USE"),
        ("PRINT N'x'", "PRINT"),
        ("ALTER DATABASE [x] SET QUERY_STORE = ON", "ALTER DATABASE"),
        ("CREATE DATABASE [x]", "CREATE DATABASE"),
        (
            "ALTER TABLE [a] NOCHECK CONSTRAINT ALL",
            "NOCHECK CONSTRAINT",
        ),
        (
            "EXEC sys.sp_addextendedproperty @name=N'Caption', @value=N'x'",
            "extended properties",
        ),
        ("BEGIN TRANSACTION", "BEGIN and COMMIT"),
        (
            "CREATE PARTITION FUNCTION pf (int) AS RANGE LEFT FOR VALUES (1)",
            "partition functions and schemes",
        ),
    ] {
        assert_eq!(
            classify_sqlserver(text, &known),
            Action::Ignore(Ignored::Other(kind)),
            "{text}"
        );
    }
    assert_eq!(
        classify_sqlserver("SET QUOTED_IDENTIFIER OFF", &known),
        Action::Ignore(Ignored::Set("QUOTED_IDENTIFIER".to_owned()))
    );
    assert_eq!(
        classify_sqlserver("SET XACT_ABORT ON", &known),
        Action::Run(Effect::Setting)
    );
    assert_eq!(
        classify_sqlserver("SET NOEXEC ON", &known),
        Action::Ignore(Ignored::Set("NOEXEC".to_owned()))
    );
    assert_eq!(
        classify_sqlserver(
            "CREATE SEQUENCE [dbo].[Numbers] AS [bigint] START WITH 1",
            &known
        ),
        Action::Run(Effect::Sequence("dbo.Numbers".to_owned()))
    );
    assert_eq!(
        classify_sqlserver("CREATE TYPE [dbo].[Phone] FROM [nvarchar](20) NOT NULL", &known),
        Action::Ignore(Ignored::Other("alias types"))
    );
    assert_eq!(
        classify_sqlserver(
            "EXEC sys.sp_addextendedproperty @name=N'MS_Description', @value=N'x', @level0type=N'SCHEMA', @level0name=N'dbo'",
            &known
        ),
        Action::Run(Effect::Comment)
    );
    for (text, kind) in [
        ("CREATE PROC [dbo].[p] AS SELECT 1", "procedure"),
        (
            "CREATE OR ALTER FUNCTION f() RETURNS int AS BEGIN RETURN 1 END",
            "function",
        ),
        ("CREATE SYNONYM [dbo].[s] FOR [x].[y]", "synonym"),
        ("CREATE TYPE [dbo].[t] AS TABLE (a int)", "type"),
        ("CREATE TYPE [dbo].[t] EXTERNAL NAME [a].[b]", "type"),
        ("CREATE PRIMARY XML INDEX [x] ON [t] ([c])", "index"),
        (
            "CREATE ASYMMETRIC KEY k WITH ALGORITHM = RSA_2048",
            "server object",
        ),
        ("DBCC CHECKDB", "statement"),
        (
            "MERGE t USING s ON 1 = 1 WHEN MATCHED THEN DELETE;",
            "statement",
        ),
        ("DECLARE @a int", "statement"),
        ("EXEC dbo.Something 1", "statement"),
    ] {
        match classify_sqlserver(text, &known) {
            Action::Skip(skip) => assert_eq!(skip.kind, kind, "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
    assert_eq!(sqlserver_table_key("dbo.Orders", "public"), "public.orders");
    assert_eq!(
        sqlserver_table_key("Sales.Orders", "public"),
        "sales.orders"
    );
    assert_eq!(sqlserver_table_key("Orders", "public"), "public.orders");
}

#[test]
fn computed_and_rowversion_columns_are_cut_and_identity_seeds_kept() {
    let text = "CREATE TABLE [dbo].[P](\n\t[Id] [int] IDENTITY(5, -2) NOT NULL,\n\t[a] [money] NULL,\n\t[b]  AS ([a]*(2)) PERSISTED,\n\t[v] [timestamp] NOT NULL,\n\t[Identity_X] [int] NULL,\n CONSTRAINT [PK_P] PRIMARY KEY CLUSTERED ([Id] ASC)\n) ON [PRIMARY]";
    let no_aliases = AliasTypes::default();
    let rewrite = rewritten_table(text, &no_aliases).unwrap();
    assert_eq!(
        rewrite.text,
        "CREATE TABLE [dbo].[P](\n\t[Id] [int] IDENTITY(5, -2) NOT NULL,\n\t[a] [money] NULL,\n\t[Identity_X] [int] NULL,\n CONSTRAINT [PK_P] PRIMARY KEY CLUSTERED ([Id] ASC)\n) ON [PRIMARY]"
    );
    let shown: Vec<String> = rewrite
        .notes
        .iter()
        .map(|note| format!("{}: {}", note.object, note.difference))
        .collect();
    assert_eq!(
        shown,
        [
            "dbo.P.b: computed column not kept",
            "dbo.P.v: rowversion column not kept",
        ]
    );
    assert!(!rewrite.identity_not_kept);
    assert!(rewritten_table("CREATE TABLE t (a int IDENTITY(1,1))", &no_aliases).is_none());
    assert!(rewritten_table("CREATE TABLE t (a AS (1))", &no_aliases).is_none());
    assert!(rewritten_table("INSERT t VALUES (1)", &no_aliases).is_none());
}

#[test]
fn xml_alias_and_numeric_identity_columns_are_rewritten_to_types_eggdb_has() {
    let mut aliases = AliasTypes::default();
    aliases.learn("CREATE TYPE [dbo].[Phone] FROM [nvarchar](20) NOT NULL");
    aliases.learn("CREATE TYPE [Doc] FROM [xml] NULL");
    aliases.learn("CREATE TYPE [dbo].[Rows] AS TABLE ([a] [int])");
    let text = "CREATE TABLE [dbo].[T](\n\t[Id] [numeric](12, 0) IDENTITY(1,1) NOT NULL,\n\t[Big] [decimal](24, 0) NULL,\n\t[Body] [xml] NULL,\n\t[Home] [dbo].[Phone] NULL,\n\t[Work] [Phone],\n\t[Note] [dbo].[Doc] NULL,\n PRIMARY KEY ([Id])\n)";
    let rewrite = rewritten_table(text, &aliases).unwrap();
    assert_eq!(
        rewrite.text,
        "CREATE TABLE [dbo].[T](\n\t[Id] bigint IDENTITY(1,1) NOT NULL,\n\t[Big] [decimal](24, 0) NULL,\n\t[Body] nvarchar(max) NULL,\n\t[Home] [nvarchar](20) NULL,\n\t[Work] [nvarchar](20) NOT NULL,\n\t[Note] nvarchar(max) NULL,\n PRIMARY KEY ([Id])\n)"
    );
    let objects: Vec<&str> = rewrite.notes.iter().map(|note| note.object.as_str()).collect();
    assert_eq!(objects, ["dbo.T.Id", "dbo.T.Body", "dbo.T.Note"]);
    assert!(!rewrite.identity_not_kept);
    let wide = rewritten_table(
        "CREATE TABLE [w]([Id] [numeric](20, 0) IDENTITY (1, 1) NOT NULL PRIMARY KEY)",
        &aliases,
    )
    .unwrap();
    assert_eq!(
        wide.text,
        "CREATE TABLE [w]([Id] [numeric](20, 0)  NOT NULL PRIMARY KEY)"
    );
    assert!(wide.identity_not_kept);
}

#[test]
fn large_batches_split_at_semicolons_and_data_lines_outside_blocks() {
    let text = "UPDATE t SET a = 1;\nUPDATE t\nSET b = ';\nDELETE'\nIF 1 = 1 BEGIN UPDATE t SET c = CASE WHEN 1 = 1 THEN 1 END; END;\nBEGIN TRAN;\nINSERT t VALUES (1) /* ;\nDELETE */\nDELETE FROM t";
    let at = |needle: &str| text.find(needle).unwrap();
    assert_eq!(
        statement_boundaries(text),
        [
            at(";\nUPDATE t\n") + 1,
            at(";\nBEGIN TRAN") + 1,
            at("BEGIN TRAN;") + "BEGIN TRAN;".len(),
            at("DELETE FROM t"),
        ]
    );
}

#[test]
fn a_batch_over_4_mib_without_go_restores_statement_by_statement() {
    let store = MemoryStore::default();
    let mut script = String::from(
        "CREATE TABLE [dbo].[T]([Id] [int] NOT NULL PRIMARY KEY, [Note] [nvarchar](max) NULL)\nGO\nINSERT [dbo].[T] ([Id]) VALUES (1), (2), (3)\nGO\n",
    );
    let note = "x".repeat(5 << 19);
    for id in 1..=3 {
        script.push_str(&format!(
            "UPDATE [dbo].[T] SET [Note] = N'{note}' WHERE [Id] = {id}; "
        ));
    }
    script.push_str("\nGO\n");
    let report = restore(&store, script.as_bytes(), false);
    assert_eq!(report.error, None);
    assert_eq!(
        query(&store, "SELECT id, length(note) AS n FROM t ORDER BY id"),
        format!("id|n\n1|{0}\n2|{0}\n3|{0}\nSELECT 3", 5 << 19)
    );
    let one_large = format!("SELECT N'{}'\nGO\n", "y".repeat(5 << 20));
    let refused = restore(&MemoryStore::default(), one_large.as_bytes(), false);
    let error = refused.error.expect("a statement over 4 MiB that is not data");
    assert!(
        error.error.message().contains("is not an INSERT, UPDATE or DELETE"),
        "{}",
        error.error
    );
}

#[test]
fn identity_shapes_eggdb_refuses_restore_without_the_property_and_their_identity_insert_is_ignored()
{
    let store = MemoryStore::default();
    let script = b"CREATE TABLE [dbo].[Log]([Id] [int] IDENTITY(1,1) NOT NULL, [Msg] [nvarchar](20) NULL)\nGO\nCREATE TABLE [dbo].[Pair]([K] [int] NOT NULL, [Id] [int] IDENTITY(1,1) NOT NULL, CONSTRAINT [PK_Pair] PRIMARY KEY CLUSTERED ([K] ASC))\nGO\nCREATE TABLE [dbo].[Big]([Id] [numeric](10, 0) IDENTITY(1,1) NOT NULL PRIMARY KEY, [V] [int] NULL)\nGO\nCREATE TABLE [dbo].[Wide]([Id] [numeric](20, 0) IDENTITY(1,1) NOT NULL PRIMARY KEY)\nGO\nSET IDENTITY_INSERT [dbo].[Log] ON\nINSERT [dbo].[Log] ([Id], [Msg]) VALUES (7, N'a')\nSET IDENTITY_INSERT [dbo].[Log] OFF\nGO\nSET IDENTITY_INSERT [dbo].[Pair] ON\nINSERT [dbo].[Pair] ([K], [Id]) VALUES (1, 9)\nSET IDENTITY_INSERT [dbo].[Pair] OFF\nGO\nSET IDENTITY_INSERT [dbo].[Big] ON\nINSERT [dbo].[Big] ([Id], [V]) VALUES (40, 1)\nSET IDENTITY_INSERT [dbo].[Big] OFF\nGO\nSET IDENTITY_INSERT [dbo].[Wide] ON\nINSERT [dbo].[Wide] ([Id]) VALUES (12345678901234567890)\nSET IDENTITY_INSERT [dbo].[Wide] OFF\nGO\n";
    let report = restore(&store, script, false);
    assert_eq!(report.error, None, "{report:?}");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!((report.tables, report.rows, report.tables_skipped), (4, 4, 0));
    let differences: Vec<(&str, &str)> = report
        .differences
        .iter()
        .map(|difference| (difference.object.as_str(), difference.difference.as_str()))
        .collect();
    assert_eq!(differences.len(), 4, "{differences:?}");
    assert!(differences[0].0 == "dbo.Log.Id" && differences[0].1.starts_with("IDENTITY not kept"));
    assert!(differences[1].0 == "dbo.Pair.Id" && differences[1].1.starts_with("IDENTITY not kept"));
    assert_eq!(
        differences[2],
        ("dbo.Big.Id", "IDENTITY of numeric(10, 0) kept as a bigint column")
    );
    assert!(differences[3].0 == "dbo.Wide.Id" && differences[3].1.starts_with("IDENTITY not kept"));
    assert!(
        report.ignored.other.contains(&(
            "SET IDENTITY_INSERT of a table whose IDENTITY was not kept".to_owned(),
            6
        )),
        "{:?}",
        report.ignored.other
    );
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    session
        .execute("INSERT [dbo].[Big] ([V]) VALUES (2)", &mut |_| {})
        .unwrap();
    assert_eq!(session.close(), Ok(false));
    assert_eq!(
        query(&store, "SELECT id, v FROM big ORDER BY id"),
        "id|v\n40|1\n41|2\nSELECT 2"
    );
    assert_eq!(
        query(&store, "SELECT id, msg FROM log"),
        "id|msg\n7|a\nSELECT 1"
    );
}

#[test]
fn alias_types_restore_as_their_base_types_with_their_nullability() {
    let store = MemoryStore::default();
    let script = b"CREATE TYPE [dbo].[Phone] FROM [nvarchar](20) NOT NULL\nGO\nCREATE TYPE [dbo].[Flag] FROM [bit] NULL\nGO\nCREATE TABLE [dbo].[People]([Id] [int] NOT NULL PRIMARY KEY, [Home] [dbo].[Phone] NULL, [Work] [dbo].[Phone], [Active] [Flag] NULL)\nGO\nINSERT [dbo].[People] ([Id], [Home], [Work], [Active]) VALUES (1, NULL, N'555', 1)\nGO\n";
    let report = restore(&store, script, false);
    assert_eq!(report.error, None, "{report:?}");
    assert_eq!((report.tables, report.rows), (1, 1));
    assert!(
        report
            .ignored
            .other
            .contains(&("alias types".to_owned(), 2)),
        "{:?}",
        report.ignored.other
    );
    let differences: Vec<(u64, &str, &str)> = report
        .differences
        .iter()
        .map(|difference| {
            (
                difference.line,
                difference.object.as_str(),
                difference.difference.as_str(),
            )
        })
        .collect();
    assert_eq!(
        differences,
        [
            (
                1,
                "type dbo.Phone",
                "an alias type: its columns are created as [nvarchar](20) NOT NULL"
            ),
            (3, "type dbo.Flag", "an alias type: its columns are created as [bit]"),
        ]
    );
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let refused = session
        .execute(
            "INSERT [dbo].[People] ([Id], [Home]) VALUES (2, N'1')",
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(refused.vendor_code(), Some(515), "{refused}");
    assert_eq!(session.close(), Ok(false));
    assert_eq!(
        query(&store, "SELECT work, active FROM people"),
        "work|active\n555|t\nSELECT 1"
    );
}

#[test]
fn sequences_restore_and_number_new_rows_through_their_default() {
    let store = MemoryStore::default();
    let script = b"CREATE SEQUENCE [dbo].[OrderNumbers] \r\n AS [bigint]\r\n START WITH 1000\r\n INCREMENT BY 10\r\n MINVALUE -9223372036854775808\r\n MAXVALUE 9223372036854775807\r\n CACHE \r\nGO\r\nCREATE TABLE [dbo].[Orders]([Id] [bigint] NOT NULL PRIMARY KEY, [Note] [nvarchar](10) NULL)\r\nGO\r\nALTER TABLE [dbo].[Orders] ADD  CONSTRAINT [DF_Orders_Id]  DEFAULT (NEXT VALUE FOR [dbo].[OrderNumbers]) FOR [Id]\r\nGO\r\n";
    let report = restore(&store, script, false);
    assert_eq!(report.error, None, "{report:?}");
    assert_eq!((report.sequences, report.tables), (1, 1));
    assert!(report.differences.is_empty(), "{:?}", report.differences);
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let mut ids = Vec::new();
    session
        .execute(
            "INSERT [dbo].[Orders] ([Note]) VALUES (N'a'); INSERT [dbo].[Orders] ([Note]) VALUES (N'b'); SELECT NEXT VALUE FOR [dbo].[OrderNumbers] AS n",
            &mut |result| {
                for row in &result.rows {
                    ids.push(row[0].to_text().unwrap_or_default());
                }
            },
        )
        .unwrap();
    assert_eq!(ids, ["1020"]);
    assert_eq!(session.close(), Ok(false));
    assert_eq!(
        query(&store, "SELECT id, note FROM orders ORDER BY id"),
        "id|note\n1000|a\n1010|b\nSELECT 2"
    );
}

#[test]
fn descriptions_become_comments_and_char_builds_line_breaks() {
    let store = MemoryStore::default();
    let script = b"CREATE TABLE [dbo].[People]([Id] [int] NOT NULL PRIMARY KEY, [Name] [nvarchar](40) NULL)\nGO\nINSERT [dbo].[People] ([Id], [Name]) VALUES (1, N'line one' + CHAR(13) + CHAR(10) + N'line two')\nGO\nEXEC sys.sp_addextendedproperty @name=N'MS_Description', @value=N'Our people' , @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'People'\nGO\nEXEC sys.sp_addextendedproperty @name=N'MS_Description', @value=N'Full name' , @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'People', @level2type=N'COLUMN',@level2name=N'Name'\nGO\nEXEC sys.sp_addextendedproperty @name=N'Caption', @value=N'People' , @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'People'\nGO\n";
    let report = restore(&store, script, false);
    assert_eq!(report.error, None, "{report:?}");
    assert_eq!(report.comments, 2);
    assert!(
        report
            .ignored
            .other
            .contains(&("extended properties".to_owned(), 1))
    );
    assert_eq!(
        query(
            &store,
            "SELECT obj_description('people'::regclass, 'pg_class') AS t, col_description('people'::regclass, 2) AS c"
        ),
        "t|c\nOur people|Full name\nSELECT 1"
    );
    assert_eq!(
        query(&store, "SELECT name = E'line one\\r\\nline two' AS kept FROM people"),
        "kept\nt\nSELECT 1"
    );
}

#[test]
fn transact_sql_scripts_split_into_batches_at_go_without_running_sqlcmd_commands() {
    let batches = sqlserver_batches("SELECT 1\nGO\n\nSELECT 'a\nGO\nb'\ngo 3\nSELECT 2").unwrap();
    let shown: Vec<(&str, u64, u64)> = batches
        .iter()
        .map(|batch| (batch.text.as_str(), batch.line, batch.repeat))
        .collect();
    assert_eq!(
        shown,
        [
            ("SELECT 1\n", 1, 1),
            ("SELECT 'a\nGO\nb'\n", 4, 3),
            ("SELECT 2", 8, 1),
        ]
    );
    let big = format!("SELECT 1\n{}GO\n", "INSERT t VALUES (1)\n".repeat(80_000));
    assert_eq!(sqlserver_batches(&big).unwrap().len(), 1);
    let error = sqlserver_batches("SELECT 1\nGO\n:setvar a 1\nSELECT '$(a)'\n").unwrap_err();
    assert!(error.message().contains(":setvar is a sqlcmd command"), "{error}");
    assert!(sqlserver_batches("").unwrap().is_empty());
}

#[test]
fn an_ssms_shaped_script_restores_with_its_report() {
    let store = MemoryStore::default();
    let script = "USE [master]\r\nGO\r\nCREATE DATABASE [Shop]\r\nGO\r\nUSE [Shop]\r\nGO\r\nCREATE SCHEMA [sales]\r\nGO\r\nSET ANSI_NULLS ON\r\nGO\r\nCREATE TABLE [sales].[Orders](\r\n\t[Id] [int] IDENTITY(10,5) NOT NULL,\r\n\t[At] [datetimeoffset](7) NOT NULL,\r\n\t[Total] [money] NOT NULL,\r\n\t[Twice]  AS ([Total]*(2)),\r\n CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED \r\n(\r\n\t[Id] ASC\r\n)WITH (PAD_INDEX = OFF) ON [PRIMARY]\r\n) ON [PRIMARY]\r\nGO\r\nCREATE TABLE [dbo].[Docs]([Id] [int] IDENTITY(1,1) NOT NULL PRIMARY KEY, [Body] [xml] NULL)\r\nGO\r\nSET IDENTITY_INSERT [sales].[Orders] ON \r\n\r\nINSERT [sales].[Orders] ([Id], [At], [Total]) VALUES (0, CAST(N'2024-01-01T10:00:00+02:00' AS DateTimeOffset), 1.5000)\r\nINSERT [sales].[Orders] ([Id], [At], [Total]) VALUES (10, CAST(N'2024-01-02T10:00:00+00:00' AS DateTimeOffset), 2.0000)\r\nSET IDENTITY_INSERT [sales].[Orders] OFF\r\nGO\r\nSET IDENTITY_INSERT [dbo].[Docs] ON\r\nINSERT [dbo].[Docs] ([Id], [Body]) VALUES (1, N'<a/>')\r\nSET IDENTITY_INSERT [dbo].[Docs] OFF\r\nGO\r\nCREATE PROCEDURE [dbo].[P] AS SELECT * INTO #t FROM [sales].[Orders]\r\nGO\r\nGRANT EXECUTE ON [dbo].[P] TO [public]\r\nGO\r\n";
    let report = restore(&store, &utf16(script), false);
    assert_eq!(report.error, None);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!((report.schemas, report.tables, report.rows), (1, 2, 3));
    assert_eq!((report.tables_skipped, report.rows_skipped), (0, 0));
    let skipped: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|object| (object.kind.as_str(), object.name.as_str()))
        .collect();
    assert_eq!(skipped, [("procedure", "dbo.P")]);
    let differences: Vec<(u64, &str)> = report
        .differences
        .iter()
        .map(|difference| (difference.line, difference.object.as_str()))
        .collect();
    assert_eq!(
        differences,
        [
            (11, "sales.Orders.Twice"),
            (11, "sales.Orders.At"),
            (22, "dbo.Docs.Body")
        ]
    );
    assert_eq!(
        query(&store, "SELECT id, body FROM docs"),
        "id|body\n1|<a/>\nSELECT 1"
    );
    assert_eq!(report.ignored.grant, 1);
    assert_eq!(
        query(&store, "SELECT id, total FROM sales.orders ORDER BY id"),
        "id|total\n0|1.5000\n10|2.0000\nSELECT 2"
    );
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    session
        .execute(
            "INSERT [sales].[Orders] ([At], [Total]) VALUES (CAST(N'2024-01-03T10:00:00+00:00' AS DateTimeOffset), 3.0000)",
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(session.close(), Ok(false));
    assert_eq!(
        query(&store, "SELECT id FROM sales.orders ORDER BY id"),
        "id\n0\n10\n15\nSELECT 3"
    );
}

#[test]
fn a_constraint_existing_rows_violate_is_reported_and_its_check_is_a_consequence() {
    let store = MemoryStore::default();
    let script = b"CREATE TABLE [dbo].[A]([Id] [int] NOT NULL PRIMARY KEY)\nGO\nCREATE TABLE [dbo].[B]([Id] [int] NOT NULL PRIMARY KEY, [A] [int] NOT NULL)\nGO\nINSERT [dbo].[B] ([Id], [A]) VALUES (1, 7)\nGO\nALTER TABLE [dbo].[B]  WITH NOCHECK ADD  CONSTRAINT [FK_B_A] FOREIGN KEY([A])\nREFERENCES [dbo].[A] ([Id])\nGO\nALTER TABLE [dbo].[B] CHECK CONSTRAINT [FK_B_A]\nGO\nALTER TABLE [dbo].[B] ADD CONSTRAINT [DF_B_A] DEFAULT (abs(-1) + 1) FOR [A]\nGO\n";
    let report = restore(&store, script, false);
    assert_eq!(report.error, None, "{report:?}");
    let differences: Vec<(u64, &str, &str)> = report
        .differences
        .iter()
        .map(|difference| {
            (
                difference.line,
                difference.object.as_str(),
                difference.difference.as_str(),
            )
        })
        .collect();
    assert_eq!(differences.len(), 1, "{differences:?}");
    assert_eq!(differences[0].0, 7);
    assert_eq!(differences[0].1, "FK_B_A");
    assert!(
        differences[0]
            .2
            .starts_with("not created: existing rows violate it")
    );
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    session
        .execute("INSERT [dbo].[B] ([Id]) VALUES (2)", &mut |_| {})
        .unwrap();
    assert_eq!(session.close(), Ok(false));
    assert_eq!(
        query(&store, "SELECT a FROM b WHERE id = 2"),
        "a\n2\nSELECT 1"
    );
    assert_eq!(report.consequences.len(), 1);
    assert_eq!(report.consequences[0].line, 10);
    assert_eq!(report.consequences[0].uses, "constraint FK_B_A");
}

#[test]
fn a_syntax_error_skips_its_batch_with_keep_going_and_data_of_skipped_tables_is_counted() {
    let store = MemoryStore::default();
    let script = b"CREATE TABLE [dbo].[T]([Id] [int] NOT NULL PRIMARY KEY)\nGO\nINSERT [dbo].[T] ([Id]) VALUES (1)\nINSERT [dbo].[T] ([Id] VALUES (2)\nGO\nINSERT [dbo].[T] ([Id]) VALUES (3)\nGO\nCREATE TABLE [dbo].[V]([Id] [int] NOT NULL PRIMARY KEY, [x] [sql_variant] NULL)\nGO\nINSERT [dbo].[V] ([Id], [x]) VALUES (1, 1), (2, 2)\nGO\n";
    let report = restore(&store, script, true);
    assert_eq!(report.error, None);
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert_eq!(report.errors[0].line, 4);
    assert_eq!((report.rows, report.rows_skipped), (1, 2));
    assert_eq!(
        query(&store, "SELECT id FROM t ORDER BY id"),
        "id\n3\nSELECT 1"
    );
    let stopped = restore(&MemoryStore::default(), script, false);
    assert_eq!(stopped.error.map(|error| error.line), Some(4));
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

struct Trickle<'b> {
    bytes: &'b [u8],
    at: usize,
    step: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.step.min(buf.len()).min(self.bytes.len() - self.at);
        buf[..count].copy_from_slice(&self.bytes[self.at..self.at + count]);
        self.at += count;
        Ok(count)
    }
}

#[test]
fn hostile_scripts_end_in_items_or_an_error_and_never_panic() {
    const FRAGMENTS: [&str; 39] = [
        "GO\n",
        "go 2\n",
        "GO /* x\n",
        "*/",
        "'",
        "''",
        "[",
        "]]",
        "]",
        "\"",
        "--",
        "/*",
        "\n",
        "\r",
        "\r\n",
        ":setvar a \"b\"\n",
        ":on error exit\n",
        "$(a)",
        "$(",
        ":r x\n",
        "!! x\n",
        "#t",
        "{",
        "$action",
        "CREATE PROCEDURE p AS ",
        "INSERT [t] VALUES (",
        "SET IDENTITY_INSERT [t] ON\n",
        "CREATE TABLE [t]([a] AS (1), [b] [timestamp])",
        "\u{1F389}",
        "\0",
        "IF 1=1 ",
        ";",
        "CREATE TYPE [p] FROM [xml] NOT NULL\n",
        "CREATE TABLE [t]([a] [p], [b] [numeric](30, 0) IDENTITY, [c] [xml](CONTENT x))",
        "UPDATE [t] SET a = 1",
        "BEGIN ",
        " END",
        "NEXT VALUE FOR [s]",
        "SET XACT_ABORT ON\n",
    ];
    let mut random = Random(0x5eed_4040);
    for case in 0..3000 {
        let mut text = String::new();
        for _ in 0..random.below(40) {
            text.push_str(FRAGMENTS[random.below(FRAGMENTS.len())]);
        }
        let mut bytes = if case % 3 == 0 {
            utf16(&text)
        } else {
            text.clone().into_bytes()
        };
        if case % 7 == 0 && !bytes.is_empty() {
            let at = random.below(bytes.len());
            bytes.insert(at, (random.next() & 0xff) as u8);
        }
        if case % 11 == 0 {
            bytes.extend_from_slice(b"\xFF\xFE");
        }
        let trickle = Trickle {
            bytes: &bytes,
            at: 0,
            step: 1 + random.below(7),
        };
        let mut input = BufReader::with_capacity(3, trickle);
        let input: &mut dyn BufRead = &mut input;
        let mut script = SqlServerScript::new(input);
        for _ in 0..10_000 {
            match script.next_item() {
                Ok(ScriptItem::End) | Err(_) => break,
                Ok(_) => {}
            }
        }
        if case % 10 == 0 {
            let report = restore(&MemoryStore::default(), &bytes, true);
            assert!(report.statements <= 10_000);
        }
    }
}

#[test]
fn the_ssms_sample_runs_batch_by_batch_as_sqlcmd_sends_it() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let sample = include_bytes!("../../../eggdb/tests/data/ssms-sample.sql");
    let mut refused = Vec::new();
    for item in batches_of(sample).unwrap() {
        let BatchItem::Batch(batch) = item else {
            continue;
        };
        let outcome = session.execute_batch(&batch.text, &mut |step| {
            if let Err(error) = step.outcome {
                refused.push((batch.line, error.vendor_code()));
            }
            true
        });
        if let Err(error) = outcome {
            refused.push((batch.line, error.vendor_code()));
        }
    }
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].0, 50);
    let mut rows = Vec::new();
    session
        .execute(
            "SELECT COUNT(*) FROM dbo.Loaves WHERE Kind = N'rye'",
            &mut |result| rows.push(render(&result)),
        )
        .unwrap();
    assert_eq!(session.close(), Ok(false));
    assert!(rows[0].contains("\n2\n"), "{rows:?}");
}

#[test]
fn an_index_name_another_table_already_uses_is_a_known_difference() {
    let store = MemoryStore::default();
    let script = b"CREATE TABLE [dbo].[A]([Id] [int] NOT NULL PRIMARY KEY, [At] [datetime] NULL)\nGO\nCREATE TABLE [dbo].[B]([Id] [int] NOT NULL PRIMARY KEY, [At] [datetime] NULL)\nGO\nCREATE NONCLUSTERED INDEX [IX_At] ON [dbo].[A] ([At] ASC)\nGO\nCREATE NONCLUSTERED INDEX [IX_At] ON [dbo].[B] ([At] ASC)\nGO\n";
    let report = restore(&store, script, false);
    assert_eq!(report.error, None, "{report:?}");
    assert_eq!(report.indexes, 1);
    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].line, 7);
    assert_eq!(report.differences[0].object, "index IX_At");
}
