using System.Runtime.InteropServices;
using System.Text;

namespace EggDB.Data;

internal sealed class EggDbPreparedStatement : IDisposable
{
    private readonly EggDbConnection _connection;
    private nint _handle;

    internal EggDbPreparedStatement(EggDbConnection connection, nint handle)
    {
        _connection = connection;
        _handle = handle;
        connection.Track(this);
        var status = Native.StatementParameterCount(handle, out var count);
        if (status != 0)
        {
            Dispose();
            throw Native.LastException(status);
        }
        ParameterCount = (int)count;
    }

    internal int ParameterCount { get; }

    internal bool IsFinalized => _handle == 0;

    private nint Live() =>
        _handle != 0 ? _handle : throw new ObjectDisposedException(nameof(EggDbPreparedStatement), "the prepared statement is finalized");

    internal void Bind(IReadOnlyList<object?> values)
    {
        var handle = Live();
        Native.Check(Native.StatementReset(handle));
        Native.Check(Native.ClearBindings(handle));
        for (var index = 0; index < values.Count; index++)
        {
            Native.Check(BindOne(handle, (nuint)(index + 1), values[index]));
        }
    }

    private unsafe int BindOne(nint handle, nuint index, object? value)
    {
        switch (value)
        {
            case null or DBNull:
                return Native.BindNull(handle, index);
            case bool flag:
                return Native.BindBool(handle, index, flag ? 1 : 0);
            case sbyte or byte or short or ushort or int or uint or long:
                return Native.BindInt64(handle, index, Convert.ToInt64(value, System.Globalization.CultureInfo.InvariantCulture));
            case ulong unsigned when unsigned <= long.MaxValue:
                return Native.BindInt64(handle, index, (long)unsigned);
            case float or double:
                return Native.BindDouble(handle, index, Convert.ToDouble(value, System.Globalization.CultureInfo.InvariantCulture));
            case byte[] bytes:
                fixed (byte* first = bytes)
                {
                    return Native.BindBlob(handle, index, first, (nuint)bytes.Length);
                }
            default:
                var text = Encoding.UTF8.GetBytes(_connection.ParameterText(value) ?? string.Empty);
                fixed (byte* first = text)
                {
                    return Native.BindText(handle, index, first, (nuint)text.Length);
                }
        }
    }

    internal List<StatementResult> Execute()
    {
        Native.Check(Native.StatementExecute(Live(), out var results, out var count));
        return EggDbConnection.Gather(results, (int)count);
    }

    internal string[] Columns()
    {
        var handle = Live();
        Native.Check(Native.StatementColumnCount(handle, out var count));
        var names = new string[(int)count];
        for (var column = 0; column < names.Length; column++)
        {
            Native.Check(Native.StatementColumnName(handle, (nuint)column, out var name));
            names[column] = Marshal.PtrToStringUTF8(name) ?? string.Empty;
        }
        return names;
    }

    internal unsafe object?[]? Step()
    {
        var handle = Live();
        Native.Check(Native.StatementStep(handle, out var hasRow));
        if (hasRow == 0)
        {
            return null;
        }
        Native.Check(Native.StatementRow(handle, out var values, out var count));
        var row = new object?[(int)count];
        var first = (NativeValue*)values;
        for (var column = 0; column < row.Length; column++)
        {
            row[column] = EggDbConnection.Converted(first[column]);
        }
        return row;
    }

    internal void Reset()
    {
        if (_handle != 0)
        {
            Native.StatementReset(_handle);
        }
    }

    public void Dispose()
    {
        if (_handle == 0)
        {
            return;
        }
        Native.StatementFinalize(_handle);
        _handle = 0;
        _connection.Untrack(this);
    }
}
