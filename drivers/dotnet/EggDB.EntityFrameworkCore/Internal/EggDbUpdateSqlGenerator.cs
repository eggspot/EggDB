using Microsoft.EntityFrameworkCore.Update;

namespace EggDB.EntityFrameworkCore.Internal;

/// <summary>Inserts, updates and deletes, reading generated values back with <c>RETURNING</c>.</summary>
public class EggDbUpdateSqlGenerator : UpdateSqlGenerator
{
    /// <summary>A generator built from EF Core's dependencies.</summary>
    public EggDbUpdateSqlGenerator(UpdateSqlGeneratorDependencies dependencies)
        : base(dependencies)
    {
    }
}

/// <summary>Sends one change per command, each reading back what the database generated.</summary>
public class EggDbModificationCommandBatchFactory : IModificationCommandBatchFactory
{
    private readonly ModificationCommandBatchFactoryDependencies _dependencies;

    /// <summary>A factory built from EF Core's dependencies.</summary>
    public EggDbModificationCommandBatchFactory(ModificationCommandBatchFactoryDependencies dependencies) => _dependencies = dependencies;

    /// <inheritdoc />
    public virtual ModificationCommandBatch Create() => new SingularModificationCommandBatch(_dependencies);
}
