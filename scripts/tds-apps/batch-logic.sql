CREATE TABLE logic_items (id INT PRIMARY KEY, name NVARCHAR(20) NOT NULL, qty INT NOT NULL)
GO
INSERT INTO logic_items VALUES (1, N'rye', 4), (2, N'spelt', 0), (3, N'oat', 7)
GO
DECLARE @i int = 0, @total int = 0
WHILE @i < 5
BEGIN
    SET @i += 1
    IF @i = 3 CONTINUE
    SET @total += @i
END
PRINT N'loop total ' + CAST(@total AS nvarchar(10))
SELECT @i AS i, @total AS total
GO
BEGIN TRY
    INSERT INTO logic_items VALUES (1, N'again', 1)
END TRY
BEGIN CATCH
    SELECT ERROR_NUMBER() AS number, ERROR_SEVERITY() AS severity
END CATCH
GO
BEGIN TRY
    THROW 50001, N'custom failure', 2
END TRY
BEGIN CATCH
    PRINT N'caught ' + CAST(ERROR_NUMBER() AS nvarchar(10))
    RAISERROR(N'warning %s', 10, 1, N'only')
END CATCH
GO
DECLARE @count int
EXEC sp_executesql N'SELECT @c = COUNT(*) FROM logic_items WHERE qty > @min', N'@min int, @c int OUTPUT', @min = 1, @c = @count OUTPUT
SELECT @count AS counted
GO
CREATE PROCEDURE dbo.logic_stock @name nvarchar(20), @qty int OUTPUT
AS
SET NOCOUNT ON
SELECT @qty = qty FROM logic_items WHERE name = @name
IF @qty IS NULL RETURN 1
SELECT id, name FROM logic_items WHERE name = @name
RETURN 0
GO
DECLARE @q int, @rc int
EXEC @rc = dbo.logic_stock N'oat', @q OUTPUT
SELECT @rc AS status, @q AS qty
GO
CREATE FUNCTION dbo.logic_double (@n int) RETURNS int AS BEGIN RETURN @n * 2 END
GO
SELECT name, dbo.logic_double(qty) AS doubled FROM logic_items WHERE id < 3 ORDER BY id
GO
CREATE FUNCTION dbo.logic_in_stock (@min int) RETURNS TABLE AS RETURN (SELECT id, name FROM logic_items WHERE qty >= @min)
GO
SELECT id, name FROM dbo.logic_in_stock(5)
GO
CREATE PROCEDURE dbo.logic_split @total int, @half int OUTPUT AS SET @half = @total / 2 RETURN @total % 2
GO
RAISERROR(N'stopped here %d', 16, 3, 7)
GO
DROP TABLE logic_items
GO
DROP FUNCTION dbo.logic_double, dbo.logic_in_stock
GO
DROP PROCEDURE dbo.logic_stock
GO
