using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Infrastructure;

namespace EggDB.EntityFrameworkCore;

/// <summary><c>UseEggDb</c>, which points a context at an EggDB database.</summary>
public static class EggDbDbContextOptionsBuilderExtensions
{
    /// <summary>
    /// Uses EggDB with <paramref name="connectionString"/>: <c>Data Source=app.eggdb</c> for an
    /// embedded file, or <c>Data Source=postgresql://user:password@host/db</c> for a server.
    /// EF Core's SQL is PostgreSQL's, so an embedded connection runs in that dialect.
    /// </summary>
    public static DbContextOptionsBuilder UseEggDb(this DbContextOptionsBuilder optionsBuilder, string connectionString)
    {
        ArgumentNullException.ThrowIfNull(optionsBuilder);
        ArgumentException.ThrowIfNullOrEmpty(connectionString);
        var extension = (optionsBuilder.Options.FindExtension<EggDbOptionsExtension>() ?? new EggDbOptionsExtension())
            .WithConnectionString(connectionString);
        ((IDbContextOptionsBuilderInfrastructure)optionsBuilder).AddOrUpdateExtension((EggDbOptionsExtension)extension);
        return optionsBuilder;
    }

    /// <summary>As <see cref="UseEggDb(DbContextOptionsBuilder, string)"/>, for a typed context.</summary>
    public static DbContextOptionsBuilder<TContext> UseEggDb<TContext>(this DbContextOptionsBuilder<TContext> optionsBuilder, string connectionString)
        where TContext : DbContext =>
        (DbContextOptionsBuilder<TContext>)UseEggDb((DbContextOptionsBuilder)optionsBuilder, connectionString);
}
