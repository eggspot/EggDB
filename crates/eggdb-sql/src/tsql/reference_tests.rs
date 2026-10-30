use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::{BatchStep, QueryResult, Session};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    Session::with_dialect(store, Dialect::SqlServer)
}

fn batch(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_batch(text, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    steps
}

fn errors(steps: &[BatchStep]) -> Vec<Option<u32>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().err())
        .map(crate::SqlError::sqlserver_number)
        .collect()
}

fn succeeds(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<BatchStep> {
    let steps = batch(session, text);
    assert_eq!(errors(&steps), Vec::<Option<u32>>::new(), "{text}");
    steps
}

fn shown(result: &QueryResult) -> Vec<String> {
    let Some(columns) = &result.columns else {
        return Vec::new();
    };
    let mut lines = vec![
        columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>()
            .join("|"),
    ];
    for row in &result.rows {
        let cells: Vec<String> = row
            .iter()
            .zip(columns)
            .map(|(value, column)| {
                Dialect::SqlServer
                    .value_text_typed(value, column.sql_type)
                    .unwrap_or_else(|| "NULL".into())
            })
            .collect();
        lines.push(cells.join("|"));
    }
    lines
}

fn result_sets(steps: &[BatchStep]) -> Vec<Vec<String>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .filter(|result| result.columns.is_some())
        .map(shown)
        .collect()
}

fn printed(steps: &[BatchStep]) -> Vec<String> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .flat_map(|result| result.info_messages.iter().map(|message| message.text.clone()))
        .collect()
}

fn with_staff(session: &mut Session<'_, MemoryStore>) {
    succeeds(
        session,
        "CREATE TABLE Staff (Id int PRIMARY KEY, FirstName nvarchar(50) NOT NULL, \
         LastName nvarchar(50) NOT NULL, Department nvarchar(50) NOT NULL, SalesYTD decimal(12,2) NULL)",
    );
    succeeds(
        session,
        "INSERT INTO Staff VALUES (1, N'Pilar', N'Ackerman', N'Shipping', NULL), \
         (2, N'Michael', N'Blythe', N'Sales', 3763178.18), \
         (3, N'Linda', N'Mitchell', N'Sales', 4251368.55)",
    );
}

#[test]
fn the_references_procedure_returns_more_than_one_result_set() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    with_staff(&mut session);
    succeeds(
        &mut session,
        "CREATE PROCEDURE dbo.uspMultipleResults
         AS
             SELECT TOP(2) LastName AS last_name FROM Staff ORDER BY Id;
             SELECT TOP(1) Department AS department FROM Staff ORDER BY Id DESC;",
    );
    let steps = succeeds(&mut session, "EXECUTE dbo.uspMultipleResults;");
    assert_eq!(
        result_sets(&steps),
        vec![
            vec!["last_name", "Ackerman", "Blythe"],
            vec!["department", "Sales"],
        ]
    );
}

#[test]
fn the_references_return_code_procedure_prints_and_returns_each_code() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    with_staff(&mut session);
    succeeds(
        &mut session,
        "CREATE PROCEDURE dbo.usp_GetSalesYTD
             @SalesPerson nvarchar(50) = NULL,
             @SalesYTD decimal(12,2) = NULL OUTPUT
         AS
             IF @SalesPerson IS NULL
             BEGIN
                 PRINT 'ERROR: You must specify a last name for the sales person.'
                 RETURN(1)
             END
             ELSE
             BEGIN
                 IF (SELECT COUNT(*) FROM Staff WHERE LastName = @SalesPerson) = 0
                     RETURN(2)
             END
             SELECT @SalesYTD = SalesYTD FROM Staff WHERE LastName = @SalesPerson;
             IF @@ERROR <> 0
             BEGIN
                 RETURN(3)
             END
             ELSE
             BEGIN
                 IF @SalesYTD IS NULL
                     RETURN(4)
                 ELSE
                     RETURN(0)
             END",
    );
    let steps = succeeds(
        &mut session,
        "DECLARE @SalesYTDForSalesPerson decimal(12,2), @ret_code int, @none int, @missing int, @empty int;
         EXECUTE @ret_code = dbo.usp_GetSalesYTD N'Blythe', @SalesYTD = @SalesYTDForSalesPerson OUTPUT;
         EXECUTE @none = dbo.usp_GetSalesYTD;
         EXECUTE @missing = dbo.usp_GetSalesYTD N'Nobody';
         EXECUTE @empty = dbo.usp_GetSalesYTD N'Ackerman';
         SELECT @ret_code AS ret_code, @SalesYTDForSalesPerson AS sales_ytd, @none AS no_name,
             @missing AS missing_person, @empty AS no_sales;",
    );
    assert_eq!(
        result_sets(&steps),
        vec![vec![
            "ret_code|sales_ytd|no_name|missing_person|no_sales",
            "0|3763178.18|1|2|4"
        ]]
    );
    assert_eq!(
        printed(&steps),
        vec!["ERROR: You must specify a last name for the sales person."]
    );
}

