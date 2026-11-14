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

    [LibraryImport(Library, EntryPoint = "eggdb_sql_connect", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int SqlConnect(string url, out nint session);

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

    [LibraryImport(Library, EntryPoint = "eggdb_sql_prepare")]
    internal static unsafe partial int Prepare(nint session, byte* text, nuint length, out nint statement);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_parameter_count")]
    internal static partial int StatementParameterCount(nint statement, out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_bind_null")]
    internal static partial int BindNull(nint statement, nuint index);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_bind_int64")]
    internal static partial int BindInt64(nint statement, nuint index, long value);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_bind_double")]
    internal static partial int BindDouble(nint statement, nuint index, double value);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_bind_bool")]
    internal static partial int BindBool(nint statement, nuint index, int value);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_bind_text")]
    internal static unsafe partial int BindText(nint statement, nuint index, byte* text, nuint length);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_bind_blob")]
    internal static unsafe partial int BindBlob(nint statement, nuint index, byte* data, nuint length);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_clear_bindings")]
    internal static partial int ClearBindings(nint statement);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_execute")]
    internal static partial int StatementExecute(nint statement, out nint results, out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_reset")]
    internal static partial int StatementReset(nint statement);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_column_count")]
    internal static partial int StatementColumnCount(nint statement, out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_column_name")]
    internal static partial int StatementColumnName(nint statement, nuint column, out nint name);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_step")]
    internal static partial int StatementStep(nint statement, out int hasRow);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_row")]
    internal static partial int StatementRow(nint statement, out nint values, out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_stmt_finalize")]
    internal static partial int StatementFinalize(nint statement);

    [LibraryImport(Library, EntryPoint = "eggdb_sql_copy_in_begin", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int CopyInBegin(nint session, string text, out nint copy);

    [LibraryImport(Library, EntryPoint = "eggdb_sql_copy_out_begin", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int CopyOutBegin(nint session, string text, out nint copy);

    [LibraryImport(Library, EntryPoint = "eggdb_copy_put")]
    internal static unsafe partial int CopyPut(nint copy, byte* data, nuint length);

    [LibraryImport(Library, EntryPoint = "eggdb_copy_read")]
    internal static partial int CopyRead(nint copy, out nint data, out nuint length);

    [LibraryImport(Library, EntryPoint = "eggdb_copy_end")]
    internal static partial int CopyEnd(nint copy, out nint results, out nuint count);

    [LibraryImport(Library, EntryPoint = "eggdb_copy_free")]
    internal static partial void CopyFree(nint copy);

    internal static void Check(int status)
    {
        if (status != 0)
        {
            throw LastException(status);
        }
    }

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
