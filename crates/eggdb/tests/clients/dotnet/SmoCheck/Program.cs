using System;
using System.Collections.Generic;
using System.Linq;
using Microsoft.Data.SqlClient;
using Microsoft.SqlServer.Management.Common;
using Microsoft.SqlServer.Management.Smo;

internal static class SmoCheck
{
    private static int Main(string[] arguments)
    {
        if (arguments.Length != 2)
        {
            Console.Error.WriteLine("usage: dotnet run --project SmoCheck -- <connection string> <database>");
            return 2;
        }
        try
        {
            Run(arguments[0], arguments[1]);
            return 0;
        }
        catch (Exception error)
        {
            Console.WriteLine($"FAILED: {error.GetType().Name}: {Describe(error)}");
            return 1;
        }
    }

    private static string Describe(Exception error)
    {
        var messages = new List<string>();
        for (Exception? current = error; current != null; current = current.InnerException)
        {
            messages.Add(current.Message);
        }
        return string.Join(" -> ", messages);
    }

    private static void Run(string connectionString, string databaseName)
    {
        using var connection = new SqlConnection(connectionString);
        var server = new Server(new ServerConnection(connection));
        Console.WriteLine($"server: version {server.Information.Version.Major}, edition known {!string.IsNullOrEmpty(server.Information.Edition)}");
        var databases = server.Databases.Cast<Database>().Select(database => database.Name).ToList();
        Require(databases.Contains(databaseName), $"Object Explorer's database list lacks {databaseName}: {string.Join(", ", databases)}");
        Console.WriteLine($"databases include {databaseName}");
        var database = server.Databases[databaseName];
        var schemas = database.Schemas.Cast<Schema>().Where(schema => !schema.IsSystemObject).Select(schema => schema.Name).OrderBy(name => name, StringComparer.Ordinal);
        Console.WriteLine($"schemas: {string.Join(", ", schemas)}");
        var tables = database.Tables.Cast<Table>().Where(table => !table.IsSystemObject).OrderBy(table => table.Schema, StringComparer.Ordinal).ThenBy(table => table.Name, StringComparer.Ordinal).ToList();
        Console.WriteLine($"tables: {string.Join(", ", tables.Select(table => $"{table.Schema}.{table.Name}"))}");
        foreach (var table in tables)
        {
            PrintTable(table);
        }
        var procedures = database.StoredProcedures.Cast<StoredProcedure>().Where(procedure => !procedure.IsSystemObject).Select(procedure => $"{procedure.Schema}.{procedure.Name}").OrderBy(name => name, StringComparer.Ordinal);
        Console.WriteLine($"procedures: {string.Join(", ", procedures)}");
        var views = database.Views.Cast<View>().Where(view => !view.IsSystemObject).Select(view => $"{view.Schema}.{view.Name}").OrderBy(name => name, StringComparer.Ordinal);
        Console.WriteLine($"views: {string.Join(", ", views)}");
        var scripter = new Scripter(server)
        {
            Options =
            {
                DriAll = true,
                Indexes = true,
                SchemaQualify = true,
                IncludeIfNotExists = false,
            },
        };
        var script = scripter.Script(tables.Select(table => (SqlSmoObject)table).ToArray()).Cast<string>().ToList();
        Require(script.Any(line => line.Contains("CREATE TABLE [dbo].[Customers]", StringComparison.Ordinal)), "Generate Scripts wrote no CREATE TABLE for dbo.Customers");
        Require(script.Any(line => line.Contains("FOREIGN KEY", StringComparison.Ordinal)), "Generate Scripts wrote no FOREIGN KEY");
        Console.WriteLine($"generate scripts: {script.Count} statements, CREATE TABLE and FOREIGN KEY present");
    }

    private static void PrintTable(Table table)
    {
        var columns = table.Columns.Cast<Column>().Select(column =>
            $"{column.Name} {column.DataType.Name}{(column.Nullable ? " null" : "")}{(column.InPrimaryKey ? " pk" : "")}{(column.Identity ? " identity" : "")}");
        Console.WriteLine($"{table.Schema}.{table.Name} columns: {string.Join(", ", columns)}");
        var indexes = table.Indexes.Cast<Index>().Select(index => $"{index.Name}{(index.IndexKeyType == IndexKeyType.DriPrimaryKey ? " primary" : "")}").OrderBy(name => name, StringComparer.Ordinal);
        Console.WriteLine($"{table.Schema}.{table.Name} indexes: {string.Join(", ", indexes)}");
        var foreignKeys = table.ForeignKeys.Cast<ForeignKey>().Select(key => $"{key.Name} -> {key.ReferencedTableSchema}.{key.ReferencedTable}");
        Console.WriteLine($"{table.Schema}.{table.Name} foreign keys: {string.Join(", ", foreignKeys)}");
        var checks = table.Checks.Cast<Check>().Select(check => check.Name);
        Console.WriteLine($"{table.Schema}.{table.Name} checks: {string.Join(", ", checks)}");
    }

    private static void Require(bool condition, string message)
    {
        if (!condition)
        {
            throw new InvalidOperationException(message);
        }
    }
}
