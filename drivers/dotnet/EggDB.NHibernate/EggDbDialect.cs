using NHibernate.Dialect;

namespace EggDB.NHibernate;

/// <summary>
/// NHibernate's dialect for EggDB: NHibernate's SQLite dialect, the SQL EggDB's ADO.NET provider
/// speaks unless its connection string asks for another dialect.
/// </summary>
public sealed class EggDbDialect : SQLiteDialect
{
}
