using System.Data;
using System.Data.Common;

namespace EggDB.Data;

/// <summary>A transaction on an <see cref="EggDbConnection"/>.</summary>
public sealed class EggDbTransaction : DbTransaction
{
    private readonly EggDbConnection _connection;
    private bool _finished;

    internal EggDbTransaction(EggDbConnection connection, IsolationLevel isolationLevel)
    {
        _connection = connection;
        IsolationLevel = isolationLevel;
    }

    /// <inheritdoc />
    public override IsolationLevel IsolationLevel { get; }

    /// <inheritdoc />
    protected override DbConnection DbConnection => _connection;

    /// <inheritdoc />
    public override void Commit() => Finish("COMMIT");

    /// <inheritdoc />
    public override void Rollback() => Finish("ROLLBACK");

    private void Finish(string statement)
    {
        if (_finished)
        {
            return;
        }
        _connection.Run(statement, Array.Empty<object?>());
        _finished = true;
        _connection.Transaction = null;
    }

    /// <inheritdoc />
    protected override void Dispose(bool disposing)
    {
        if (disposing && !_finished && _connection.State == ConnectionState.Open)
        {
            Rollback();
        }
        base.Dispose(disposing);
    }
}
