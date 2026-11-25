using System.Data;
using Microsoft.EntityFrameworkCore.Storage;

namespace EggDB.EntityFrameworkCore.Internal;

/// <summary>.NET types and EggDB's PostgreSQL-dialect column types, both ways.</summary>
public class EggDbTypeMappingSource : RelationalTypeMappingSource
{
    private static readonly RelationalTypeMapping Integer = new IntTypeMapping("integer", DbType.Int32);
    private static readonly RelationalTypeMapping BigInt = new LongTypeMapping("bigint", DbType.Int64);
    private static readonly RelationalTypeMapping SmallInt = new ShortTypeMapping("smallint", DbType.Int16);
    private static readonly RelationalTypeMapping Byte = new ByteTypeMapping("smallint", DbType.Byte);
    private static readonly RelationalTypeMapping Boolean = new EggDbBoolTypeMapping();
    private static readonly RelationalTypeMapping Text = new StringTypeMapping("text", DbType.String, unicode: true);
    private static readonly RelationalTypeMapping Numeric = new DecimalTypeMapping("numeric", DbType.Decimal);
    private static readonly RelationalTypeMapping Double = new DoubleTypeMapping("double precision", DbType.Double);
    private static readonly RelationalTypeMapping Real = new FloatTypeMapping("real", DbType.Single);
    private static readonly RelationalTypeMapping Timestamp = new DateTimeTypeMapping("timestamp", DbType.DateTime);
    private static readonly RelationalTypeMapping TimestampWithZone = new DateTimeOffsetTypeMapping("timestamptz", DbType.DateTimeOffset);
    private static readonly RelationalTypeMapping Date = new DateOnlyTypeMapping("date", DbType.Date);
    private static readonly RelationalTypeMapping Time = new TimeOnlyTypeMapping("time", DbType.Time);
    private static readonly RelationalTypeMapping Interval = new TimeSpanTypeMapping("interval", DbType.Object);
    private static readonly RelationalTypeMapping Uuid = new GuidTypeMapping("uuid", DbType.Guid);
    private static readonly RelationalTypeMapping Bytea = new EggDbByteArrayTypeMapping();

    private static readonly Dictionary<Type, RelationalTypeMapping> ByClrType = new()
    {
        [typeof(int)] = Integer,
        [typeof(long)] = BigInt,
        [typeof(short)] = SmallInt,
        [typeof(byte)] = Byte,
        [typeof(bool)] = Boolean,
        [typeof(string)] = Text,
        [typeof(decimal)] = Numeric,
        [typeof(double)] = Double,
        [typeof(float)] = Real,
        [typeof(DateTime)] = Timestamp,
        [typeof(DateTimeOffset)] = TimestampWithZone,
        [typeof(DateOnly)] = Date,
        [typeof(TimeOnly)] = Time,
        [typeof(TimeSpan)] = Interval,
        [typeof(Guid)] = Uuid,
        [typeof(byte[])] = Bytea,
    };

    private static readonly Dictionary<string, RelationalTypeMapping> ByStoreType = new(StringComparer.OrdinalIgnoreCase)
    {
        ["integer"] = Integer,
        ["int"] = Integer,
        ["int4"] = Integer,
        ["bigint"] = BigInt,
        ["int8"] = BigInt,
        ["smallint"] = SmallInt,
        ["int2"] = SmallInt,
        ["boolean"] = Boolean,
        ["bool"] = Boolean,
        ["text"] = Text,
        ["varchar"] = Text,
        ["character varying"] = Text,
        ["numeric"] = Numeric,
        ["decimal"] = Numeric,
        ["double precision"] = Double,
        ["float8"] = Double,
        ["real"] = Real,
        ["float4"] = Real,
        ["timestamp"] = Timestamp,
        ["timestamp without time zone"] = Timestamp,
        ["timestamptz"] = TimestampWithZone,
        ["timestamp with time zone"] = TimestampWithZone,
        ["date"] = Date,
        ["time"] = Time,
        ["interval"] = Interval,
        ["uuid"] = Uuid,
        ["bytea"] = Bytea,
    };

    /// <summary>A mapping source built from EF Core's dependencies.</summary>
    public EggDbTypeMappingSource(TypeMappingSourceDependencies dependencies, RelationalTypeMappingSourceDependencies relationalDependencies)
        : base(dependencies, relationalDependencies)
    {
    }

    /// <inheritdoc />
    protected override RelationalTypeMapping? FindMapping(in RelationalTypeMappingInfo mappingInfo)
    {
        var clrType = mappingInfo.ClrType;
        var storeType = mappingInfo.StoreTypeNameBase ?? mappingInfo.StoreTypeName;
        if (storeType is not null
            && ByStoreType.TryGetValue(storeType, out var byStore)
            && (clrType is null || byStore.ClrType == clrType))
        {
            return storeType.StartsWith("varchar", StringComparison.OrdinalIgnoreCase) && mappingInfo.Size is { } length
                ? new StringTypeMapping($"varchar({length})", DbType.String, unicode: true, size: length)
                : byStore;
        }
        if (clrType is not null && ByClrType.TryGetValue(clrType, out var byClr))
        {
            if (clrType == typeof(string) && mappingInfo.Size is { } size)
            {
                return new StringTypeMapping($"varchar({size})", DbType.String, unicode: true, size: size);
            }
            if (clrType == typeof(decimal) && mappingInfo.Precision is { } precision)
            {
                return new DecimalTypeMapping($"numeric({precision},{mappingInfo.Scale ?? 0})", DbType.Decimal, precision, mappingInfo.Scale ?? 0);
            }
            return byClr;
        }
        return base.FindMapping(mappingInfo);
    }
}

/// <summary><c>boolean</c>, written as <c>TRUE</c> and <c>FALSE</c>.</summary>
public class EggDbBoolTypeMapping : BoolTypeMapping
{
    /// <summary>The <c>boolean</c> mapping.</summary>
    public EggDbBoolTypeMapping()
        : base("boolean", DbType.Boolean)
    {
    }

    /// <summary>A copy with other parameters, as EF Core clones mappings.</summary>
    protected EggDbBoolTypeMapping(RelationalTypeMappingParameters parameters)
        : base(parameters)
    {
    }

    /// <inheritdoc />
    protected override RelationalTypeMapping Clone(RelationalTypeMappingParameters parameters) => new EggDbBoolTypeMapping(parameters);

    /// <inheritdoc />
    protected override string GenerateNonNullSqlLiteral(object value) => (bool)value ? "TRUE" : "FALSE";
}

/// <summary><c>bytea</c>, written as PostgreSQL's <c>'\x..'</c> literals.</summary>
public class EggDbByteArrayTypeMapping : ByteArrayTypeMapping
{
    /// <summary>The <c>bytea</c> mapping.</summary>
    public EggDbByteArrayTypeMapping()
        : base("bytea", DbType.Binary)
    {
    }

    /// <summary>A copy with other parameters, as EF Core clones mappings.</summary>
    protected EggDbByteArrayTypeMapping(RelationalTypeMappingParameters parameters)
        : base(parameters)
    {
    }

    /// <inheritdoc />
    protected override RelationalTypeMapping Clone(RelationalTypeMappingParameters parameters) => new EggDbByteArrayTypeMapping(parameters);

    /// <inheritdoc />
    protected override string GenerateNonNullSqlLiteral(object value) => $"'\\x{Convert.ToHexString((byte[])value)}'::bytea";
}
