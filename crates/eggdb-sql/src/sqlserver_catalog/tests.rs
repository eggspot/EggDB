use super::procedures::like;
use super::split_multipart_name;
use crate::memory_store::MemoryStore;
use crate::{BatchStep, Dialect, QueryResult, Session, SqlError, Value};

const FIXTURE: [&str; 7] = [
    "CREATE SCHEMA [sales]",
    "CREATE TABLE [dbo].[Customers] ([Id] INT IDENTITY (1, 1) NOT NULL, [Name] NVARCHAR (100) NOT NULL, [Email] NVARCHAR (200) NULL, [Active] BIT NOT NULL, CONSTRAINT [PK_Customers] PRIMARY KEY CLUSTERED ([Id] ASC))",
    "CREATE TABLE [sales].[Orders] ([Id] BIGINT IDENTITY (1, 1) NOT NULL, [CustomerId] INT NOT NULL, [Total] DECIMAL (10, 2) NOT NULL, CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Id] ASC))",
    "CREATE NONCLUSTERED INDEX [IX_Orders_Customer] ON [sales].[Orders]([CustomerId] ASC)",
    "ALTER TABLE [dbo].[Customers] ADD CONSTRAINT [DF_Customers_Active] DEFAULT ((1)) FOR [Active]",
    "ALTER TABLE [sales].[Orders] WITH NOCHECK ADD CONSTRAINT [FK_Orders_Customers] FOREIGN KEY ([CustomerId]) REFERENCES [dbo].[Customers] ([Id])",
    "ALTER TABLE [sales].[Orders] WITH NOCHECK ADD CONSTRAINT [CK_Orders_Total] CHECK ([Total] >= 0)",
];

