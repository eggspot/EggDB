using System;
using System.Collections.Generic;
using System.Data;
using System.Globalization;
using Microsoft.Data.SqlClient;

internal static class TdsAppCheck
{
    private static int Main(string[] arguments)
    {
        if (arguments.Length == 2 && arguments[1] == "metadata")
        {
            return TdsMetadataCheck.Run(arguments[0]);
        }
        if (arguments.Length != 1)
        {
            Console.Error.WriteLine("usage: dotnet run --project TdsAppCheck -- <connection string> [metadata]");
            return 2;
        }
        var connectionString = arguments[0];
        using (var connection = new SqlConnection(connectionString))
        {
            connection.Open();
            Step("setup", () => Setup(connection));
            Step("insert", () => Insert(connection));
            Step("select", () => PrintRows(connection, 1));
            Step("update", () => Update(connection));
            Step("commit", () => Commit(connection));
            Step("rollback", () => Rollback(connection));
            Step("delete", () => Delete(connection));
            Step("error", () => Error(connection));
        }
        Step("pooled", () => Pooled(connectionString));
        return 0;
    }

    private static void Step(string name, Action body)
    {
        Console.WriteLine("== " + name);
        try
        {
            body();
        }
        catch (SqlException failure)
        {
            Console.WriteLine("error " + failure.Number.ToString(CultureInfo.InvariantCulture) + " " + failure.Message);
        }
    }

    private static string Shown(object? value) => value switch
    {
        null => "NULL",
        DBNull => "NULL",
        decimal number => number.ToString(CultureInfo.InvariantCulture),
        bool flag => flag ? "1" : "0",
        DateTime moment => moment.ToString("yyyy-MM-dd HH:mm:ss.fff", CultureInfo.InvariantCulture),
        Guid code => code.ToString().ToUpperInvariant(),
        IFormattable formattable => formattable.ToString(null, CultureInfo.InvariantCulture),
        _ => value.ToString() ?? "NULL",
    };

    private static void Setup(SqlConnection connection)
    {
        using var drop = new SqlCommand("DROP TABLE IF EXISTS bakes", connection);
        drop.ExecuteNonQuery();
        using var create = new SqlCommand(
            "CREATE TABLE bakes (id INT PRIMARY KEY, name NVARCHAR(40) NOT NULL, "
            + "price DECIMAL(10,2), baked DATETIME2(3), flag BIT NOT NULL, code UNIQUEIDENTIFIER)",
            connection);
        create.ExecuteNonQuery();
        Console.WriteLine("ready");
    }

    private static SqlCommand Inserting(SqlConnection connection, SqlTransaction? transaction)
    {
        var insert = new SqlCommand(
            "INSERT INTO bakes (id, name, price, baked, flag, code) VALUES (@id, @name, @price, @baked, @flag, @code)",
            connection,
            transaction);
        insert.Parameters.Add("@id", SqlDbType.Int);
        insert.Parameters.Add("@name", SqlDbType.NVarChar, 40);
        var price = insert.Parameters.Add("@price", SqlDbType.Decimal);
        price.Precision = 10;
        price.Scale = 2;
        var baked = insert.Parameters.Add("@baked", SqlDbType.DateTime2);
        baked.Scale = 3;
        insert.Parameters.Add("@flag", SqlDbType.Bit);
        insert.Parameters.Add("@code", SqlDbType.UniqueIdentifier);
        return insert;
    }

    private static void InsertOne(
        SqlCommand insert, int id, string name, string? price, string? baked, bool flag, string? code)
    {
        insert.Parameters["@id"].Value = id;
        insert.Parameters["@name"].Value = name;
        insert.Parameters["@price"].Value =
            price is null ? DBNull.Value : (object)decimal.Parse(price, CultureInfo.InvariantCulture);
        insert.Parameters["@baked"].Value =
            baked is null ? DBNull.Value : (object)DateTime.Parse(baked, CultureInfo.InvariantCulture);
        insert.Parameters["@flag"].Value = flag;
        insert.Parameters["@code"].Value = code is null ? DBNull.Value : (object)Guid.Parse(code);
        Console.WriteLine("inserted " + Shown(insert.ExecuteNonQuery()));
    }

