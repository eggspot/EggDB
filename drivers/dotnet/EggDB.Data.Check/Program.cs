using EggDB.Data;

var path = Path.Combine(Path.GetTempPath(), $"eggdb-dotnet-check-{Environment.ProcessId}.db");
File.Delete(path);
using (var connection = new EggDbConnection($"Data Source={path};Dialect=sqlite"))
{
    connection.Open();
    using (var create = connection.CreateCommand())
    {
        create.CommandText = "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, photo BLOB)";
        create.ExecuteNonQuery();
    }
    using (var transaction = connection.BeginTransaction())
    {
        using var insert = connection.CreateCommand();
        insert.CommandText = "INSERT INTO people (name, score, photo) VALUES (?, ?, ?)";
        foreach (var (name, score) in new[] { ("Ada", 1.5), ("Grace", 2.5) })
        {
            insert.Parameters.Clear();
            insert.Parameters.Add(new EggDbParameter { Value = name });
            insert.Parameters.Add(new EggDbParameter { Value = score });
            insert.Parameters.Add(new EggDbParameter { Value = new byte[] { 0, 255 } });
            Check(insert.ExecuteNonQuery() == 1, "an insert changes one row");
        }
        transaction.Commit();
    }
    using (var select = connection.CreateCommand())
    {
        select.CommandText = "SELECT id, name, score, photo FROM people ORDER BY id";
        using var reader = select.ExecuteReader();
        var rows = new List<string>();
        while (reader.Read())
        {
            rows.Add($"{reader.GetInt64(0)} {reader.GetString(1)} {reader.GetDouble(2)} {((byte[])reader.GetValue(3)).Length}");
        }
        Check(string.Join(";", rows) == "1 Ada 1.5 2;2 Grace 2.5 2", $"rows read back: {string.Join(";", rows)}");
    }
    using (var count = connection.CreateCommand())
    {
        count.CommandText = "SELECT count(*) FROM people";
        Check(Convert.ToInt64(count.ExecuteScalar()) == 2, "count(*) is 2");
    }
    try
    {
        using var wrong = connection.CreateCommand();
        wrong.CommandText = "SELECT * FROM nowhere";
        wrong.ExecuteReader();
        Check(false, "a missing table fails");
    }
    catch (EggDbException error)
    {
        Check(error.Message.Contains("nowhere"), $"the error names the table: {error.Message}");
    }
}
File.Delete(path);
Console.WriteLine("dotnet driver check: passed");

static void Check(bool condition, string what)
{
    if (!condition)
    {
        throw new InvalidOperationException($"dotnet driver check failed: {what}");
    }
}
