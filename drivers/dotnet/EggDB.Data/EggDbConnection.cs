using System.Data;
using System.Data.Common;
using System.Diagnostics.CodeAnalysis;
using System.Runtime.InteropServices;
using System.Text;

namespace EggDB.Data;

/// <summary>
/// A connection to an embedded EggDB database. The connection string names the file and the
/// SQL dialect: <c>Data Source=app.db;Dialect=postgresql;Mode=ReadOnly</c>; the file
/// <c>:memory:</c> opens a database in memory.
/// </summary>
public sealed class EggDbConnection : DbConnection
{
    private nint _database;
    private nint _session;
    private string _connectionString = string.Empty;
    private ConnectionState _state = ConnectionState.Closed;

    /// <summary>A closed connection with no connection string.</summary>
    public EggDbConnection()
    {
    }

    /// <summary>A closed connection for <paramref name="connectionString"/>.</summary>
    public EggDbConnection(string connectionString)
    {
        _connectionString = connectionString;
    }

    /// <inheritdoc />
    [AllowNull]
    public override string ConnectionString
    {
        get => _connectionString;
        set => _connectionString = value ?? string.Empty;
    }

    /// <inheritdoc />
    public override string Database => DataSource;

    /// <inheritdoc />
    public override string DataSource => Settings().GetValueOrDefault("data source", string.Empty);

    /// <inheritdoc />
    public override string ServerVersion => "embedded";

    /// <inheritdoc />
    public override ConnectionState State => _state;

    internal string Dialect => Settings().GetValueOrDefault("dialect", "sqlite");

    internal EggDbTransaction? Transaction { get; set; }

    private Dictionary<string, string> Settings()
    {
        var builder = new DbConnectionStringBuilder { ConnectionString = _connectionString };
        var settings = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (string key in builder.Keys)
        {
            settings[key.ToLowerInvariant()] = builder[key]?.ToString() ?? string.Empty;
        }
        return settings;
    }

    /// <inheritdoc />
    public override void Open()
    {
        if (_state == ConnectionState.Open)
        {
            return;
        }
        var settings = Settings();
        var options = new NativeOptions
        {
            Flags = settings.GetValueOrDefault("mode", string.Empty).Equals("readonly", StringComparison.OrdinalIgnoreCase)
                ? Native.OpenReadOnly
                : 0,
        };
        var status = Native.Open(DataSource, ref options, out _database);
        if (status != 0)
        {
            throw Native.LastException(status);
        }
        status = Native.SqlOpen(_database, Dialect, out _session);
        if (status != 0)
        {
            var error = Native.LastException(status);
            Native.Close(_database);
            _database = 0;
            throw error;
        }
        _state = ConnectionState.Open;
    }

    /// <inheritdoc />
    public override void Close()
    {
        if (_state != ConnectionState.Open)
        {
            return;
        }
        Native.SqlClose(_session);
        Native.Close(_database);
        _session = 0;
        _database = 0;
        Transaction = null;
        _state = ConnectionState.Closed;
    }

    /// <inheritdoc />
    public override void ChangeDatabase(string databaseName) =>
        throw new NotSupportedException("an embedded EggDB connection holds one database file");

    /// <inheritdoc />
    protected override DbTransaction BeginDbTransaction(IsolationLevel isolationLevel)
    {
        Run("BEGIN", Array.Empty<object?>());
        Transaction = new EggDbTransaction(this, isolationLevel);
        return Transaction;
    }

    /// <inheritdoc />
    protected override DbCommand CreateDbCommand() => new EggDbCommand { Connection = this };

    /// <inheritdoc />
    protected override void Dispose(bool disposing)
    {
        Close();
        base.Dispose(disposing);
    }

    internal unsafe List<StatementResult> Run(string text, IReadOnlyList<object?> parameters)
    {
        if (_state != ConnectionState.Open)
        {
            throw new InvalidOperationException("the connection is not open");
        }
        var handles = new List<GCHandle>();
        var natives = new NativeParameter[Math.Max(parameters.Count, 1)];
        try
        {
            for (var index = 0; index < parameters.Count; index++)
            {
                var textForm = ParameterText(parameters[index]);
                if (textForm is null)
                {
                    continue;
                }
                var bytes = Encoding.UTF8.GetBytes(textForm);
                var pinned = GCHandle.Alloc(bytes, GCHandleType.Pinned);
                handles.Add(pinned);
                natives[index] = new NativeParameter { Text = pinned.AddrOfPinnedObject(), Length = (nuint)bytes.Length };
            }
            fixed (NativeParameter* first = natives)
            {
                var status = Native.Execute(_session, Numbered(text), first, (nuint)parameters.Count, out var results, out var count);
                if (status != 0)
                {
                    throw Native.LastException(status);
                }
                return Gather(results, (int)count);
            }
        }
        finally
        {
            foreach (var handle in handles)
            {
                handle.Free();
            }
        }
    }

    private string Numbered(string text)
    {
        if (Dialect != "postgresql")
        {
            return text;
        }
        var output = new StringBuilder(text.Length + 8);
        char? quote = null;
        var number = 0;
        foreach (var character in text)
        {
            if (quote is not null)
            {
                if (character == quote)
                {
                    quote = null;
                }
                output.Append(character);
            }
            else if (character is '\'' or '"')
            {
                quote = character;
                output.Append(character);
            }
            else if (character == '?')
            {
                number++;
                output.Append('$').Append(number);
            }
            else
            {
                output.Append(character);
            }
        }
        return output.ToString();
    }

    private string? ParameterText(object? value) => value switch
    {
        null or DBNull => null,
        bool flag => Dialect == "postgresql" ? (flag ? "true" : "false") : (flag ? "1" : "0"),
        byte[] bytes => "\\x" + Convert.ToHexString(bytes).ToLowerInvariant(),
        DateTime time => time.ToString("yyyy-MM-dd HH:mm:ss.FFFFFFF", System.Globalization.CultureInfo.InvariantCulture),
        IFormattable formattable => formattable.ToString(null, System.Globalization.CultureInfo.InvariantCulture),
        _ => value.ToString(),
    };

    private static unsafe List<StatementResult> Gather(nint results, int count)
    {
        var gathered = new List<StatementResult>(count);
        var first = (NativeResult*)results;
        for (var index = 0; index < count; index++)
        {
            var result = first[index];
            var columns = (int)result.ColumnCount;
            var names = new string[columns];
            var namePointers = (nint*)result.ColumnNames;
            for (var column = 0; column < columns; column++)
            {
                names[column] = Marshal.PtrToStringUTF8(namePointers[column]) ?? string.Empty;
            }
            var rows = new List<object?[]>((int)result.RowCount);
            var values = (NativeValue*)result.Values;
            for (var row = 0; row < (int)result.RowCount; row++)
            {
                var converted = new object?[columns];
                for (var column = 0; column < columns; column++)
                {
                    converted[column] = Converted(values[row * columns + column]);
                }
                rows.Add(converted);
            }
            gathered.Add(new StatementResult(Marshal.PtrToStringUTF8(result.Tag) ?? string.Empty, names, rows));
        }
        return gathered;
    }

    private static unsafe object? Converted(NativeValue value) => value.Kind switch
    {
        Native.Integer => value.Integer,
        Native.Real => value.Real,
        Native.Blob => new ReadOnlySpan<byte>((void*)value.Data, (int)value.Length).ToArray(),
        Native.Text => Encoding.UTF8.GetString((byte*)value.Data, (int)value.Length),
        _ => null,
    };
}

internal sealed record StatementResult(string Tag, string[] Columns, List<object?[]> Rows);
