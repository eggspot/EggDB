using System.Data.Common;
using EggDB.Data;
using Microsoft.EntityFrameworkCore.Storage;

namespace EggDB.EntityFrameworkCore.Internal;

/// <summary>Creates, finds and deletes an EggDB database: a file when embedded, the server's tables over the network.</summary>
public class EggDbDatabaseCreator : RelationalDatabaseCreator
{
    private readonly IEggDbRelationalConnection _connection;
    private readonly IRawSqlCommandBuilder _rawSqlCommandBuilder;

    /// <summary>A creator built from EF Core's dependencies.</summary>
    public EggDbDatabaseCreator(
        RelationalDatabaseCreatorDependencies dependencies,
        IEggDbRelationalConnection connection,
        IRawSqlCommandBuilder rawSqlCommandBuilder)
        : base(dependencies)
    {
        _connection = connection;
        _rawSqlCommandBuilder = rawSqlCommandBuilder;
    }

    private string? FilePath()
    {
        var builder = new DbConnectionStringBuilder { ConnectionString = _connection.ConnectionString ?? string.Empty };
        var source = builder.TryGetValue("Data Source", out var value) ? value?.ToString() : null;
        if (string.IsNullOrEmpty(source) || source == ":memory:" || source.Contains("://", StringComparison.Ordinal))
        {
            return null;
        }
        return source;
    }

    /// <inheritdoc />
    public override bool Exists()
    {
        var path = FilePath();
        if (path is not null)
        {
            return File.Exists(path);
        }
        try
        {
            _connection.Open();
            _connection.Close();
            return true;
        }
        catch (EggDbException)
        {
            return false;
        }
    }

    /// <inheritdoc />
    public override bool HasTables()
    {
        var count = _rawSqlCommandBuilder
            .Build("SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema')")
            .ExecuteScalar(new RelationalCommandParameterObject(_connection, null, null, null, Dependencies.CommandLogger, CommandSource.Migrations));
        return Convert.ToInt64(count, System.Globalization.CultureInfo.InvariantCulture) != 0;
    }

    /// <inheritdoc />
    public override void Create()
    {
        _connection.Open();
        _connection.Close();
    }

    /// <inheritdoc />
    public override void Delete()
    {
        var path = FilePath() ?? throw new NotSupportedException("EggDB deletes only an embedded database's file; drop a server's tables instead.");
        _connection.Close();
        File.Delete(path);
    }
}
