using System.Data.Common;
using System.Diagnostics;
using EggDB.Data;
using Microsoft.Data.Sqlite;

var rows = int.TryParse(Environment.GetEnvironmentVariable("DRIVER_BENCH_ROWS"), out var givenRows) ? givenRows : 20000;
var lookups = int.TryParse(Environment.GetEnvironmentVariable("DRIVER_BENCH_LOOKUPS"), out var givenLookups) ? givenLookups : 5000;
var directory = Directory.CreateTempSubdirectory("eggdb-dotnet-bench").FullName;

double Timed(Action work)
{
    var watch = Stopwatch.StartNew();
    work();
    return watch.Elapsed.TotalMilliseconds;
}

double[] Side(DbConnection connection)
{
    connection.Open();
    using (var create = connection.CreateCommand())
    {
        create.CommandText = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)";
        create.ExecuteNonQuery();
    }
    var insert = Timed(() =>
    {
        using var transaction = connection.BeginTransaction();
        using var command = connection.CreateCommand();
        command.Transaction = transaction;
        command.CommandText = "INSERT INTO t VALUES (@id, @name, @score)";
        var id = command.CreateParameter();
        var name = command.CreateParameter();
        var score = command.CreateParameter();
        command.Parameters.Add(id);
        command.Parameters.Add(name);
        command.Parameters.Add(score);
        id.ParameterName = "@id";
        name.ParameterName = "@name";
        score.ParameterName = "@score";
        id.Value = 0L;
        name.Value = string.Empty;
        score.Value = 0.0;
        command.Prepare();
        for (var at = 1; at <= rows; at++)
        {
            id.Value = (long)at;
            name.Value = $"name {at}";
            score.Value = at * 0.25;
            command.ExecuteNonQuery();
        }
        transaction.Commit();
    });
    var lookup = Timed(() =>
    {
        using var command = connection.CreateCommand();
        command.CommandText = "SELECT name FROM t WHERE id = @id";
        var id = command.CreateParameter();
        id.ParameterName = "@id";
        command.Parameters.Add(id);
        id.Value = 1L;
        command.Prepare();
        for (var at = 1; at <= lookups; at++)
        {
            id.Value = (long)(at * 7 % rows + 1);
            using var reader = command.ExecuteReader();
            while (reader.Read())
            {
                _ = reader.GetString(0);
            }
        }
    });
    var scan = Timed(() =>
    {
        using var command = connection.CreateCommand();
        command.CommandText = "SELECT name FROM t";
        command.Prepare();
        using var reader = command.ExecuteReader();
        long total = 0;
        while (reader.Read())
        {
            total += reader.GetString(0).Length;
        }
        GC.KeepAlive(total);
    });
    connection.Close();
    return new[] { insert, lookup, scan };
}

var egg = Side(new EggDbConnection($"Data Source={Path.Combine(directory, "bench.eggdb")}"));
var lite = Side(new SqliteConnection($"Data Source={Path.Combine(directory, "bench.sqlite")}"));
string[] workloads = { "insert", "lookup", "scan" };
for (var at = 0; at < workloads.Length; at++)
{
    Console.WriteLine(FormattableString.Invariant($"dotnet {workloads[at]} {egg[at]:F2} {lite[at]:F2}"));
}
Directory.Delete(directory, recursive: true);
