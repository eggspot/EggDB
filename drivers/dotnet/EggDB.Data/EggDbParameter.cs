using System.Collections;
using System.Data;
using System.Data.Common;
using System.Diagnostics.CodeAnalysis;

namespace EggDB.Data;

/// <summary>A value for one <c>?</c> placeholder, in order.</summary>
public sealed class EggDbParameter : DbParameter
{
    private string _name = string.Empty;
    private string _sourceColumn = string.Empty;

    /// <inheritdoc />
    public override DbType DbType { get; set; } = DbType.String;

    /// <inheritdoc />
    public override ParameterDirection Direction { get; set; } = ParameterDirection.Input;

    /// <inheritdoc />
    public override bool IsNullable { get; set; } = true;

    /// <inheritdoc />
    [AllowNull]
    public override string ParameterName
    {
        get => _name;
        set => _name = value ?? string.Empty;
    }

    /// <inheritdoc />
    public override int Size { get; set; }

    /// <inheritdoc />
    [AllowNull]
    public override string SourceColumn
    {
        get => _sourceColumn;
        set => _sourceColumn = value ?? string.Empty;
    }

    /// <inheritdoc />
    public override bool SourceColumnNullMapping { get; set; }

    /// <inheritdoc />
    public override object? Value { get; set; }

    /// <inheritdoc />
    public override void ResetDbType() => DbType = DbType.String;
}

/// <summary>The parameters of an <see cref="EggDbCommand"/>, bound to its placeholders in order.</summary>
public sealed class EggDbParameterCollection : DbParameterCollection
{
    private readonly List<EggDbParameter> _parameters = new();

    internal IReadOnlyList<object?> Values() => _parameters.Select(parameter => parameter.Value).ToList();

    /// <inheritdoc />
    public override int Count => _parameters.Count;

    /// <inheritdoc />
    public override object SyncRoot => ((ICollection)_parameters).SyncRoot;

    /// <inheritdoc />
    public override int Add(object value)
    {
        _parameters.Add((EggDbParameter)value);
        return _parameters.Count - 1;
    }

    /// <inheritdoc />
    public override void AddRange(Array values)
    {
        foreach (var value in values)
        {
            Add(value!);
        }
    }

    /// <inheritdoc />
    public override void Clear() => _parameters.Clear();

    /// <inheritdoc />
    public override bool Contains(object value) => _parameters.Contains((EggDbParameter)value);

    /// <inheritdoc />
    public override bool Contains(string value) => IndexOf(value) >= 0;

    /// <inheritdoc />
    public override void CopyTo(Array array, int index) => ((ICollection)_parameters).CopyTo(array, index);

    /// <inheritdoc />
    public override IEnumerator GetEnumerator() => _parameters.GetEnumerator();

    /// <inheritdoc />
    public override int IndexOf(object value) => _parameters.IndexOf((EggDbParameter)value);

    /// <inheritdoc />
    public override int IndexOf(string parameterName) =>
        _parameters.FindIndex(parameter => parameter.ParameterName == parameterName);

    /// <inheritdoc />
    public override void Insert(int index, object value) => _parameters.Insert(index, (EggDbParameter)value);

    /// <inheritdoc />
    public override void Remove(object value) => _parameters.Remove((EggDbParameter)value);

    /// <inheritdoc />
    public override void RemoveAt(int index) => _parameters.RemoveAt(index);

    /// <inheritdoc />
    public override void RemoveAt(string parameterName) => RemoveAt(IndexOf(parameterName));

    /// <inheritdoc />
    protected override DbParameter GetParameter(int index) => _parameters[index];

    /// <inheritdoc />
    protected override DbParameter GetParameter(string parameterName) => _parameters[IndexOf(parameterName)];

    /// <inheritdoc />
    protected override void SetParameter(int index, DbParameter value) => _parameters[index] = (EggDbParameter)value;

    /// <inheritdoc />
    protected override void SetParameter(string parameterName, DbParameter value) =>
        _parameters[IndexOf(parameterName)] = (EggDbParameter)value;
}
