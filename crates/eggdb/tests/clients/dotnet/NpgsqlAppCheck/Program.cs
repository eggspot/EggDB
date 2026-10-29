using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.Threading.Tasks;
using Npgsql;
using NpgsqlTypes;

internal static class NpgsqlAppCheck
{
    private static int Main(string[] arguments)
    {
        if (arguments.Length != 1)
        {
            Console.Error.WriteLine("usage: dotnet run --project NpgsqlAppCheck -- <connection string>");
            return 2;
        }
        using var dataSource = NpgsqlDataSource.Create(arguments[0]);
        Step("open", () => Open(dataSource));
        Step("setup", () => Setup(dataSource));
        Step("insert", () => Insert(dataSource));
        Step("select", () => PrintRows(dataSource));
        Step("update", () => Update(dataSource));
        Step("array", () => Arrays(dataSource));
        Step("rollback", () => Rollback(dataSource));
        Step("commit", () => Commit(dataSource));
        Step("batch", () => Batch(dataSource));
        Step("failing batch", () => FailingBatch(dataSource));
        Step("cancel", () => Cancel(dataSource));
        Step("delete", () => Delete(dataSource));
        return 0;
    }

    private static void Step(string name, Action body)
    {
        Console.WriteLine("== " + name);
        try
        {
            body();
        }
        catch (PostgresException failure)
        {
            Console.WriteLine("error " + failure.SqlState);
        }
        catch (NpgsqlException failure)
        {
            Console.WriteLine("failure " + failure.GetType().Name + " " + failure.Message);
        }
    }

    private static string Shown(object? value) => value switch
    {
        null => "NULL",
        DBNull => "NULL",
        decimal number => number.ToString(CultureInfo.InvariantCulture),
        bool flag => flag ? "true" : "false",
        DateTime moment => moment.ToString("yyyy-MM-dd HH:mm:ss.ffffff", CultureInfo.InvariantCulture),
        Guid code => code.ToString(),
        int[] numbers => "{" + string.Join(",", numbers) + "}",
        IFormattable formattable => formattable.ToString(null, CultureInfo.InvariantCulture),
        _ => value.ToString() ?? "NULL",
    };

    private static object ValueOrNull<T>(T? value)
        where T : struct => value.HasValue ? (object)value.Value : DBNull.Value;

    private static void Run(NpgsqlConnection connection, string sql)
    {
        using var command = new NpgsqlCommand(sql, connection);
        command.ExecuteNonQuery();
    }

    private static void Count(NpgsqlConnection connection, int lowest)
    {
        using var count = new NpgsqlCommand("SELECT count(*) FROM npgsql_bakes WHERE id >= @lowest", connection);
        count.Parameters.AddWithValue("lowest", lowest);
        Console.WriteLine("count " + Shown(count.ExecuteScalar()));
    }

    private static void Open(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        Console.WriteLine("state " + connection.State);
        using var sum = new NpgsqlCommand("SELECT @left + @right", connection);
        sum.Parameters.AddWithValue("left", 40);
        sum.Parameters.AddWithValue("right", 2);
        Console.WriteLine("sum " + Shown(sum.ExecuteScalar()));
    }

    private static void Setup(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        Run(connection, "DROP TABLE IF EXISTS npgsql_bakes");
        Run(
            connection,
            "CREATE TABLE npgsql_bakes (id integer PRIMARY KEY, name text NOT NULL, price numeric(10,2), "
            + "baked timestamp, flag boolean NOT NULL, code uuid, scores integer[])");
        Console.WriteLine("ready");
    }

    private static int InsertOne(
        NpgsqlConnection connection,
        NpgsqlTransaction? transaction,
        int id,
        string name,
        decimal? price,
        DateTime? baked,
        bool flag,
        Guid? code)
    {
        using var insert = new NpgsqlCommand(
            "INSERT INTO npgsql_bakes (id, name, price, baked, flag, code) VALUES (@id, @name, @price, @baked, @flag, @code)",
            connection,
            transaction);
        insert.Parameters.AddWithValue("id", NpgsqlDbType.Integer, id);
        insert.Parameters.AddWithValue("name", NpgsqlDbType.Text, name);
        insert.Parameters.AddWithValue("price", NpgsqlDbType.Numeric, ValueOrNull(price));
        insert.Parameters.AddWithValue("baked", NpgsqlDbType.Timestamp, ValueOrNull(baked));
        insert.Parameters.AddWithValue("flag", NpgsqlDbType.Boolean, flag);
        insert.Parameters.AddWithValue("code", NpgsqlDbType.Uuid, ValueOrNull(code));
        return insert.ExecuteNonQuery();
    }

    private static void Insert(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        var bread = InsertOne(
            connection, null, 1, "bread", 12.50m, new DateTime(2024, 5, 6, 7, 8, 9, 123), true,
            Guid.Parse("6f9619ff-8b86-d011-b42d-00c04fc964ff"));
        var egg = InsertOne(connection, null, 2, "egg", 0.35m, new DateTime(2024, 5, 6, 8, 0, 0), false, null);
        var pie = InsertOne(
            connection, null, 3, "pie", null, null, true, Guid.Parse("1b4e28ba-2fa1-11d2-883f-0016d3cca427"));
        Console.WriteLine("inserted " + (bread + egg + pie).ToString(CultureInfo.InvariantCulture));
    }

