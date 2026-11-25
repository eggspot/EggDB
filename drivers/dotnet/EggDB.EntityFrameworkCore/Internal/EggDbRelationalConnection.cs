using System.Data.Common;
using EggDB.Data;
using Microsoft.EntityFrameworkCore.Storage;

namespace EggDB.EntityFrameworkCore.Internal;

/// <summary>EF Core's connection to EggDB.</summary>
public interface IEggDbRelationalConnection : IRelationalConnection
{
}

/// <summary>Opens <see cref="EggDbConnection"/>s, in PostgreSQL's dialect unless the connection string names another.</summary>
public class EggDbRelationalConnection : RelationalConnection, IEggDbRelationalConnection
{
    /// <summary>A connection built from EF Core's dependencies.</summary>
    public EggDbRelationalConnection(RelationalConnectionDependencies dependencies)
        : base(dependencies)
    {
    }

    /// <inheritdoc />
    protected override DbConnection CreateDbConnection() => new EggDbConnection(WithDialect(GetValidatedConnectionString()));

    internal static string WithDialect(string connectionString)
    {
        var builder = new DbConnectionStringBuilder { ConnectionString = connectionString };
        if (!builder.ContainsKey("Dialect"))
        {
            builder["Dialect"] = "postgresql";
        }
        return builder.ConnectionString;
    }
}
