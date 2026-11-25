using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Query.SqlExpressions;

namespace EggDB.EntityFrameworkCore.Internal;

/// <summary>Queries in EggDB's PostgreSQL dialect, with <c>LIMIT</c> and <c>OFFSET</c>.</summary>
public class EggDbQuerySqlGenerator : QuerySqlGenerator
{
    /// <summary>A generator built from EF Core's dependencies.</summary>
    public EggDbQuerySqlGenerator(QuerySqlGeneratorDependencies dependencies)
        : base(dependencies)
    {
    }

    /// <inheritdoc />
    protected override void GenerateLimitOffset(SelectExpression selectExpression)
    {
        if (selectExpression.Limit is not null)
        {
            Sql.AppendLine().Append("LIMIT ");
            Visit(selectExpression.Limit);
        }
        if (selectExpression.Offset is not null)
        {
            if (selectExpression.Limit is null)
            {
                Sql.AppendLine();
            }
            else
            {
                Sql.Append(" ");
            }
            Sql.Append("OFFSET ");
            Visit(selectExpression.Offset);
        }
    }
}

/// <summary>Makes <see cref="EggDbQuerySqlGenerator"/>s.</summary>
public class EggDbQuerySqlGeneratorFactory : IQuerySqlGeneratorFactory
{
    private readonly QuerySqlGeneratorDependencies _dependencies;

    /// <summary>A factory built from EF Core's dependencies.</summary>
    public EggDbQuerySqlGeneratorFactory(QuerySqlGeneratorDependencies dependencies) => _dependencies = dependencies;

    /// <inheritdoc />
    public virtual QuerySqlGenerator Create() => new EggDbQuerySqlGenerator(_dependencies);
}
