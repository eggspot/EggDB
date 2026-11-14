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
            rows.Add(FormattableString.Invariant($"{reader.GetInt64(0)} {reader.GetString(1)} {reader.GetDouble(2)} {((byte[])reader.GetValue(3)).Length}"));
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
using (var connection = new EggDbConnection("Data Source=:memory:;Dialect=postgresql"))
{
    connection.Open();
    using (var create = connection.CreateCommand())
    {
        create.CommandText = "CREATE TABLE t (id integer, name text)";
        create.ExecuteNonQuery();
    }
    using (var insert = connection.CreateCommand())
    {
        insert.CommandText = "INSERT INTO t VALUES (?, ?)";
        insert.Parameters.Add(new EggDbParameter { Value = 0L });
        insert.Parameters.Add(new EggDbParameter { Value = string.Empty });
        insert.Prepare();
        foreach (var (id, name) in new[] { (1L, "ada"), (2L, "grace") })
        {
            insert.Parameters[0].Value = id;
            insert.Parameters[1].Value = name;
            Check(insert.ExecuteNonQuery() == 1, "a prepared insert changes one row");
        }
    }
    Check(connection.CopyFrom("COPY t FROM STDIN", "3\tedsger\n") == 1, "COPY loads one row");
    using (var select = connection.CreateCommand())
    {
        select.CommandText = "SELECT name FROM t WHERE id > ? ORDER BY id";
        select.Parameters.Add(new EggDbParameter { Value = 1 });
        select.Prepare();
        using var reader = select.ExecuteReader();
        Check(reader.HasRows, "the stepped reader has rows");
        var names = new List<string>();
        while (reader.Read())
        {
            names.Add(reader.GetString(0));
        }
        Check(string.Join(",", names) == "grace,edsger", $"stepped rows: {string.Join(",", names)}");
    }
    using var copied = new MemoryStream();
    connection.CopyTo("COPY (SELECT id FROM t ORDER BY id) TO STDOUT", copied);
    Check(System.Text.Encoding.UTF8.GetString(copied.ToArray()) == "1\n2\n3\n", "COPY reads the rows back");
}
var keyValuePath = Path.Combine(Path.GetTempPath(), $"eggdb-kv-{Guid.NewGuid():N}.db");
using (var store = new EggDbKeyValue(keyValuePath))
{
    using (var write = store.BeginWrite())
    {
        Check(write.CreateCollection("sessions"), "the collection is made");
        write.Put("s:1", "ada", "sessions");
        write.Put("plain", "default");
        write.Commit();
    }
    using (var read = store.BeginRead())
    {
        Check(System.Text.Encoding.UTF8.GetString(read.Get("s:1", "sessions") ?? []) == "ada", "the collection holds its key");
        Check(read.Get("s:1") is null, "the default key space does not");
        Check(read.Scan(new EggDbScanOptions { Prefix = "s:"u8.ToArray() }, "sessions").Count == 1, "the prefix scan finds one key");
        Check(read.CollectionStats("sessions").Entries == 1, "the collection counts one entry");
    }
    using (var rename = store.BeginWrite())
    {
        rename.RenameCollection("sessions", "logins");
        rename.Commit();
    }
    using (var read = store.BeginRead())
    {
        Check(read.Collections().SequenceEqual(["logins"]), "the collection was renamed");
    }
}
File.Delete(keyValuePath);
using (var named = new EggDbConnection("Data Source=:memory:;Dialect=sqlite"))
{
    named.Open();
    using var create = named.CreateCommand();
    create.CommandText = "CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT, weight INTEGER)";
    create.ExecuteNonQuery();
    using var insert = named.CreateCommand();
    insert.CommandText = "INSERT INTO eggs (weight, colour) VALUES (@weight, @colour)";
    insert.Parameters.Add(new EggDbParameter { ParameterName = "@colour", Value = "brown" });
    insert.Parameters.Add(new EggDbParameter { ParameterName = "weight", Value = 61 });
    Check(insert.ExecuteNonQuery() == 1, "a named insert changes one row");
    using var select = named.CreateCommand();
    select.CommandText = "SELECT colour FROM eggs WHERE weight = :weight AND colour <> '@colour'";
    select.Parameters.Add(new EggDbParameter { ParameterName = "weight", Value = 61 });
    Check((string?)select.ExecuteScalar() == "brown", "named parameters bind by name, not inside quotes");
}
Console.WriteLine("dotnet driver check: passed");

static void Check(bool condition, string what)
{
    if (!condition)
    {
        throw new InvalidOperationException($"dotnet driver check failed: {what}");
    }
}
