using System;
using System.Collections.Generic;
using System.Data;
using System.Data.Common;
using System.Globalization;
using System.Linq;
using Microsoft.Data.SqlClient;

internal static class TdsMetadataCheck
{
    internal static int Run(string connectionString)
    {
        using var connection = new SqlConnection(connectionString);
        connection.Open();
        Step("tables", () => Print(connection.GetSchema("Tables"), "TABLE_CATALOG", "TABLE_SCHEMA", "TABLE_NAME", "TABLE_TYPE"));
        Step("columns of dbo.customers", () => PrintColumns(connection, "dbo", "customers"));
        Step("columns of sales.orders", () => PrintColumns(connection, "sales", "orders"));
        Step("indexes of sales.orders", () => Print(
            Schema(connection, "Indexes", null, "sales", "orders", null),
            "table_schema", "table_name", "index_name", "type_desc"));
        Step("index columns of sales.orders", () => Print(
            Schema(connection, "IndexColumns", null, "sales", "orders", null, null),
            "index_name", "column_name", "ordinal_position", "KeyType"));
        Step("foreign keys", () => Print(
            connection.GetSchema("ForeignKeys"),
            "CONSTRAINT_SCHEMA", "CONSTRAINT_NAME", "TABLE_SCHEMA", "TABLE_NAME", "CONSTRAINT_TYPE", "IS_DEFERRABLE", "INITIALLY_DEFERRED"));
        return 0;
    }

    private static DataTable Schema(DbConnection connection, string collection, params string?[] restrictions) =>
        connection.GetSchema(collection, restrictions);

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
        IFormattable formattable => formattable.ToString(null, CultureInfo.InvariantCulture),
        _ => value.ToString() ?? "NULL",
    };

    private static List<string> Lines(DataTable table, params string[] columns)
    {
        var lines = new List<string>();
        foreach (DataRow row in table.Rows)
        {
            lines.Add(string.Join("|", columns.Select(column => Shown(row[column]))));
        }
        return lines;
    }

    private static void Print(DataTable table, params string[] columns)
    {
        var lines = Lines(table, columns);
        lines.Sort(StringComparer.Ordinal);
        foreach (var line in lines)
        {
            Console.WriteLine(line);
        }
    }

    private static void PrintColumns(SqlConnection connection, string schema, string table)
    {
        var columns = Schema(connection, "Columns", null, schema, table, null);
        foreach (var line in Lines(
            columns,
            "COLUMN_NAME", "ORDINAL_POSITION", "IS_NULLABLE", "DATA_TYPE", "CHARACTER_MAXIMUM_LENGTH",
            "NUMERIC_PRECISION", "NUMERIC_SCALE", "COLUMN_DEFAULT"))
        {
            Console.WriteLine(line);
        }
    }
}
