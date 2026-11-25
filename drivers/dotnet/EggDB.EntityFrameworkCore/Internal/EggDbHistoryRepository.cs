using Microsoft.EntityFrameworkCore.Migrations;

namespace EggDB.EntityFrameworkCore.Internal;

/// <summary>The migrations history table, <c>__EFMigrationsHistory</c>, in EggDB.</summary>
public class EggDbHistoryRepository : HistoryRepository
{
    /// <summary>A repository built from EF Core's dependencies.</summary>
    public EggDbHistoryRepository(HistoryRepositoryDependencies dependencies)
        : base(dependencies)
    {
    }

    private static string Literal(string text) => "'" + text.Replace("'", "''", StringComparison.Ordinal) + "'";

    /// <inheritdoc />
    protected override string ExistsSql =>
        "SELECT count(*) FROM information_schema.tables WHERE table_name = " + Literal(TableName)
        + (TableSchema is null ? string.Empty : " AND table_schema = " + Literal(TableSchema));

    /// <inheritdoc />
    protected override bool InterpretExistsResult(object? value) =>
        value is not null && value is not DBNull && Convert.ToInt64(value, System.Globalization.CultureInfo.InvariantCulture) != 0;

    /// <inheritdoc />
    public override string GetCreateIfNotExistsScript() =>
        GetCreateScript().Replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", StringComparison.Ordinal);

    /// <inheritdoc />
    public override string GetBeginIfNotExistsScript(string migrationId) =>
        throw new NotSupportedException("EggDB's idempotent migration scripts are not supported yet; generate a plain script.");

    /// <inheritdoc />
    public override string GetBeginIfExistsScript(string migrationId) =>
        throw new NotSupportedException("EggDB's idempotent migration scripts are not supported yet; generate a plain script.");

    /// <inheritdoc />
    public override string GetEndIfScript() =>
        throw new NotSupportedException("EggDB's idempotent migration scripts are not supported yet; generate a plain script.");
}
