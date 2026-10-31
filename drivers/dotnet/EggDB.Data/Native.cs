using System.Runtime.InteropServices;

namespace EggDB.Data;

[StructLayout(LayoutKind.Sequential)]
internal struct NativeOptions
{
    public uint Flags;
    public uint Durability;
    public ulong CacheBytes;
}

[StructLayout(LayoutKind.Sequential)]
internal struct NativeValue
{
    public int Kind;
    public long Integer;
    public double Real;
    public nint Data;
    public nuint Length;
}

[StructLayout(LayoutKind.Sequential)]
internal struct NativeParameter
{
    public nint Text;
    public nuint Length;
}

[StructLayout(LayoutKind.Sequential)]
internal struct NativeResult
{
    public nint Tag;
    public nuint ColumnCount;
    public nint ColumnNames;
    public nuint RowCount;
    public nint Values;
}

internal static partial class Native
{
    private const string Library = "eggdb_capi";

    internal const int Null = 0;
    internal const int Integer = 1;
    internal const int Real = 2;
    internal const int Text = 3;
    internal const int Blob = 4;
    internal const uint OpenReadOnly = 1;

    [LibraryImport(Library, EntryPoint = "eggdb_open", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int Open(string path, ref NativeOptions options, out nint database);

    [LibraryImport(Library, EntryPoint = "eggdb_close")]
    internal static partial int Close(nint database);

    [LibraryImport(Library, EntryPoint = "eggdb_sql_open", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int SqlOpen(nint database, string dialect, out nint session);

    [LibraryImport(Library, EntryPoint = "eggdb_sql_close")]
    internal static partial int SqlClose(nint session);

    [LibraryImport(Library, EntryPoint = "eggdb_sql_execute_with_parameters", StringMarshalling = StringMarshalling.Utf8)]
    internal static unsafe partial int Execute(
        nint session,
        string text,
        NativeParameter* parameters,
        nuint parameterCount,
        out nint results,
        out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_last_error")]
    internal static partial nint LastError();

    [LibraryImport(Library, EntryPoint = "eggdb_last_sqlstate")]
    internal static partial nint LastSqlState();

    internal static EggDbException LastException(int status)
    {
        var message = Marshal.PtrToStringUTF8(LastError()) ?? $"EggDB status {status}";
        var sqlState = Marshal.PtrToStringUTF8(LastSqlState()) ?? string.Empty;
        return new EggDbException(message, sqlState, status);
    }
}
