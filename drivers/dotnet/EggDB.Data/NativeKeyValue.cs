using System.Runtime.InteropServices;

namespace EggDB.Data;

[StructLayout(LayoutKind.Sequential)]
internal struct NativeEntry
{
    public nint Key;
    public nuint KeyLength;
    public nint Value;
    public nuint ValueLength;
}

[StructLayout(LayoutKind.Sequential)]
internal struct NativeCollectionStats
{
    public ulong Entries;
    public ulong KeyBytes;
    public ulong ValueBytes;
}

internal static unsafe partial class NativeKeyValue
{
    private const string Library = "eggdb_capi";

    internal const uint ScanReverse = 1;
    internal const uint ScanEndInclusive = 2;
    internal const uint ScanPrefix = 4;

    [LibraryImport(Library, EntryPoint = "eggdb_free")]
    internal static partial void Free(nint data, nuint length);

    [LibraryImport(Library, EntryPoint = "eggdb_begin_read")]
    internal static partial int BeginRead(nint database, out nint transaction);

    [LibraryImport(Library, EntryPoint = "eggdb_begin_write")]
    internal static partial int BeginWrite(nint database, out nint transaction);

    [LibraryImport(Library, EntryPoint = "eggdb_commit")]
    internal static partial int Commit(nint transaction, out ulong stamp);

    [LibraryImport(Library, EntryPoint = "eggdb_rollback")]
    internal static partial int Rollback(nint transaction);

    [LibraryImport(Library, EntryPoint = "eggdb_get", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int Get(nint transaction, string? collection, byte* key, nuint keyLength, out nint value, out nuint valueLength);

    [LibraryImport(Library, EntryPoint = "eggdb_put", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int Put(nint transaction, string? collection, byte* key, nuint keyLength, byte* value, nuint valueLength);

    [LibraryImport(Library, EntryPoint = "eggdb_put_with_expiry", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int PutWithExpiry(nint transaction, string? collection, byte* key, nuint keyLength, byte* value, nuint valueLength, ulong ttlMilliseconds);

    [LibraryImport(Library, EntryPoint = "eggdb_delete", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int Delete(nint transaction, string? collection, byte* key, nuint keyLength, out int deleted);

    [LibraryImport(Library, EntryPoint = "eggdb_scan_open", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int ScanOpen(nint transaction, string? collection, byte* start, nuint startLength, byte* end, nuint endLength, uint flags, out nint scan);

    [LibraryImport(Library, EntryPoint = "eggdb_scan_next")]
    internal static partial int ScanNext(nint transaction, nint scan, nuint maxEntries, out NativeEntry* entries, out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_scan_free")]
    internal static partial void ScanFree(nint scan);

    [LibraryImport(Library, EntryPoint = "eggdb_create_collection", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int CreateCollection(nint transaction, string name, out int created);

    [LibraryImport(Library, EntryPoint = "eggdb_drop_collection", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int DropCollection(nint transaction, string name, out int dropped);

    [LibraryImport(Library, EntryPoint = "eggdb_rename_collection", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int RenameCollection(nint transaction, string name, string newName);

    [LibraryImport(Library, EntryPoint = "eggdb_collection_names")]
    internal static partial int CollectionNames(nint transaction, out nint names, out nuint namesLength, out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_collection_stats", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int CollectionStats(nint transaction, string name, out NativeCollectionStats stats);
}
