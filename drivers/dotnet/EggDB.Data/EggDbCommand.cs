using System.Data;
using System.Data.Common;
using System.Diagnostics.CodeAnalysis;

namespace EggDB.Data;

/// <summary>A SQL statement with <c>?</c> placeholders, run on an <see cref="EggDbConnection"/>.</summary>
public sealed class EggDbCommand : DbCommand
{
    private string _commandText = string.Empty;

    /// <inheritdoc />
    [AllowNull]
    public override string CommandText
    {
        get => _commandText;
        set => _commandText = value ?? string.Empty;
    }

    /// <inheritdoc />
    public override int CommandTimeout { get; set; }

    /// <inheritdoc />
    public override CommandType CommandType { get; set; } = CommandType.Text;

    /// <inheritdoc />
    public override bool DesignTimeVisible { get; set; }

    /// <inheritdoc />
    public override UpdateRowSource UpdatedRowSource { get; set; }

    /// <inheritdoc />
    protected override DbConnection? DbConnection { get; set; }

    /// <inheritdoc />
    protected override DbParameterCollection DbParameterCollection { get; } = new EggDbParameterCollection();

    /// <inheritdoc />
    protected override DbTransaction? DbTransaction { get; set; }

    private EggDbConnection OpenConnection() =>
        DbConnection as EggDbConnection ?? throw new InvalidOperationException("the command has no EggDB connection");

    private List<StatementResult> Run()
    {
        var values = ((EggDbParameterCollection)DbParameterCollection).Values();
        return OpenConnection().Run(_commandText, values);
    }

    /// <inheritdoc />
    public override void Cancel()
    {
    }

    /// <inheritdoc />
    public override int ExecuteNonQuery()
    {
        var results = Run();
        var total = 0;
        foreach (var result in results)
        {
            var words = result.Tag.Split(' ');
            if (words.Length > 1 && words[0] is "INSERT" or "UPDATE" or "DELETE" && int.TryParse(words[^1], out var count))
            {
                total += count;
            }
        }
        return total;
    }

    /// <inheritdoc />
    public override object? ExecuteScalar()
    {
        var results = Run();
        var last = results.Count > 0 ? results[^1] : null;
        return last is { Rows.Count: > 0 } && last.Columns.Length > 0 ? last.Rows[0][0] ?? DBNull.Value : null;
    }

    /// <inheritdoc />
    public override void Prepare()
    {
    }

    /// <inheritdoc />
    protected override DbParameter CreateDbParameter() => new EggDbParameter();

    /// <inheritdoc />
    protected override DbDataReader ExecuteDbDataReader(CommandBehavior behavior) =>
        new EggDbDataReader(Run());
}
