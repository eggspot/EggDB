using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using Microsoft.Data.SqlClient;

internal static class TdsLogicCheck
{
    internal static int Run(string connectionString, string scriptFile)
    {
        var batches = Batches(File.ReadAllText(scriptFile));
        using var connection = new SqlConnection(connectionString);
        connection.InfoMessage += (_, message) =>
        {
            foreach (SqlError info in message.Errors)
            {
                Console.WriteLine("info " + Number(info.Number) + " " + info.Message);
            }
        };
        connection.Open();
        for (var index = 0; index < batches.Count; index++)
        {
            Console.WriteLine("== batch " + Number(index + 1));
            try
            {
                Execute(connection, batches[index]);
                Console.WriteLine("done");
            }
            catch (SqlException failure)
            {
                foreach (SqlError error in failure.Errors)
                {
                    Console.WriteLine(
                        "error " + Number(error.Number) + " " + Number(error.Class) + " "
                        + Number(error.State) + " " + Number(error.LineNumber) + " " + error.Message);
                }
            }
        }
        return 0;
    }

    private static string Number(int number) => number.ToString(CultureInfo.InvariantCulture);

    private static List<string> Batches(string script)
    {
        var batches = new List<string>();
        var current = new List<string>();
        foreach (var line in script.Replace("\r\n", "\n").Split('\n'))
        {
            if (line.Trim().Equals("GO", StringComparison.OrdinalIgnoreCase))
            {
                if (current.Count > 0)
                {
                    batches.Add(string.Join("\n", current));
                }
                current.Clear();
                continue;
            }
            if (line.Trim().Length > 0 || current.Count > 0)
            {
                current.Add(line);
            }
        }
        if (current.Count > 0 && string.Join("\n", current).Trim().Length > 0)
        {
            batches.Add(string.Join("\n", current));
        }
        return batches;
    }

    private static string Shown(object? value) => value switch
    {
        null => "NULL",
        DBNull => "NULL",
        decimal number => number.ToString(CultureInfo.InvariantCulture),
        bool flag => flag ? "1" : "0",
        IFormattable formattable => formattable.ToString(null, CultureInfo.InvariantCulture),
        _ => value.ToString() ?? "NULL",
    };

    private static void Execute(SqlConnection connection, string batch)
    {
        using var command = new SqlCommand(batch, connection);
        using var reader = command.ExecuteReader();
        do
        {
            if (reader.FieldCount == 0)
            {
                continue;
            }
            var names = new List<string>();
            for (var column = 0; column < reader.FieldCount; column++)
            {
                names.Add(reader.GetName(column));
            }
            Console.WriteLine(string.Join("|", names));
            while (reader.Read())
            {
                var values = new List<string>();
                for (var column = 0; column < reader.FieldCount; column++)
                {
                    values.Add(Shown(reader.GetValue(column)));
                }
                Console.WriteLine(string.Join("|", values));
            }
        }
        while (reader.NextResult());
    }
}
