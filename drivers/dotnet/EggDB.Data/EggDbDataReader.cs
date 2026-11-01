using System.Collections;
using System.Data.Common;

namespace EggDB.Data;

/// <summary>The rows of an <see cref="EggDbCommand"/>, read forward.</summary>
public sealed class EggDbDataReader : DbDataReader
{
    private readonly List<StatementResult> _results;
    private int _result;
    private int _row = -1;

    internal EggDbDataReader(List<StatementResult> results)
    {
        _results = results.Where(result => result.Columns.Length > 0).ToList();
    }

    private StatementResult? Current => _result < _results.Count ? _results[_result] : null;

    private object? ValueAt(int ordinal) => Current?.Rows[_row][ordinal];

    /// <inheritdoc />
    public override object this[int ordinal] => GetValue(ordinal);

    /// <inheritdoc />
    public override object this[string name] => GetValue(GetOrdinal(name));

    /// <inheritdoc />
    public override int Depth => 0;

    /// <inheritdoc />
    public override int FieldCount => Current?.Columns.Length ?? 0;

    /// <inheritdoc />
    public override bool HasRows => Current is { Rows.Count: > 0 };

    /// <inheritdoc />
    public override bool IsClosed => false;

    /// <inheritdoc />
    public override int RecordsAffected => -1;

    /// <inheritdoc />
    public override bool GetBoolean(int ordinal) => Convert.ToBoolean(ValueAt(ordinal));

    /// <inheritdoc />
    public override byte GetByte(int ordinal) => Convert.ToByte(ValueAt(ordinal));

    /// <inheritdoc />
    public override long GetBytes(int ordinal, long dataOffset, byte[]? buffer, int bufferOffset, int length)
    {
        var bytes = (byte[])ValueAt(ordinal)!;
        if (buffer is null)
        {
            return bytes.Length;
        }
        var copied = (int)Math.Min(length, bytes.Length - dataOffset);
        Array.Copy(bytes, dataOffset, buffer, bufferOffset, copied);
        return copied;
    }

    /// <inheritdoc />
    public override char GetChar(int ordinal) => GetString(ordinal)[0];

    /// <inheritdoc />
    public override long GetChars(int ordinal, long dataOffset, char[]? buffer, int bufferOffset, int length)
    {
        var text = GetString(ordinal);
        if (buffer is null)
        {
            return text.Length;
        }
        var copied = (int)Math.Min(length, text.Length - dataOffset);
        text.CopyTo((int)dataOffset, buffer, bufferOffset, copied);
        return copied;
    }

    /// <inheritdoc />
    public override string GetDataTypeName(int ordinal) => GetFieldType(ordinal).Name;

    /// <inheritdoc />
    public override DateTime GetDateTime(int ordinal) =>
        DateTime.Parse(GetString(ordinal), System.Globalization.CultureInfo.InvariantCulture);

    /// <inheritdoc />
    public override decimal GetDecimal(int ordinal) =>
        Convert.ToDecimal(ValueAt(ordinal), System.Globalization.CultureInfo.InvariantCulture);

    /// <inheritdoc />
    public override double GetDouble(int ordinal) =>
        Convert.ToDouble(ValueAt(ordinal), System.Globalization.CultureInfo.InvariantCulture);

    /// <inheritdoc />
    public override IEnumerator GetEnumerator() => new DbEnumerator(this);

    /// <inheritdoc />
    public override Type GetFieldType(int ordinal)
    {
        foreach (var row in Current?.Rows ?? new List<object?[]>())
        {
            if (row[ordinal] is { } value)
            {
                return value.GetType();
            }
        }
        return typeof(object);
    }

    /// <inheritdoc />
    public override float GetFloat(int ordinal) => (float)GetDouble(ordinal);

    /// <inheritdoc />
    public override Guid GetGuid(int ordinal) => Guid.Parse(GetString(ordinal));

    /// <inheritdoc />
    public override short GetInt16(int ordinal) => Convert.ToInt16(ValueAt(ordinal));

    /// <inheritdoc />
    public override int GetInt32(int ordinal) => Convert.ToInt32(ValueAt(ordinal));

    /// <inheritdoc />
    public override long GetInt64(int ordinal) => Convert.ToInt64(ValueAt(ordinal));

    /// <inheritdoc />
    public override string GetName(int ordinal) => Current!.Columns[ordinal];

    /// <inheritdoc />
    public override int GetOrdinal(string name) =>
        Array.FindIndex(Current!.Columns, column => string.Equals(column, name, StringComparison.OrdinalIgnoreCase));

    /// <inheritdoc />
    public override string GetString(int ordinal) =>
        Convert.ToString(ValueAt(ordinal), System.Globalization.CultureInfo.InvariantCulture) ?? string.Empty;

    /// <inheritdoc />
    public override object GetValue(int ordinal) => ValueAt(ordinal) ?? DBNull.Value;

    /// <inheritdoc />
    public override int GetValues(object[] values)
    {
        var count = Math.Min(values.Length, FieldCount);
        for (var ordinal = 0; ordinal < count; ordinal++)
        {
            values[ordinal] = GetValue(ordinal);
        }
        return count;
    }

    /// <inheritdoc />
    public override bool IsDBNull(int ordinal) => ValueAt(ordinal) is null;

    /// <inheritdoc />
    public override bool NextResult()
    {
        _result++;
        _row = -1;
        return Current is not null;
    }

    /// <inheritdoc />
    public override bool Read()
    {
        if (Current is null || _row + 1 >= Current.Rows.Count)
        {
            return false;
        }
        _row++;
        return true;
    }
}
