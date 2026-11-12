using NHibernate.Driver;

namespace EggDB.NHibernate;

/// <summary>
/// NHibernate's driver for EggDB's ADO.NET provider: positional <c>?</c> parameters, as EggDB's
/// commands take them, on <see cref="EggDB.Data.EggDbConnection"/>.
/// </summary>
public sealed class EggDbDriver : ReflectionBasedDriver
{
    /// <summary>The driver, loading the provider from the EggDB.Data assembly.</summary>
    public EggDbDriver()
        : base("EggDB.Data", "EggDB.Data", "EggDB.Data.EggDbConnection", "EggDB.Data.EggDbCommand")
    {
    }

    /// <inheritdoc />
    public override bool UseNamedPrefixInSql => false;

    /// <inheritdoc />
    public override bool UseNamedPrefixInParameter => false;

    /// <inheritdoc />
    public override string NamedPrefix => string.Empty;

    /// <inheritdoc />
    public override bool SupportsMultipleOpenReaders => false;

    /// <inheritdoc />
    public override bool SupportsMultipleQueries => true;
}