fn shop(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    session.identify("eggdb", "shop");
    for statement in FIXTURE {
        batch(&mut session, statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    session
}

fn batch(session: &mut Session<'_, MemoryStore>, text: &str) -> Result<Vec<BatchStep>, SqlError> {
    let mut steps = Vec::new();
    session.execute_batch(text, &mut |step| {
        steps.push(step);
        true
    })?;
    Ok(steps)
}

fn results(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<QueryResult> {
    batch(session, text)
        .unwrap_or_else(|error| panic!("{text}: {error}"))
        .into_iter()
        .map(|step| {
            step.outcome
                .unwrap_or_else(|error| panic!("{text}: {error}"))
        })
        .collect()
}

fn rows(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<Vec<Value>> {
    results(session, text)
        .into_iter()
        .rev()
        .find(|result| result.columns.is_some())
        .map(|result| result.rows)
        .unwrap_or_default()
}

fn error_code(session: &mut Session<'_, MemoryStore>, text: &str) -> Option<u16> {
    match batch(session, text) {
        Err(error) => error.vendor_code(),
        Ok(steps) => steps
            .into_iter()
            .find_map(|step| step.outcome.err())
            .and_then(|error| error.vendor_code()),
    }
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

#[test]
fn like_patterns_take_percent_underscore_and_bracket_sets_without_regard_to_case() {
    assert!(like("%", "anything"));
    assert!(like("cust%", "Customers"));
    assert!(like("c_stomers", "customers"));
    assert!(like("[%]x", "%x"));
    assert!(!like("[%]x", "ax"));
    assert!(like("[a-c]at", "bat"));
    assert!(!like("[^a-c]at", "bat"));
    assert!(like("%[_]%", "a_b"));
    assert!(!like("%[_]%", "ab"));
    assert!(!like("orders", "order"));
    assert!(like("", ""));
}

#[test]
fn multipart_names_split_on_dots_outside_brackets_and_quotes() {
    assert_eq!(
        split_multipart_name("[Shop].[dbo].[My.Table]"),
        Some(vec![
            "shop".to_owned(),
            "dbo".to_owned(),
            "my.table".to_owned()
        ])
    );
    assert_eq!(
        split_multipart_name("master..sysdatabases"),
        Some(vec![
            "master".to_owned(),
            String::new(),
            "sysdatabases".to_owned()
        ])
    );
    assert_eq!(split_multipart_name("a.b.c.d.e"), None);
    assert_eq!(split_multipart_name("[unclosed"), None);
    assert_eq!(split_multipart_name("dbo."), None);
}

#[test]
fn sys_views_list_the_tables_columns_and_indexes_of_every_schema() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT s.name, t.name, t.type FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id ORDER BY s.name, t.name"
        ),
        vec![
            vec![
                text("dbo"),
                text("customers"),
                Value::Bpchar("U ".to_owned())
            ],
            vec![
                text("sales"),
                text("orders"),
                Value::Bpchar("U ".to_owned())
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT c.name, c.system_type_id, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity FROM sys.columns c WHERE c.object_id = OBJECT_ID('dbo.Customers') ORDER BY c.column_id"
        ),
        vec![
            vec![
                text("id"),
                Value::SmallInt(56),
                Value::SmallInt(4),
                Value::SmallInt(10),
                Value::SmallInt(0),
                Value::Boolean(false),
                Value::Boolean(true),
            ],
            vec![
                text("name"),
                Value::SmallInt(231),
                Value::SmallInt(200),
                Value::SmallInt(0),
                Value::SmallInt(0),
                Value::Boolean(false),
                Value::Boolean(false),
            ],
            vec![
                text("email"),
                Value::SmallInt(231),
                Value::SmallInt(400),
                Value::SmallInt(0),
                Value::SmallInt(0),
                Value::Boolean(true),
                Value::Boolean(false),
            ],
            vec![
                text("active"),
                Value::SmallInt(104),
                Value::SmallInt(1),
                Value::SmallInt(1),
                Value::SmallInt(0),
                Value::Boolean(false),
                Value::Boolean(false),
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT i.name, i.index_id, i.type_desc, i.is_primary_key FROM sys.indexes i WHERE i.object_id = OBJECT_ID('sales.orders') ORDER BY i.index_id"
        ),
        vec![
            vec![
                text("pk_orders"),
                Value::Integer(1),
                text("CLUSTERED"),
                Value::Boolean(true),
            ],
            vec![
                text("ix_orders_customer"),
                Value::Integer(2),
                text("NONCLUSTERED"),
                Value::Boolean(false),
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT OBJECT_NAME(fk.parent_object_id), OBJECT_NAME(fk.referenced_object_id), COL_NAME(fkc.parent_object_id, fkc.parent_column_id), COL_NAME(fkc.referenced_object_id, fkc.referenced_column_id), fk.delete_referential_action_desc FROM sys.foreign_keys fk JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id"
        ),
        vec![vec![
            text("orders"),
            text("customers"),
            text("customerid"),
            text("id"),
            text("NO_ACTION"),
        ]]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT name, definition FROM sys.default_constraints"
        ),
        vec![vec![text("df__customers__active"), text("((1))")]]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT o.name, o.type_desc FROM sys.objects o WHERE o.parent_object_id = OBJECT_ID('sales.orders') ORDER BY o.name"
        ),
        vec![
            vec![text("ck_orders_total"), text("CHECK_CONSTRAINT")],
            vec![text("fk_orders_customers"), text("FOREIGN_KEY_CONSTRAINT")],
            vec![text("pk_orders"), text("PRIMARY_KEY_CONSTRAINT")],
        ]
    );
    assert_eq!(
        rows(&mut session, "SELECT name, database_id FROM sys.databases"),
        vec![vec![text("shop"), Value::Integer(5)]]
    );
}

#[test]
fn information_schema_answers_with_sql_server_names_and_types() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT TABLE_CATALOG, TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.TABLES ORDER BY TABLE_SCHEMA, TABLE_NAME"
        ),
        vec![
            vec![
                text("shop"),
                text("dbo"),
                text("customers"),
                text("BASE TABLE")
            ],
            vec![
                text("shop"),
                text("sales"),
                text("orders"),
                text("BASE TABLE")
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT COLUMN_NAME, DATA_TYPE, NUMERIC_PRECISION, NUMERIC_SCALE, IS_NULLABLE FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = 'sales' AND TABLE_NAME = 'orders' ORDER BY ORDINAL_POSITION"
        ),
        vec![
            vec![
                text("id"),
                text("bigint"),
                Value::SmallInt(19),
                Value::Integer(0),
                text("NO"),
            ],
            vec![
                text("customerid"),
                text("int"),
                Value::SmallInt(10),
                Value::Integer(0),
                text("NO"),
            ],
            vec![
                text("total"),
                text("decimal"),
                Value::SmallInt(10),
                Value::Integer(2),
                text("NO"),
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT COLUMN_NAME, CHARACTER_MAXIMUM_LENGTH, COLUMN_DEFAULT FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = 'customers' ORDER BY ORDINAL_POSITION"
        ),
        vec![
            vec![text("id"), Value::Null, Value::Null],
            vec![text("name"), Value::Integer(100), Value::Null],
            vec![text("email"), Value::Integer(200), Value::Null],
            vec![text("active"), Value::Null, text("((1))")],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT CONSTRAINT_NAME, CONSTRAINT_TYPE FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS WHERE TABLE_NAME = 'orders' ORDER BY CONSTRAINT_NAME"
        ),
        vec![
            vec![text("ck_orders_total"), text("CHECK")],
            vec![text("fk_orders_customers"), text("FOREIGN KEY")],
            vec![text("pk_orders"), text("PRIMARY KEY")],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT CONSTRAINT_NAME, UNIQUE_CONSTRAINT_SCHEMA, UNIQUE_CONSTRAINT_NAME, UPDATE_RULE, DELETE_RULE FROM INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS"
        ),
        vec![vec![
            text("fk_orders_customers"),
            text("dbo"),
            text("pk_customers"),
            text("NO ACTION"),
            text("NO ACTION"),
        ]]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT COLUMN_NAME, ORDINAL_POSITION FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE WHERE CONSTRAINT_NAME = 'fk_orders_customers'"
        ),
        vec![vec![text("customerid"), Value::Integer(1)]]
    );
}

#[test]
fn metadata_functions_resolve_names_ids_and_properties() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT OBJECT_NAME(OBJECT_ID('[sales].[Orders]')), SCHEMA_NAME(), DB_NAME(), OBJECTPROPERTY(OBJECT_ID('sales.orders'), 'TableHasForeignKey'), COLUMNPROPERTY(OBJECT_ID('dbo.customers'), 'Email', 'AllowsNull'), OBJECT_ID('dbo.missing'), TYPE_NAME(56), SCHEMA_ID('sales'), OBJECT_ID('shop.dbo.customers', 'U') - OBJECT_ID('customers'), OBJECT_ID('other.dbo.customers')"
        ),
        vec![vec![
            text("orders"),
            text("dbo"),
            text("shop"),
            Value::Integer(1),
            Value::Integer(1),
            Value::Null,
            text("int"),
            Value::Integer(5),
            Value::Integer(0),
            Value::Null,
        ]]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT SERVERPROPERTY('ProductVersion'), SERVERPROPERTY('EngineEdition'), DB_ID(), DB_NAME(5), SYSTEM_USER, CURRENT_USER, @@MICROSOFTVERSION / 16777216, COL_LENGTH('dbo.customers', 'name')"
        ),
        vec![vec![
            text("16.0.1000.0"),
            Value::Integer(3),
            Value::SmallInt(5),
            text("shop"),
            text("eggdb"),
            text("dbo"),
            Value::Integer(16),
            Value::SmallInt(200),
        ]]
    );
    assert_eq!(
        rows(
            &mut session,
            "IF OBJECT_ID('dbo.customers') IS NOT NULL SELECT 1 ELSE SELECT 0"
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn catalog_procedures_run_by_exec_with_arguments_by_position_or_name() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(&mut session, "EXEC sp_tables @table_owner = N'sales'"),
        vec![vec![
            text("shop"),
            text("sales"),
            text("orders"),
            text("TABLE"),
            Value::Null,
        ]]
    );
    assert_eq!(
        rows(
            &mut session,
            "EXEC sp_tables N'%', N'dbo', NULL, N'''TABLE'',''VIEW'''"
        )
        .len(),
        1
    );
    assert_eq!(
        rows(&mut session, "EXECUTE sp_tables 'cust%', DEFAULT, 'shop'")
            .iter()
            .map(|row| row[2].clone())
            .collect::<Vec<_>>(),
        vec![text("customers")]
    );
    assert_eq!(
        rows(&mut session, "EXEC sp_pkeys 'orders', 'sales'"),
        vec![vec![
            text("shop"),
            text("sales"),
            text("orders"),
            text("id"),
            Value::SmallInt(1),
            text("pk_orders"),
        ]]
    );
    let statistics = rows(
        &mut session,
        "EXEC sp_statistics_100 'orders', 'sales', 'shop', '%', 'N', 'Q'",
    );
    assert_eq!(
        statistics
            .iter()
            .map(|row| (row[5].clone(), row[6].clone(), row[8].clone()))
            .collect::<Vec<_>>(),
        vec![
            (Value::Null, Value::SmallInt(0), Value::Null),
            (text("pk_orders"), Value::SmallInt(1), text("id")),
            (
                text("ix_orders_customer"),
                Value::SmallInt(3),
                text("customerid")
            ),
        ]
    );
    let types = rows(&mut session, "sp_datatype_info_100 @ODBCVer=3");
    let pairs: Vec<(Value, Value)> = types
        .iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    assert!(pairs.contains(&(text("int"), Value::SmallInt(4))));
    assert!(pairs.contains(&(text("datetime2"), Value::SmallInt(93))));
    assert!(pairs.contains(&(text("nvarchar"), Value::SmallInt(-9))));
    assert_eq!(
        rows(
            &mut session,
            "EXEC sys.sp_columns_managed NULL, 'sales', 'orders', NULL, 0"
        )
        .iter()
        .map(|row| (row[3].clone(), row[7].clone()))
        .collect::<Vec<_>>(),
        vec![
            (text("id"), text("bigint")),
            (text("customerid"), text("int")),
            (text("total"), text("decimal")),
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "EXEC sys.sp_indexes_managed NULL, 'sales', 'orders', NULL"
        )
        .iter()
        .map(|row| (row[6].clone(), row[7].clone()))
        .collect::<Vec<_>>(),
        vec![
            (text("ix_orders_customer"), text("NONCLUSTERED")),
            (text("pk_orders"), text("CLUSTERED")),
        ]
    );
    assert_eq!(error_code(&mut session, "EXEC sp_nothing"), Some(2812));
    assert_eq!(
        error_code(&mut session, "EXEC sp_executesql N'SELECT 1'"),
        Some(40517)
    );
    assert_eq!(
        error_code(&mut session, "EXEC sp_tables @table_owner = 'x', 'y'"),
        Some(119)
    );
    assert_eq!(error_code(&mut session, "EXEC sp_pkeys"), Some(201));
    assert_eq!(
        error_code(&mut session, "EXEC sp_tables @nothing = 1"),
        Some(8145)
    );
}

