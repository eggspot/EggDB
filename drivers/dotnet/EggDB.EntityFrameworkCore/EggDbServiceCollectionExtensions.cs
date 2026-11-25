using EggDB.EntityFrameworkCore.Internal;
using Microsoft.EntityFrameworkCore.Diagnostics;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Migrations;
using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore.Update;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;

namespace EggDB.EntityFrameworkCore;

/// <summary>Registers EggDB's EF Core services.</summary>
public static class EggDbServiceCollectionExtensions
{
    /// <summary>Adds the services EF Core needs to talk to EggDB; <c>UseEggDb</c> calls it.</summary>
    public static IServiceCollection AddEntityFrameworkEggDb(this IServiceCollection services)
    {
        var builder = new EntityFrameworkRelationalServicesBuilder(services)
            .TryAdd<LoggingDefinitions, EggDbLoggingDefinitions>()
            .TryAdd<IDatabaseProvider, DatabaseProvider<EggDbOptionsExtension>>()
            .TryAdd<IRelationalTypeMappingSource, EggDbTypeMappingSource>()
            .TryAdd<IRelationalAnnotationProvider, EggDbAnnotationProvider>()
            .TryAdd<IModificationCommandBatchFactory, EggDbModificationCommandBatchFactory>()
            .TryAdd<IRelationalConnection>(provider => provider.GetRequiredService<IEggDbRelationalConnection>())
            .TryAdd<IMigrationsSqlGenerator, EggDbMigrationsSqlGenerator>()
            .TryAdd<IRelationalDatabaseCreator, EggDbDatabaseCreator>()
            .TryAdd<IHistoryRepository, EggDbHistoryRepository>()
            .TryAdd<IQuerySqlGeneratorFactory, EggDbQuerySqlGeneratorFactory>()
            .TryAdd<IUpdateSqlGenerator, EggDbUpdateSqlGenerator>()
            .TryAddProviderSpecificServices(provider =>
                provider.TryAddScoped<IEggDbRelationalConnection, EggDbRelationalConnection>());
        builder.TryAddCoreServices();
        return services;
    }
}