#[test]
fn the_references_output_parameters_feed_the_callers_if() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(
        &mut session,
        "CREATE TABLE Product (Id int PRIMARY KEY, Name nvarchar(50) NOT NULL, \
         Category nvarchar(50) NOT NULL, ListPrice decimal(10,2) NOT NULL)",
    );
    succeeds(
        &mut session,
        "INSERT INTO Product VALUES (1, N'Road-750', N'Road Bikes', 539.99), \
         (2, N'Mountain-500', N'Mountain Bikes', 564.99), \
         (3, N'Touring-3000', N'Touring Bikes', 742.35), (4, N'Helmet', N'Helmets', 34.99)",
    );
    succeeds(
        &mut session,
        "CREATE PROCEDURE dbo.uspGetList @Product varchar(40)
             , @MaxPrice decimal(10,2)
             , @ComparePrice decimal(10,2) OUTPUT
             , @ListPrice decimal(10,2) OUT
         AS
             SET NOCOUNT ON;
             SELECT Name AS product, ListPrice AS list_price
             FROM Product
             WHERE Category LIKE @Product AND ListPrice < @MaxPrice
             ORDER BY Id;
             SET @ListPrice = (SELECT MAX(ListPrice) FROM Product
                 WHERE Category LIKE @Product AND ListPrice < @MaxPrice);
             SET @ComparePrice = @MaxPrice;",
    );
    let steps = succeeds(
        &mut session,
        "DECLARE @ComparePrice decimal(10,2), @Cost decimal(10,2);
         EXECUTE dbo.uspGetList '%Bikes%', 700, @ComparePrice OUT, @Cost OUTPUT
         IF @Cost <= @ComparePrice
         BEGIN
             PRINT 'These products can be purchased for less than $'
                 + RTRIM(CAST(@ComparePrice AS varchar(20))) + '.'
         END
         ELSE
             PRINT 'The prices for all products in this category exceed $'
                 + RTRIM(CAST(@ComparePrice AS varchar(20))) + '.'",
    );
    assert_eq!(
        result_sets(&steps),
        vec![vec![
            "product|list_price",
            "Road-750|539.99",
            "Mountain-500|564.99"
        ]]
    );
    assert_eq!(
        printed(&steps),
        vec!["These products can be purchased for less than $700.00."]
    );
}

#[test]
fn a_trigger_body_executes_a_stored_procedure() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(
        &mut session,
        "CREATE TABLE dbo.orders (id int PRIMARY KEY, qty int NOT NULL)",
    );
    succeeds(
        &mut session,
        "CREATE TABLE dbo.order_log (id int, note nvarchar(40))",
    );
    succeeds(
        &mut session,
        "CREATE PROCEDURE dbo.log_order @id int, @note nvarchar(40) AS
             SET NOCOUNT ON;
             INSERT INTO dbo.order_log VALUES (@id, @note);",
    );
    succeeds(
        &mut session,
        "CREATE TRIGGER dbo.orders_logged ON dbo.orders AFTER INSERT AS
         BEGIN
             SET NOCOUNT ON;
             DECLARE @id int = (SELECT MAX(id) FROM inserted);
             EXEC dbo.log_order @id, N'inserted';
         END",
    );
    succeeds(&mut session, "INSERT INTO dbo.orders VALUES (7, 2)");
    let steps = succeeds(&mut session, "SELECT id, note FROM dbo.order_log");
    assert_eq!(result_sets(&steps), vec![vec!["id|note", "7|inserted"]]);
}

#[test]
fn procedures_and_functions_outlive_the_session_that_created_them() {
    let store = MemoryStore::new();
    {
        let mut creator = session(&store);
        succeeds(
            &mut creator,
            "CREATE FUNCTION dbo.Twice (@n int) RETURNS int AS BEGIN RETURN @n * 2 END",
        );
        succeeds(
            &mut creator,
            "CREATE PROCEDURE dbo.TwiceOf @n int, @r int OUTPUT AS SET @r = dbo.Twice(@n)",
        );
    }
    let mut later = session(&store);
    let steps = succeeds(
        &mut later,
        "DECLARE @r int; EXEC dbo.TwiceOf 21, @r OUTPUT; SELECT @r AS r, dbo.Twice(4) AS eight",
    );
    assert_eq!(result_sets(&steps), vec![vec!["r|eight", "42|8"]]);
}