#[test]
fn sp_help_returns_one_result_set_per_part_of_the_table() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    let help = results(&mut session, "EXEC sp_help 'sales.orders'");
    assert!(help.len() >= 6, "{help:?}");
    assert_eq!(
        help[0].rows[0][..3],
        [text("orders"), text("sales"), text("user table")]
    );
    assert_eq!(
        help[1]
            .rows
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        vec![text("id"), text("customerid"), text("total")]
    );
    assert_eq!(help[2].rows[0][0], text("id"));
    let indexes = rows(&mut session, "EXEC sp_helpindex 'sales.orders'");
    assert_eq!(
        indexes,
        vec![
            vec![
                text("ix_orders_customer"),
                text("nonclustered located on PRIMARY"),
                text("customerid"),
            ],
            vec![
                text("pk_orders"),
                text("clustered, unique, primary key located on PRIMARY"),
                text("id"),
            ],
        ]
    );
    assert_eq!(
        error_code(&mut session, "EXEC sp_help 'nothing'"),
        Some(15009)
    );
}

#[test]
fn mssql_jdbc_get_columns_runs_through_a_table_variable_and_insert_exec() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    let text_of_batch = "DECLARE @mssqljdbc_temp_sp_columns_result TABLE(TABLE_QUALIFIER SYSNAME, TABLE_OWNER SYSNAME,TABLE_NAME SYSNAME, COLUMN_NAME SYSNAME, DATA_TYPE SMALLINT, TYPE_NAME SYSNAME, PRECISION INT,LENGTH INT, SCALE SMALLINT, RADIX SMALLINT, NULLABLE SMALLINT, REMARKS VARCHAR(254), COLUMN_DEF NVARCHAR(4000),SQL_DATA_TYPE SMALLINT, SQL_DATETIME_SUB SMALLINT, CHAR_OCTET_LENGTH INT, ORDINAL_POSITION INT,IS_NULLABLE VARCHAR(254), SS_IS_SPARSE SMALLINT, SS_IS_COLUMN_SET SMALLINT, SS_IS_COMPUTED SMALLINT,SS_IS_IDENTITY SMALLINT, SS_UDT_CATALOG_NAME NVARCHAR(128), SS_UDT_SCHEMA_NAME NVARCHAR(128),SS_UDT_ASSEMBLY_TYPE_NAME NVARCHAR(max), SS_XML_SCHEMACOLLECTION_CATALOG_NAME NVARCHAR(128),SS_XML_SCHEMACOLLECTION_SCHEMA_NAME NVARCHAR(128), SS_XML_SCHEMACOLLECTION_NAME NVARCHAR(128),SS_DATA_TYPE TINYINT);INSERT INTO @mssqljdbc_temp_sp_columns_result EXEC sp_columns_100 N'customers',N'dbo',N'shop',N'%',2,3;SELECT TABLE_QUALIFIER AS TABLE_CAT, TABLE_OWNER AS TABLE_SCHEM, TABLE_NAME, COLUMN_NAME, CAST(DATA_TYPE AS INT) AS DATA_TYPE,TYPE_NAME, PRECISION AS COLUMN_SIZE, LENGTH AS BUFFER_LENGTH, CAST(SCALE AS INT) AS DECIMAL_DIGITS, CAST(RADIX AS INT) AS NUM_PREC_RADIX,CAST(NULLABLE AS INT) AS NULLABLE, CAST(REMARKS AS VARCHAR) AS REMARKS, COLUMN_DEF, CAST(SQL_DATA_TYPE AS INT) AS SQL_DATA_TYPE, CAST(SQL_DATETIME_SUB AS INT) AS SQL_DATETIME_SUB, CHAR_OCTET_LENGTH, ORDINAL_POSITION, IS_NULLABLE,CAST(NULL AS VARCHAR) AS SCOPE_CATALOG, CAST(NULL AS VARCHAR) AS SCOPE_SCHEMA, CAST(NULL AS VARCHAR) AS SCOPE_TABLE, CAST(SS_DATA_TYPE AS SMALLINT) AS SOURCE_DATA_TYPE, CASE SS_IS_IDENTITY WHEN 0 THEN 'NO' WHEN 1 THEN 'YES' WHEN '' THEN '' END AS IS_AUTOINCREMENT,CASE SS_IS_COMPUTED WHEN 0 THEN 'NO' WHEN 1 THEN 'YES' WHEN '' THEN '' END AS IS_GENERATEDCOLUMN, SS_IS_SPARSE, SS_IS_COLUMN_SET, SS_UDT_CATALOG_NAME, SS_UDT_SCHEMA_NAME, SS_UDT_ASSEMBLY_TYPE_NAME,SS_XML_SCHEMACOLLECTION_CATALOG_NAME, SS_XML_SCHEMACOLLECTION_SCHEMA_NAME, SS_XML_SCHEMACOLLECTION_NAME FROM @mssqljdbc_temp_sp_columns_result ORDER BY TABLE_CAT, TABLE_SCHEM, TABLE_NAME, ORDINAL_POSITION;";
    let found = rows(&mut session, text_of_batch);
    assert_eq!(
        found
            .iter()
            .map(|row| (
                row[3].clone(),
                row[4].clone(),
                row[5].clone(),
                row[6].clone(),
                row[12].clone(),
                row[17].clone(),
                row[22].clone()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                text("id"),
                Value::Integer(4),
                text("int identity"),
                Value::Integer(10),
                Value::Null,
                text("NO"),
                text("YES")
            ),
            (
                text("name"),
                Value::Integer(-9),
                text("nvarchar"),
                Value::Integer(100),
                Value::Null,
                text("NO"),
                text("NO")
            ),
            (
                text("email"),
                Value::Integer(-9),
                text("nvarchar"),
                Value::Integer(200),
                Value::Null,
                text("YES"),
                text("NO")
            ),
            (
                text("active"),
                Value::Integer(-7),
                text("bit"),
                Value::Integer(1),
                text("((1))"),
                text("NO"),
                text("NO")
            ),
        ]
    );
    assert_eq!(
        error_code(
            &mut session,
            "DECLARE @t TABLE (a INT); INSERT INTO @t EXEC sp_pkeys 'orders'"
        ),
        Some(213)
    );
    assert_eq!(
        error_code(&mut session, "SELECT * FROM @missing"),
        Some(1087)
    );
}