    private static void Insert(SqlConnection connection)
    {
        using var insert = Inserting(connection, null);
        insert.Prepare();
        InsertOne(insert, 1, "bread", "12.50", "2024-05-06T07:08:09.123", true, "6F9619FF-8B86-D011-B42D-00C04FC964FF");
        InsertOne(insert, 2, "egg", "0.35", "2024-05-06T08:00:00", false, null);
        InsertOne(insert, 3, "pie", null, null, true, "1B4E28BA-2FA1-11D2-883F-0016D3CCA427");
    }

    private static void PrintRows(SqlConnection connection, int lowest)
    {
        using var select = new SqlCommand(
            "SELECT id, name, price, baked, flag, code FROM bakes WHERE id >= @lowest ORDER BY id", connection);
        select.Parameters.Add("@lowest", SqlDbType.Int).Value = lowest;
        using var rows = select.ExecuteReader();
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

    private static void Count(SqlConnection connection)
    {
        using var count = new SqlCommand("SELECT COUNT(*) AS n FROM bakes", connection);
        Console.WriteLine("count " + Shown(count.ExecuteScalar()));
    }

    private static void Update(SqlConnection connection)
    {
        using var update = new SqlCommand("UPDATE bakes SET price = @price WHERE name = @name", connection);
        var price = update.Parameters.Add("@price", SqlDbType.Decimal);
        price.Precision = 10;
        price.Scale = 2;
        price.Value = 13.00m;
        update.Parameters.Add("@name", SqlDbType.NVarChar, 40).Value = "bread";
        Console.WriteLine("updated " + Shown(update.ExecuteNonQuery()));
        PrintRows(connection, 1);
    }

    private static void Commit(SqlConnection connection)
    {
        using (var transaction = connection.BeginTransaction())
        {
            using var insert = Inserting(connection, transaction);
            InsertOne(insert, 4, "tart", "4.20", "2024-05-07T09:30:00", false, null);
            transaction.Commit();
        }
        Count(connection);
    }

    private static void Rollback(SqlConnection connection)
    {
        using (var transaction = connection.BeginTransaction())
        {
            using var insert = Inserting(connection, transaction);
            InsertOne(insert, 5, "scone", "2.00", null, true, null);
            transaction.Rollback();
        }
        Count(connection);
    }

    private static void Delete(SqlConnection connection)
    {
        using var delete = new SqlCommand("DELETE FROM bakes WHERE id = @id", connection);
        delete.Parameters.Add("@id", SqlDbType.Int).Value = 3;
        Console.WriteLine("deleted " + Shown(delete.ExecuteNonQuery()));
        PrintRows(connection, 1);
    }

    private static void Error(SqlConnection connection)
    {
        using var missing = new SqlCommand("SELECT * FROM missing_table WHERE id = @id", connection);
        missing.Parameters.Add("@id", SqlDbType.Int).Value = 1;
        using var rows = missing.ExecuteReader();
        rows.Read();
    }

    private static void Pooled(string connectionString)
    {
        using (var first = new SqlConnection(connectionString))
        {
            first.Open();
            using var quiet = new SqlCommand("SET NOCOUNT ON", first);
            quiet.ExecuteNonQuery();
        }
        using var second = new SqlConnection(connectionString);
        second.Open();
        using var update = new SqlCommand("UPDATE bakes SET flag = @flag WHERE id = @id", second);
        update.Parameters.Add("@flag", SqlDbType.Bit).Value = true;
        update.Parameters.Add("@id", SqlDbType.Int).Value = 2;
        Console.WriteLine("updated " + Shown(update.ExecuteNonQuery()));
    }
}
