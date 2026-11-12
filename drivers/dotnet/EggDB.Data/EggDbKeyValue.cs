using System.Text;

namespace EggDB.Data;

/// <summary>What a collection holds.</summary>
public readonly record struct EggDbCollectionStats(ulong Entries, ulong KeyBytes, ulong ValueBytes);

/// <summary>The range of a key-value scan: keys from <c>Start</c> up to <c>End</c>, or those beginning with <c>Prefix</c>.</summary>
public sealed record EggDbScanOptions
{
    /// <summary>The first key, or none for the first one there is.</summary>
    public byte[]? Start { get; init; }

    /// <summary>The key the scan stops at (left out unless <c>EndInclusive</c>), or none for the last one.</summary>
    public byte[]? End { get; init; }

    /// <summary>Scan only the keys that begin with these bytes.</summary>
    public byte[]? Prefix { get; init; }

    /// <summary>Scan from the last key to the first.</summary>
    public bool Reverse { get; init; }

    /// <summary>Include <c>End</c> itself.</summary>
    public bool EndInclusive { get; init; }
}

/// <summary>An embedded database file's key-value store: transactions over its default key space and its collections.</summary>
public sealed class EggDbKeyValue : IDisposable
{
    private nint database;

    /// <summary>Opens the key-value store of the database file at <paramref name="path"/>.</summary>
    public EggDbKeyValue(string path, bool readOnly = false)
    {
        var options = new NativeOptions { Flags = readOnly ? Native.OpenReadOnly : 0 };
        var status = Native.Open(path, ref options, out database);
        if (status != 0)
        {
            throw Native.LastException(status);
        }
    }

    /// <summary>Begins a read transaction over one snapshot.</summary>
    public EggDbKeyTransaction BeginRead() => Begin(false);

    /// <summary>Begins a write transaction; disposing it without <c>Commit</c> rolls it back.</summary>
    public EggDbKeyTransaction BeginWrite() => Begin(true);

    private EggDbKeyTransaction Begin(bool writing)
    {
        ObjectDisposedException.ThrowIf(database == 0, this);
        var status = writing ? NativeKeyValue.BeginWrite(database, out var handle) : NativeKeyValue.BeginRead(database, out handle);
        if (status != 0)
        {
            throw Native.LastException(status);
        }
        return new EggDbKeyTransaction(handle, writing);
    }

    /// <summary>Closes the file.</summary>
    public void Dispose()
    {
        if (database != 0)
        {
            Native.Close(database);
            database = 0;
        }
    }
}

/// <summary>One key-value transaction. A collection of <c>null</c> is the default key space.</summary>
public sealed unsafe class EggDbKeyTransaction : IDisposable
{
    private nint handle;

    internal EggDbKeyTransaction(nint handle, bool writing)
    {
        this.handle = handle;
        Writing = writing;
    }

    /// <summary>Whether the transaction writes.</summary>
    public bool Writing { get; }

    private nint Live
    {
        get
        {
            ObjectDisposedException.ThrowIf(handle == 0, this);
            return handle;
        }
    }

    private static void Check(int status)
    {
        if (status != 0)
        {
            throw Native.LastException(status);
        }
    }

    private static byte[] Copied(nint data, nuint length)
    {
        var bytes = new byte[(int)length];
        if (length > 0)
        {
            new ReadOnlySpan<byte>((void*)data, (int)length).CopyTo(bytes);
        }
        return bytes;
    }

    /// <summary>The value of <paramref name="key"/>, or null when it is absent or has expired.</summary>
    public byte[]? Get(ReadOnlySpan<byte> key, string? collection = null)
    {
        nint value;
        nuint length;
        fixed (byte* keyBytes = key)
        {
            Check(NativeKeyValue.Get(Live, collection, keyBytes, (nuint)key.Length, out value, out length));
        }
        if (value == 0)
        {
            return null;
        }
        try
        {
            return Copied(value, length);
        }
        finally
        {
            NativeKeyValue.Free(value, length);
        }
    }

    /// <summary>The value of the UTF-8 key <paramref name="key"/>, or null.</summary>
    public byte[]? Get(string key, string? collection = null) => Get(Encoding.UTF8.GetBytes(key), collection);

    /// <summary>Stores <paramref name="value"/> under <paramref name="key"/>, with a time to live when <paramref name="ttl"/> is given.</summary>
    public void Put(ReadOnlySpan<byte> key, ReadOnlySpan<byte> value, string? collection = null, TimeSpan? ttl = null)
    {
        fixed (byte* keyBytes = key)
        fixed (byte* valueBytes = value)
        {
            Check(ttl is { } lifetime
                ? NativeKeyValue.PutWithExpiry(Live, collection, keyBytes, (nuint)key.Length, valueBytes, (nuint)value.Length, (ulong)Math.Max(0, (long)lifetime.TotalMilliseconds))
                : NativeKeyValue.Put(Live, collection, keyBytes, (nuint)key.Length, valueBytes, (nuint)value.Length));
        }
    }

