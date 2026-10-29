using System;
using System.Collections.Generic;
using System.Data;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Text;
using Microsoft.Data.SqlClient;

internal static class TdsBulkCheck
{
    private static readonly string[] Columns = { "id", "name", "price", "baked", "flag", "code", "notes" };

    private const string Definition =
        "(id INT PRIMARY KEY, name NVARCHAR(40) NOT NULL, price DECIMAL(10,2), baked DATETIME2(3), "
        + "flag BIT NOT NULL, code UNIQUEIDENTIFIER, notes NVARCHAR(MAX))";

    internal static int Run(string connectionString, string bcpFile)
    {
        var source = ReadCharacterFormat(bcpFile);
        using var connection = new SqlConnection(connectionString);
        connection.Open();
        Step("setup", () => Setup(connection));
        Step("bulk copy", () => Copy(connection, source));
        Step("rows", () => PrintRows(connection, "dbo.items"));
        Step("compare", () => Compare(connection, source));
        Step("keep nulls", () => KeepNulls(connection));
        Step("failed load", () => FailedLoad(connection, source));
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

    private static List<object?[]> ReadCharacterFormat(string path)
    {
        var rows = new List<object?[]>();
        var text = File.ReadAllText(path, new UTF8Encoding(false));
        foreach (var line in text.Split('\n'))
        {
            var record = line.EndsWith('\r') ? line[..^1] : line;
            if (record.Length == 0)
            {
                continue;
            }
            var fields = record.Split('\t');
            if (fields.Length != Columns.Length)
            {
                throw new InvalidDataException("a row of " + fields.Length + " fields: " + record);
            }
            rows.Add(fields.Select<string, object?>(Field).ToArray());
        }
        return rows;
    }

    private static object? Field(string field, int column) => field switch
    {
        "" => null,
        "\0" => column == 1 || column == 6 ? "" : null,
        _ => column switch
        {
            0 => int.Parse(field, CultureInfo.InvariantCulture),
            2 => decimal.Parse(field, CultureInfo.InvariantCulture),
            3 => DateTime.Parse(field, CultureInfo.InvariantCulture),
            4 => field == "1",
            5 => Guid.Parse(field),
            _ => field,
        },
    };

    private static DataTable Table(IEnumerable<object?[]> rows)
    {
        var table = new DataTable();
        table.Columns.Add("id", typeof(int));
        table.Columns.Add("name", typeof(string));
        table.Columns.Add("price", typeof(decimal));
        table.Columns.Add("baked", typeof(DateTime));
        table.Columns.Add("flag", typeof(bool));
        table.Columns.Add("code", typeof(Guid));
        table.Columns.Add("notes", typeof(string));
        foreach (var row in rows)
        {
            table.Rows.Add(row.Select(value => value ?? DBNull.Value).ToArray());
        }
        return table;
    }

    private static void Execute(SqlConnection connection, string text)
    {
        using var command = new SqlCommand(text, connection);
        command.ExecuteNonQuery();
    }

    private static void Setup(SqlConnection connection)
    {
        Execute(connection, "DROP TABLE IF EXISTS dbo.items");
        Execute(connection, "CREATE TABLE dbo.items " + Definition);
        Execute(connection, "DROP TABLE IF EXISTS dbo.labels");
        Execute(connection, "CREATE TABLE dbo.labels (id INT PRIMARY KEY, label NVARCHAR(20) DEFAULT N'none')");
        Console.WriteLine("ready");
    }

    private static SqlBulkCopy Copier(SqlConnection connection, string table, SqlBulkCopyOptions options)
    {
        var copier = new SqlBulkCopy(connection, options, null!) { DestinationTableName = table };
        return copier;
    }

    private static void Copy(SqlConnection connection, List<object?[]> source)
    {
        using var copier = Copier(connection, "dbo.items", SqlBulkCopyOptions.CheckConstraints);
        copier.BatchSize = 2;
        foreach (var column in Columns)
        {
            copier.ColumnMappings.Add(column, column);
        }
        copier.WriteToServer(Table(source));
        Console.WriteLine("copied " + copier.RowsCopied.ToString(CultureInfo.InvariantCulture));
    }

    private static string Shown(object? value) => value switch
    {
        null => "NULL",
        DBNull => "NULL",
        decimal number => number.ToString(CultureInfo.InvariantCulture),
        bool flag => flag ? "1" : "0",
        DateTime moment => moment.ToString("yyyy-MM-dd HH:mm:ss.fff", CultureInfo.InvariantCulture),
        Guid code => code.ToString().ToUpperInvariant(),
        string text when text.Length == 0 => "(empty)",
        IFormattable formattable => formattable.ToString(null, CultureInfo.InvariantCulture),
        _ => value.ToString() ?? "NULL",
    };

    private static List<string> Rows(SqlConnection connection, string table)
    {
        var lines = new List<string>();
        using var select = new SqlCommand(
            "SELECT " + string.Join(", ", Columns) + " FROM " + table + " ORDER BY id", connection);
        using var rows = select.ExecuteReader();
        while (rows.Read())
        {
            var cells = new List<string>();
            for (var at = 0; at < rows.FieldCount; at++)
            {
                cells.Add(Shown(rows.GetValue(at)));
            }
            lines.Add(string.Join("|", cells));
        }
        return lines;
    }

    private static void PrintRows(SqlConnection connection, string table)
    {
        foreach (var line in Rows(connection, table))
        {
            Console.WriteLine(line);
        }
    }

    private static void Compare(SqlConnection connection, List<object?[]> source)
    {
        var expected = source
            .OrderBy(row => (int)row[0]!)
            .Select(row => string.Join("|", row.Select(Shown)))
            .ToList();
        var copied = Rows(connection, "dbo.items");
        Console.WriteLine("SqlBulkCopy rows match the bcp file: " + (copied.SequenceEqual(expected) ? "yes" : "no"));
        var imported = Rows(connection, "dbo.items_imported");
        Console.WriteLine("eggdb import rows match the bcp file: " + (imported.SequenceEqual(expected) ? "yes" : "no"));
    }

    private static DataTable Labels(params (int Id, string? Label)[] rows)
    {
        var table = new DataTable();
        table.Columns.Add("id", typeof(int));
        table.Columns.Add("label", typeof(string));
        foreach (var (id, label) in rows)
        {
            table.Rows.Add(id, (object?)label ?? DBNull.Value);
        }
        return table;
    }

    private static void KeepNulls(SqlConnection connection)
    {
        using (var kept = Copier(
            connection, "dbo.labels", SqlBulkCopyOptions.KeepNulls | SqlBulkCopyOptions.UseInternalTransaction))
        {
            kept.WriteToServer(Labels((1, null), (2, "two")));
        }
        using (var defaulted = Copier(connection, "dbo.labels", SqlBulkCopyOptions.TableLock))
        {
            defaulted.WriteToServer(Labels((3, null)));
        }
        using var select = new SqlCommand("SELECT id, label FROM dbo.labels ORDER BY id", connection);
        using var rows = select.ExecuteReader();
        while (rows.Read())
        {
            Console.WriteLine(Shown(rows.GetValue(0)) + "|" + Shown(rows.GetValue(1)));
        }
    }

    private static void FailedLoad(SqlConnection connection, List<object?[]> source)
    {
        try
        {
            using var copier = Copier(connection, "dbo.items", SqlBulkCopyOptions.Default);
            foreach (var column in Columns)
            {
                copier.ColumnMappings.Add(column, column);
            }
            var again = source.Take(1).Select(row => row.ToArray()).ToList();
            again[0][0] = 100;
            copier.WriteToServer(Table(again.Concat(source.Take(1))));
        }
        catch (SqlException failure)
        {
            Console.WriteLine("error " + failure.Number.ToString(CultureInfo.InvariantCulture));
        }
        using var count = new SqlCommand("SELECT COUNT(*) AS n FROM dbo.items", connection);
        Console.WriteLine("count " + Shown(count.ExecuteScalar()));
    }
}
