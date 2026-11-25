using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.Extensions.DependencyInjection;

namespace EggDB.EntityFrameworkCore;

/// <summary>The options <c>UseEggDb</c> records on a context: the connection string, as every relational provider keeps it.</summary>
public sealed class EggDbOptionsExtension : RelationalOptionsExtension
{
    private DbContextOptionsExtensionInfo? _info;

    /// <summary>Options with no connection string yet.</summary>
    public EggDbOptionsExtension()
    {
    }

    private EggDbOptionsExtension(EggDbOptionsExtension copyFrom)
        : base(copyFrom)
    {
    }

    /// <inheritdoc />
    public override DbContextOptionsExtensionInfo Info => _info ??= new ExtensionInfo(this);

    /// <inheritdoc />
    protected override RelationalOptionsExtension Clone() => new EggDbOptionsExtension(this);

    /// <inheritdoc />
    public override void ApplyServices(IServiceCollection services) => services.AddEntityFrameworkEggDb();

    private sealed class ExtensionInfo : RelationalExtensionInfo
    {
        public ExtensionInfo(IDbContextOptionsExtension extension)
            : base(extension)
        {
        }

        public override bool IsDatabaseProvider => true;

        public override string LogFragment => "using EggDB ";

        public override int GetServiceProviderHashCode() => 0;

        public override bool ShouldUseSameServiceProvider(DbContextOptionsExtensionInfo other) => other is ExtensionInfo;

        public override void PopulateDebugInfo(IDictionary<string, string> debugInfo) =>
            debugInfo["EggDB:" + nameof(EggDbDbContextOptionsBuilderExtensions.UseEggDb)] = "1";
    }
}