    /// <summary>Stores the UTF-8 <paramref name="value"/> under the UTF-8 <paramref name="key"/>.</summary>
    public void Put(string key, string value, string? collection = null, TimeSpan? ttl = null) =>
        Put(Encoding.UTF8.GetBytes(key), Encoding.UTF8.GetBytes(value), collection, ttl);

    /// <summary>Removes <paramref name="key"/>; true when it was there.</summary>
    public bool Delete(ReadOnlySpan<byte> key, string? collection = null)
    {
        int deleted;
        fixed (byte* keyBytes = key)
        {
            Check(NativeKeyValue.Delete(Live, collection, keyBytes, (nuint)key.Length, out deleted));
        }
        return deleted != 0;
    }

    /// <summary>The entries of a range, in key order (the other way with <c>Reverse</c>).</summary>
    public IReadOnlyList<KeyValuePair<byte[], byte[]>> Scan(EggDbScanOptions? options = null, string? collection = null)
    {
        options ??= new EggDbScanOptions();
        var flags = (options.Reverse ? NativeKeyValue.ScanReverse : 0) | (options.EndInclusive ? NativeKeyValue.ScanEndInclusive : 0);
        var start = options.Start;
        var end = options.End;
        if (options.Prefix is { } prefix)
        {
            flags |= NativeKeyValue.ScanPrefix;
            start = prefix;
            end = null;
        }
        nint scan;
        fixed (byte* startBytes = start)
        fixed (byte* endBytes = end)
        {
            Check(NativeKeyValue.ScanOpen(Live, collection, startBytes, (nuint)(start?.Length ?? 0), endBytes, (nuint)(end?.Length ?? 0), flags, out scan));
        }
        var found = new List<KeyValuePair<byte[], byte[]>>();
        try
        {
            while (true)
            {
                Check(NativeKeyValue.ScanNext(Live, scan, 0, out var entries, out var count));
                if (count == 0)
                {
                    return found;
                }
                for (nuint index = 0; index < count; index++)
                {
                    var entry = entries[index];
                    found.Add(new KeyValuePair<byte[], byte[]>(Copied(entry.Key, entry.KeyLength), Copied(entry.Value, entry.ValueLength)));
                }
            }
        }
        finally
        {
            NativeKeyValue.ScanFree(scan);
        }
    }

    /// <summary>Creates the collection <paramref name="name"/>; true when it was made.</summary>
    public bool CreateCollection(string name)
    {
        Check(NativeKeyValue.CreateCollection(Live, name, out var created));
        return created != 0;
    }

    /// <summary>Drops the collection <paramref name="name"/> with its keys; true when it was there.</summary>
    public bool DropCollection(string name)
    {
        Check(NativeKeyValue.DropCollection(Live, name, out var dropped));
        return dropped != 0;
    }

    /// <summary>Renames the collection <paramref name="name"/> to <paramref name="newName"/>, keeping its keys.</summary>
    public void RenameCollection(string name, string newName) => Check(NativeKeyValue.RenameCollection(Live, name, newName));

    /// <summary>The collections' names, in name order.</summary>
    public IReadOnlyList<string> Collections()
    {
        Check(NativeKeyValue.CollectionNames(Live, out var names, out var length, out var count));
        if (names == 0)
        {
            return Array.Empty<string>();
        }
        try
        {
            var joined = Copied(names, length);
            return Encoding.UTF8.GetString(joined).Split('\0').Take((int)count).ToArray();
        }
        finally
        {
            NativeKeyValue.Free(names, length);
        }
    }

    /// <summary>What the collection <paramref name="name"/> holds; needs a read transaction.</summary>
    public EggDbCollectionStats CollectionStats(string name)
    {
        Check(NativeKeyValue.CollectionStats(Live, name, out var stats));
        return new EggDbCollectionStats(stats.Entries, stats.KeyBytes, stats.ValueBytes);
    }

    /// <summary>Ends the transaction, committing its writes; returns the commit's stamp.</summary>
    public ulong Commit()
    {
        var ending = Live;
        handle = 0;
        Check(NativeKeyValue.Commit(ending, out var stamp));
        return stamp;
    }

    /// <summary>Ends the transaction without committing; after <c>Commit</c> it does nothing.</summary>
    public void Dispose()
    {
        if (handle != 0)
        {
            NativeKeyValue.Rollback(handle);
            handle = 0;
        }
    }
}