    private static void PrintRows(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        using var select = new NpgsqlCommand(
            "SELECT id, name, price, baked, flag, code, scores FROM npgsql_bakes ORDER BY id", connection);
        using var rows = select.ExecuteReader();
        var types = new List<string>();
        for (var at = 0; at < rows.FieldCount; at++)
        {
            types.Add(rows.GetName(at) + ":" + rows.GetFieldType(at).Name);
        }
        Console.WriteLine(string.Join("|", types));
        while (rows.Read())
        {
            var cells = new List<string>();
            for (var at = 0; at < rows.FieldCount; at++)
            {
                cells.Add(Shown(rows.GetValue(at)));
            }
            Console.WriteLine(string.Join("|", cells));
        }
    }

    private static void Update(NpgsqlDataSource dataSource)
    {
        using (var connection = dataSource.OpenConnection())
        {
            using var update = new NpgsqlCommand(
                "UPDATE npgsql_bakes SET price = price + @delta, flag = NOT flag WHERE id = @id", connection);
            update.Parameters.AddWithValue("delta", 0.50m);
            update.Parameters.AddWithValue("id", 1);
            Console.WriteLine("updated " + Shown(update.ExecuteNonQuery()));
        }
        PrintRows(dataSource);
    }

    private static void Arrays(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        Run(connection, "UPDATE npgsql_bakes SET scores = '{3,1,2}' WHERE id = 1");
        using var select = new NpgsqlCommand("SELECT scores FROM npgsql_bakes WHERE id = 1", connection);
        using var rows = select.ExecuteReader();
        while (rows.Read())
        {
            Console.WriteLine(rows.GetFieldType(0).Name + " " + Shown(rows.GetFieldValue<int[]>(0)));
        }
    }

    private static void Rollback(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        using (var transaction = connection.BeginTransaction())
        {
            var inserted = InsertOne(connection, transaction, 10, "scone", 2.00m, null, true, null);
            Console.WriteLine("inserted " + inserted.ToString(CultureInfo.InvariantCulture));
            transaction.Rollback();
        }
        Count(connection, 1);
    }

    private static void Commit(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        using (var transaction = connection.BeginTransaction())
        {
            var inserted = InsertOne(
                connection, transaction, 11, "tart", 4.20m, new DateTime(2024, 5, 7, 9, 30, 0), false, null);
            Console.WriteLine("inserted " + inserted.ToString(CultureInfo.InvariantCulture));
            transaction.Commit();
        }
        Count(connection, 1);
    }

    private static void Batch(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        using var batch = new NpgsqlBatch(connection)
        {
            BatchCommands =
            {
                new NpgsqlBatchCommand("INSERT INTO npgsql_bakes (id, name, flag) VALUES (20, 'roll', true)"),
                new NpgsqlBatchCommand("INSERT INTO npgsql_bakes (id, name, flag) VALUES (21, 'bun', false)"),
                new NpgsqlBatchCommand("SELECT name FROM npgsql_bakes WHERE id >= 20 ORDER BY id"),
            },
        };
        using (var rows = batch.ExecuteReader())
        {
            while (rows.Read())
            {
                Console.WriteLine("read " + Shown(rows.GetValue(0)));
            }
        }
        foreach (var command in batch.BatchCommands)
        {
            Console.WriteLine("affected " + command.RecordsAffected.ToString(CultureInfo.InvariantCulture));
        }
        Count(connection, 20);
    }

    private static void FailingBatch(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        using var batch = new NpgsqlBatch(connection)
        {
            BatchCommands =
            {
                new NpgsqlBatchCommand("INSERT INTO npgsql_bakes (id, name, flag) VALUES (30, 'bagel', true)"),
                new NpgsqlBatchCommand("INSERT INTO npgsql_bakes (id, name, flag) VALUES (1, 'twice', true)"),
                new NpgsqlBatchCommand("INSERT INTO npgsql_bakes (id, name, flag) VALUES (31, 'never', true)"),
            },
        };
        try
        {
            batch.ExecuteNonQuery();
            Console.WriteLine("the batch ran to the end");
        }
        catch (PostgresException failure)
        {
            Console.WriteLine("batch error " + failure.SqlState);
        }
        Count(connection, 30);
    }

    private static void Cancel(NpgsqlDataSource dataSource)
    {
        using var connection = dataSource.OpenConnection();
        using var sleep = new NpgsqlCommand("SELECT pg_sleep(30)", connection);
        var canceller = Task.Run(async () =>
        {
            await Task.Delay(1000);
            sleep.Cancel();
        });
        var watch = Stopwatch.StartNew();
        try
        {
            sleep.ExecuteNonQuery();
            Console.WriteLine("not cancelled");
        }
        catch (PostgresException failure)
        {
            Console.WriteLine("cancelled " + failure.SqlState);
        }
        catch (OperationCanceledException failure) when (failure.InnerException is PostgresException inner)
        {
            Console.WriteLine("cancelled " + inner.SqlState);
        }
        canceller.Wait();
        Console.WriteLine(watch.Elapsed < TimeSpan.FromSeconds(20) ? "stopped early" : "ran to the end");
        using var after = new NpgsqlCommand("SELECT 1", connection);
        Console.WriteLine("after " + Shown(after.ExecuteScalar()));
    }

    private static void Delete(NpgsqlDataSource dataSource)
    {
        using (var connection = dataSource.OpenConnection())
        {
            using var delete = new NpgsqlCommand("DELETE FROM npgsql_bakes WHERE id = @id", connection);
            delete.Parameters.AddWithValue("id", 2);
            Console.WriteLine("deleted " + Shown(delete.ExecuteNonQuery()));
        }
        PrintRows(dataSource);
    }
}
