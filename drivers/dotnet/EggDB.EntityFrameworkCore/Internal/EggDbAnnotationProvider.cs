using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata;

namespace EggDB.EntityFrameworkCore.Internal;

/// <summary>Marks the integer key columns EF Core generates values for, so migrations make them identity columns.</summary>
public class EggDbAnnotationProvider : RelationalAnnotationProvider
{
    /// <summary>The annotation an identity column carries into migrations.</summary>
    public const string Identity = "EggDB:Identity";

    /// <summary>An annotation provider built from EF Core's dependencies.</summary>
    public EggDbAnnotationProvider(RelationalAnnotationProviderDependencies dependencies)
        : base(dependencies)
    {
    }

    /// <inheritdoc />
    public override IEnumerable<IAnnotation> For(IColumn column, bool designTime)
    {
        foreach (var annotation in base.For(column, designTime))
        {
            yield return annotation;
        }
        var property = column.PropertyMappings.FirstOrDefault()?.Property;
        if (property is null)
        {
            yield break;
        }
        var clrType = Nullable.GetUnderlyingType(property.ClrType) ?? property.ClrType;
        var integer = clrType == typeof(int) || clrType == typeof(long) || clrType == typeof(short);
        if (integer
            && property.ValueGenerated == ValueGenerated.OnAdd
            && property.IsPrimaryKey()
            && property.FindTypeMapping()?.Converter is null
            && property.GetDefaultValueSql() is null
            && property.GetComputedColumnSql() is null
            && property.GetDefaultValue() is null)
        {
            yield return new Annotation(Identity, true);
        }
    }
}