#[test]
fn mssql_jdbc_get_imported_keys_joins_sp_fkeys_with_sys_foreign_keys() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    let text_of_batch = "DECLARE @jdbc_temp_fkeys_result table (PKTABLE_QUALIFIER sysname, PKTABLE_OWNER sysname, PKTABLE_NAME sysname, PKCOLUMN_NAME sysname, FKTABLE_QUALIFIER sysname, FKTABLE_OWNER sysname, FKTABLE_NAME sysname, FKCOLUMN_NAME sysname, KEY_SEQ smallint, UPDATE_RULE smallint, DELETE_RULE smallint, FK_NAME sysname, PK_NAME sysname, DEFERRABILITY smallint);INSERT INTO @jdbc_temp_fkeys_result EXEC sp_fkeys NULL,NULL,NULL,N'orders',N'sales',N'shop';SELECT  t.PKTABLE_QUALIFIER AS PKTABLE_CAT, t.PKTABLE_OWNER AS PKTABLE_SCHEM, t.PKTABLE_NAME, t.PKCOLUMN_NAME, t.FKTABLE_QUALIFIER AS FKTABLE_CAT, t.FKTABLE_OWNER AS FKTABLE_SCHEM, t.FKTABLE_NAME, t.FKCOLUMN_NAME, t.KEY_SEQ, CASE s.update_referential_action WHEN 1 THEN 0 WHEN 0 THEN 3 WHEN 2 THEN 2 WHEN 3 THEN 4 END as UPDATE_RULE, CASE s.delete_referential_action WHEN 1 THEN 0 WHEN 0 THEN 3 WHEN 2 THEN 2 WHEN 3 THEN 4 END as DELETE_RULE, t.FK_NAME, t.PK_NAME, t.DEFERRABILITY FROM @jdbc_temp_fkeys_result t LEFT JOIN sys.foreign_keys s ON t.FK_NAME = s.name COLLATE database_default AND schema_id(t.FKTABLE_OWNER) = s.schema_id ORDER BY PKTABLE_CAT, PKTABLE_SCHEM, PKTABLE_NAME, KEY_SEQ";
    assert_eq!(
        rows(&mut session, text_of_batch),
        vec![vec![
            text("shop"),
            text("dbo"),
            text("customers"),
            text("id"),
            text("shop"),
            text("sales"),
            text("orders"),
            text("customerid"),
            Value::SmallInt(1),
            Value::Integer(3),
            Value::Integer(3),
            text("fk_orders_customers"),
            text("pk_customers"),
            Value::SmallInt(7),
        ]]
    );
}

#[test]
fn catalog_relations_are_read_only_and_reserved_to_their_schemas() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert!(error_code(&mut session, "DELETE FROM sys.objects").is_some());
    assert_eq!(error_code(&mut session, "SELECT * FROM objects"), Some(208));
}
