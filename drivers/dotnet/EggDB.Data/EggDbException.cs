using System.Data.Common;

namespace EggDB.Data;

/// <summary>An error EggDB reported, with its SQLSTATE.</summary>
public sealed class EggDbException : DbException
{
    internal EggDbException(string message, string sqlState, int status)
        : base(message, status)
    {
        SqlState = sqlState;
    }

    /// <summary>The five-character SQLSTATE, or empty when EggDB gave none.</summary>
    public override string SqlState { get; }
}
